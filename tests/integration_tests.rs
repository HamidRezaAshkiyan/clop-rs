use clop_rs::clipboard::{compute_bytes_hash, compute_image_hash, ClipboardMonitor};
use clop_rs::config::{AppConfig, OptimizationMode};
use clop_rs::downloads::DownloadsWatcher;
use clop_rs::optimizer::{
    encode_rgba_png, format_bytes, has_png_metadata, optimize_file, optimize_png_bytes,
    optimize_rgba, strip_jpeg_exif,
};
use image::ImageFormat;
use std::thread;
use std::time::Duration;
use tempfile::tempdir;

/// Generates a synthetic screenshot-like RGBA image with UI blocks, gradients, and alpha.
fn generate_synthetic_screenshot(width: u32, height: u32) -> Vec<u8> {
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            let in_card =
                x > width / 6 && x < (width * 5) / 6 && y > height / 6 && y < (height * 5) / 6;
            let (r, g, b, a) = if in_card {
                (245u8, 247u8, 250u8, 255u8)
            } else {
                let r = ((x * 64) / width.max(1)) as u8 + 30;
                let g = ((y * 64) / height.max(1)) as u8 + 60;
                let b = (((x + y) * 32) / (width + height).max(1)) as u8 + 120;
                (r, g, b, 255u8)
            };
            rgba.extend_from_slice(&[r, g, b, a]);
        }
    }
    rgba
}

#[test]
fn test_smart_mode_achieves_significant_compression() {
    let width = 400;
    let height = 300;
    let rgba = generate_synthetic_screenshot(width, height);
    let unoptimized_png = encode_rgba_png(width, height, &rgba).expect("encode baseline PNG");

    let cfg = AppConfig {
        mode: OptimizationMode::Smart,
        ..Default::default()
    };

    let res = optimize_rgba(width, height, &rgba, Some(&unoptimized_png), &cfg)
        .expect("optimize_rgba smart mode");

    assert!(
        res.was_modified,
        "Expected image to be modified and reduced"
    );
    assert!(
        res.optimized_size < res.original_size,
        "Expected optimized_size ({}) < original_size ({})",
        res.optimized_size,
        res.original_size
    );
    assert!(
        res.saved_percent() > 40.0,
        "Expected >40% reduction on screenshot, got {:.1}% ({} -> {})",
        res.saved_percent(),
        res.original_size,
        res.optimized_size
    );

    let decoded = image::load_from_memory_with_format(&res.png_bytes, ImageFormat::Png)
        .expect("decode optimized PNG");
    assert_eq!(decoded.width(), width);
    assert_eq!(decoded.height(), height);
}

#[test]
fn test_lossless_mode_preserves_exact_pixels() {
    let width = 200;
    let height = 150;
    let rgba = generate_synthetic_screenshot(width, height);
    let unoptimized_png = encode_rgba_png(width, height, &rgba).expect("encode baseline PNG");

    let cfg = AppConfig {
        mode: OptimizationMode::Lossless,
        ..Default::default()
    };

    let res = optimize_rgba(width, height, &rgba, Some(&unoptimized_png), &cfg)
        .expect("optimize_rgba lossless mode");

    assert!(res.optimized_size <= res.original_size);

    let decoded = image::load_from_memory_with_format(&res.png_bytes, ImageFormat::Png)
        .expect("decode lossless PNG")
        .into_rgba8();
    assert_eq!(decoded.width(), width);
    assert_eq!(decoded.height(), height);
    assert_eq!(
        decoded.as_raw().as_slice(),
        rgba.as_slice(),
        "Lossless mode must preserve 100% bit-exact RGBA pixel values"
    );
}

#[test]
fn test_downscaling_max_dimensions_and_scale_factor() {
    let width = 800;
    let height = 600;
    let rgba = generate_synthetic_screenshot(width, height);

    let cfg = AppConfig {
        max_width: Some(400),
        ..Default::default()
    };

    let res = optimize_rgba(width, height, &rgba, None, &cfg).expect("downscale via max_width");
    assert_eq!(res.width, 400);
    assert_eq!(res.height, 300);
    assert!(res.was_modified);

    let cfg_scale = AppConfig {
        scale_factor: Some(2.0),
        ..Default::default()
    };
    let res_scale =
        optimize_rgba(width, height, &rgba, None, &cfg_scale).expect("downscale via scale_factor");
    assert_eq!(res_scale.width, 400);
    assert_eq!(res_scale.height, 300);
}

#[test]
fn test_size_guard_never_increases_size() {
    let width = 64;
    let height = 64;
    let rgba = generate_synthetic_screenshot(width, height);
    let cfg = AppConfig::default();

    let first = optimize_rgba(width, height, &rgba, None, &cfg).expect("first pass");
    let second = optimize_png_bytes(&first.png_bytes, &cfg).expect("second pass");
    assert!(
        second.optimized_size <= first.optimized_size,
        "Second optimization pass increased size from {} to {}",
        first.optimized_size,
        second.optimized_size
    );
}

#[test]
fn test_file_optimization_on_disk() {
    let dir = tempdir().expect("create tempdir");
    let input_path = dir.path().join("sample.png");
    let output_path = dir.path().join("sample_out.png");

    let rgba = generate_synthetic_screenshot(320, 240);
    let raw_png = encode_rgba_png(320, 240, &rgba).expect("encode PNG");
    std::fs::write(&input_path, &raw_png).expect("write input file");

    let cfg = AppConfig::default();
    let res = optimize_file(&input_path, Some(&output_path), &cfg).expect("optimize_file");

    assert!(res.was_modified);
    assert!(output_path.exists());
    let disk_size = std::fs::metadata(&output_path).unwrap().len() as usize;
    assert_eq!(disk_size, res.optimized_size);
    assert!(disk_size < raw_png.len());
}

#[test]
fn test_exif_and_metadata_stripping() {
    let width = 120;
    let height = 100;
    let rgba = generate_synthetic_screenshot(width, height);
    let base_png = encode_rgba_png(width, height, &rgba).expect("encode PNG");

    // Inject a synthetic `eXIf` chunk right before `IEND`
    let iend_offset = base_png.len() - 12;
    let mut png_with_exif = Vec::new();
    png_with_exif.extend_from_slice(&base_png[..iend_offset]);
    let exif_payload = b"Exif\0\0II*\0\x08\0\0\0";
    png_with_exif.extend_from_slice(&(exif_payload.len() as u32).to_be_bytes());
    png_with_exif.extend_from_slice(b"eXIf");
    png_with_exif.extend_from_slice(exif_payload);
    png_with_exif.extend_from_slice(&[0, 0, 0, 0]); // dummy CRC for chunk scanner
    png_with_exif.extend_from_slice(&base_png[iend_offset..]);

    assert!(has_png_metadata(&png_with_exif));

    let cfg = AppConfig {
        strip_metadata: true,
        ..Default::default()
    };
    let res = optimize_rgba(width, height, &rgba, Some(&png_with_exif), &cfg)
        .expect("optimize with exif strip");
    assert!(res.exif_stripped);
    assert!(!has_png_metadata(&res.png_bytes));

    // Also test lossless JPEG APP1 EXIF marker stripping
    let jpeg_with_app1 = vec![
        0xFF, 0xD8, // SOI
        0xFF, 0xE1, 0x00, 0x08, b'E', b'x', b'i', b'f', 0x00, 0x00, // APP1 EXIF
        0xFF, 0xDA, 0x00, 0x02, 0x11, 0x22, 0xFF, 0xD9, // SOS + data + EOI
    ];
    let stripped_jpeg = strip_jpeg_exif(&jpeg_with_app1).expect("strip_jpeg_exif");
    assert_eq!(
        stripped_jpeg,
        vec![0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x02, 0x11, 0x22, 0xFF, 0xD9]
    );
}

#[test]
fn test_downloads_folder_watcher_and_revert() {
    let dir = tempdir().expect("create tempdir");
    let cfg = AppConfig {
        watch_downloads: true,
        downloads_dir: Some(dir.path().to_path_buf()),
        ..Default::default()
    };

    let mut watcher = DownloadsWatcher::new(Some(dir.path().to_path_buf()));

    // Drop a new unoptimized PNG into the watched Downloads folder
    let file_path = dir.path().join("downloaded_screenshot.png");
    let rgba = generate_synthetic_screenshot(300, 200);
    let raw_png = encode_rgba_png(300, 200, &rgba).expect("encode PNG");
    let orig_len = raw_png.len();
    std::fs::write(&file_path, &raw_png).expect("write downloaded PNG");

    // Wait 320ms so the file age check (>300ms) considers the browser download complete
    thread::sleep(Duration::from_millis(320));

    let pending = watcher
        .inspect_next_file(&cfg)
        .expect("should detect newly added image in Downloads");
    assert_eq!(pending.file_name, "downloaded_screenshot.png");

    let summary = watcher
        .optimize_pending_file(pending, &cfg)
        .expect("optimize pending file")
        .expect("file should be modified");
    assert!(summary.optimized_size < orig_len);
    let on_disk_optimized = std::fs::metadata(&file_path).unwrap().len() as usize;
    assert_eq!(on_disk_optimized, summary.optimized_size);

    // Subsequent poll should be a no-op (already seen)
    assert!(watcher.inspect_next_file(&cfg).is_none());

    // Now test one-click Revert!
    let (reverted_path, restored_size) = watcher
        .revert_last()
        .expect("revert_last")
        .expect("backup should exist");
    assert_eq!(reverted_path, file_path);
    assert_eq!(restored_size, orig_len);
    let on_disk_reverted = std::fs::read(&file_path).unwrap();
    assert_eq!(on_disk_reverted, raw_png);
}

#[test]
fn test_clipboard_hash_deduplication() {
    let rgba_a = generate_synthetic_screenshot(128, 128);
    let mut rgba_b = rgba_a.clone();
    rgba_b[0] ^= 0xFF;

    let hash_a1 = compute_image_hash(128, 128, &rgba_a);
    let hash_a2 = compute_image_hash(128, 128, &rgba_a);
    let hash_b = compute_image_hash(128, 128, &rgba_b);

    assert_eq!(hash_a1, hash_a2);
    assert_ne!(hash_a1, hash_b);

    let mut monitor = ClipboardMonitor::new();
    assert!(!monitor.is_seen_rgba_hash(hash_a1));
    monitor.record_rgba_hash(hash_a1);
    assert!(monitor.is_seen_rgba_hash(hash_a1));

    let png_hash = compute_bytes_hash(&[1, 2, 3, 4]);
    monitor.record_png_hash(png_hash);
    assert!(monitor.is_seen_png_hash(png_hash));
}

#[test]
fn test_format_bytes() {
    assert_eq!(format_bytes(512), "512 B");
    assert_eq!(format_bytes(2048), "2.0 KB");
    assert_eq!(format_bytes(1024 * 1024 * 2), "2.00 MB");
}

#[test]
fn test_pdf_native_optimization() {
    let dir = tempdir().expect("create tempdir");
    let pdf_path = dir.path().join("document.pdf");

    // Construct a synthetic PDF with an uncompressed stream and XMP metadata bloat
    let repeated_content =
        "BT /F1 12 Tf 72 720 Td (Hello clop-rs PDF optimizer!) Tj ET\n".repeat(40);
    let xmp_bloat = format!(
        "<?xpacket begin=\"\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>{}<?xpacket end=\"w\"?>",
        " ".repeat(4096)
    );
    let raw_pdf = format!(
        "%PDF-1.5\n\
         1 0 obj\n<< /Type /Catalog /Pages 2 0 R /Metadata 4 0 R >>\nendobj\n\
         2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n\
         3 0 obj\n<< /Type /Page /Parent 2 0 R /Contents 5 0 R >>\nendobj\n\
         4 0 obj\n<< /Type /Metadata /Subtype /XML /Length {} >>\nstream\n{}\nendstream\nendobj\n\
         5 0 obj\n<< /Length {} >>\nstream\n{}\nendstream\nendobj\n\
         trailer\n<< /Root 1 0 R /Size 6 >>\n%%EOF\n",
        xmp_bloat.len(),
        xmp_bloat,
        repeated_content.len(),
        repeated_content
    );
    std::fs::write(&pdf_path, raw_pdf.as_bytes()).expect("write synthetic PDF");

    let cfg = AppConfig::default();
    let res = optimize_file(&pdf_path, None, &cfg).expect("optimize PDF file");

    assert!(res.was_modified, "Expected PDF to be compressed");
    assert!(
        res.optimized_size < res.original_size,
        "Expected optimized PDF ({}) < original ({})",
        res.optimized_size,
        res.original_size
    );
    assert!(
        res.exif_stripped,
        "Expected XMP metadata object to be stripped"
    );

    let optimized_bytes = std::fs::read(&pdf_path).expect("read optimized PDF");
    assert!(optimized_bytes.starts_with(b"%PDF-1.5"));
    assert!(String::from_utf8_lossy(&optimized_bytes).contains("%%EOF"));
}

#[test]
fn test_media_and_video_path_detection() {
    use clop_rs::downloads::is_supported_media_path;
    use clop_rs::pdf::is_pdf_path;
    use clop_rs::video::is_video_path;
    use std::path::Path;

    assert!(is_pdf_path(Path::new("report.pdf")));
    assert!(is_pdf_path(Path::new("REPORT.PDF")));
    assert!(!is_pdf_path(Path::new("image.png")));

    assert!(is_video_path(Path::new("recording.mp4")));
    assert!(is_video_path(Path::new("screen.mov")));
    assert!(is_video_path(Path::new("clip.webm")));
    assert!(!is_video_path(Path::new("photo.jpg")));

    assert!(is_supported_media_path(Path::new("doc.pdf")));
    assert!(is_supported_media_path(Path::new("video.mp4")));
    assert!(is_supported_media_path(Path::new("pic.png")));
    assert!(!is_supported_media_path(Path::new("video.mp4.crdownload")));
    assert!(!is_supported_media_path(Path::new(".hidden.png")));
}

#[test]
fn test_quality_presets_and_slider_mapping() {
    use clop_rs::config::OptimizationPreset;

    let mut cfg = AppConfig::default();

    cfg.apply_preset(OptimizationPreset::Lossless);
    assert_eq!(cfg.mode, OptimizationMode::Lossless);
    assert_eq!(cfg.effective_gs_pdf_setting(), "/prepress");
    assert_eq!(cfg.effective_video_crf(), "18");

    cfg.apply_preset(OptimizationPreset::HighQuality);
    assert_eq!(cfg.mode, OptimizationMode::Smart);
    assert_eq!(cfg.effective_gs_pdf_setting(), "/printer");
    assert_eq!(cfg.effective_video_crf(), "21");
    let hq_jpeg = cfg.effective_jpeg_quality();

    cfg.apply_preset(OptimizationPreset::SmallestSize);
    assert_eq!(cfg.mode, OptimizationMode::Smart);
    assert_eq!(cfg.effective_gs_pdf_setting(), "/screen");
    assert_eq!(cfg.effective_video_crf(), "27");
    let small_jpeg = cfg.effective_jpeg_quality();
    assert!(
        small_jpeg < hq_jpeg,
        "SmallestSize JPEG quality ({}) should be lower than HighQuality ({})",
        small_jpeg,
        hq_jpeg
    );

    // Custom slider at 40 (aggressive size) vs 95 (high quality)
    cfg.apply_quality_level(40);
    assert_eq!(cfg.preset, OptimizationPreset::Custom);
    assert_eq!(cfg.quality_max, 40);
    assert_eq!(cfg.effective_video_crf(), "30");

    // Verify JPEG file compression produces strictly smaller files with SmallestSize than HighQuality
    let dir = tempdir().expect("create tempdir");
    let hq_in = dir.path().join("photo_hq.jpg");
    let hq_out = dir.path().join("photo_hq_out.jpg");
    let small_out = dir.path().join("photo_small_out.jpg");

    let rgba = generate_synthetic_screenshot(320, 240);
    let rgb =
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_raw(320, 240, rgba).expect("rgba"))
            .into_rgb8();
    let mut raw_jpg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut raw_jpg, 98)
        .encode_image(&rgb)
        .expect("encode jpg");
    std::fs::write(&hq_in, &raw_jpg).unwrap();

    let mut hq_cfg = AppConfig::default();
    hq_cfg.apply_preset(OptimizationPreset::HighQuality);
    let res_hq = optimize_file(&hq_in, Some(&hq_out), &hq_cfg).unwrap();

    let mut small_cfg = AppConfig::default();
    small_cfg.apply_preset(OptimizationPreset::SmallestSize);
    let res_small = optimize_file(&hq_in, Some(&small_out), &small_cfg).unwrap();

    assert!(
        res_small.optimized_size < res_hq.optimized_size,
        "SmallestSize JPEG ({}) should be strictly smaller than HighQuality JPEG ({})",
        res_small.optimized_size,
        res_hq.optimized_size
    );
}

#[test]
fn test_optimization_history_persistence_and_stats() {
    use clop_rs::clipboard::OptimizationSummary;
    use clop_rs::history::{HistorySource, OptimizationHistory};

    let dir = tempdir().expect("create tempdir");
    let hist_path = dir.path().join("history.json");

    let mut history = OptimizationHistory::default();
    let cfg = AppConfig::default();

    let clip_summary = OptimizationSummary {
        source_label: "Clipboard".to_string(),
        width: 1920,
        height: 1080,
        original_size: 1_000_000,
        optimized_size: 250_000,
        saved_bytes: 750_000,
        saved_percent: 75.0,
        exif_stripped: true,
        elapsed: Duration::from_millis(45),
        thumbnail_64: vec![128u8; 64 * 64 * 4],
    };
    let entry1 = history.record_summary(HistorySource::Clipboard, &clip_summary, None, &cfg);
    assert!(entry1
        .thumbnail_data_url
        .as_deref()
        .is_some_and(|s| s.starts_with("data:image/png;base64,")));

    let file_summary = OptimizationSummary {
        source_label: "report.pdf".to_string(),
        width: 0,
        height: 0,
        original_size: 500_000,
        optimized_size: 250_000,
        saved_bytes: 250_000,
        saved_percent: 50.0,
        exif_stripped: false,
        elapsed: Duration::from_millis(120),
        thumbnail_64: Vec::new(),
    };
    history.record_summary(
        HistorySource::File,
        &file_summary,
        Some(std::path::Path::new("/tmp/report.pdf")),
        &cfg,
    );

    history.save_to_path(&hist_path).expect("save history");
    let loaded = OptimizationHistory::load_from_path(&hist_path).expect("load history");
    assert_eq!(loaded.entries.len(), 2);
    // Newest first
    assert_eq!(loaded.entries[0].label, "report.pdf");
    assert_eq!(loaded.entries[1].label, "Clipboard");

    let stats = loaded.stats();
    assert_eq!(stats.total_items, 2);
    assert_eq!(stats.clipboard_items, 1);
    assert_eq!(stats.file_items, 1);
    assert_eq!(stats.total_original_bytes, 1_500_000);
    assert_eq!(stats.total_optimized_bytes, 500_000);
    assert_eq!(stats.total_saved_bytes, 1_000_000);
    assert!((stats.overall_saved_percent - 66.666).abs() < 0.1);
}

#[test]
fn test_supported_formats_and_file_inspection() {
    use clop_rs::optimizer::supported_formats_info;
    use clop_rs::ui::{expand_paths, inspect_file_info};

    let info = supported_formats_info();
    assert!(info.images.contains(&"png".to_string()));
    assert!(info.images.contains(&"jpg".to_string()));
    assert!(info.documents.contains(&"pdf".to_string()));
    assert!(info.videos.contains(&"mp4".to_string()));

    let dir = tempdir().expect("create tempdir");
    let img_path = dir.path().join("photo.png");
    let txt_path = dir.path().join("notes.txt");
    std::fs::write(&img_path, b"dummy").unwrap();
    std::fs::write(&txt_path, b"hello").unwrap();

    let img_info = inspect_file_info(&img_path);
    assert!(img_info.supported);
    assert_eq!(img_info.category, "image");

    let txt_info = inspect_file_info(&txt_path);
    assert!(!txt_info.supported);
    assert_eq!(txt_info.category, "unsupported");

    // Expanding a directory only picks up supported media files
    let expanded = expand_paths(&[dir.path().to_path_buf()]);
    assert_eq!(expanded.len(), 1);
    assert_eq!(expanded[0], img_path);
}

#[test]
fn test_clop_cli_formats_and_optimize() {
    use std::process::Command;

    let cli_bin = env!("CARGO_BIN_EXE_clop-cli");

    // 1. Test `clop-cli formats --json`
    let out = Command::new(cli_bin)
        .args(["formats", "--json"])
        .output()
        .expect("run clop-cli formats");
    assert!(out.status.success(), "clop-cli formats failed");
    let parsed: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("parse formats json");
    assert!(parsed["images"].as_array().unwrap().len() >= 5);
    assert!(!parsed["documents"].as_array().unwrap().is_empty());
    assert!(parsed["videos"].as_array().unwrap().len() >= 4);

    // 2. Test `clop-cli optimize --preset smallest-size --json <file>`
    let dir = tempdir().expect("create tempdir");
    let input_path = dir.path().join("cli_test.png");
    let output_path = dir.path().join("cli_test_out.png");
    let rgba = generate_synthetic_screenshot(240, 180);
    let raw_png = encode_rgba_png(240, 180, &rgba).expect("encode PNG");
    std::fs::write(&input_path, &raw_png).unwrap();

    let opt_out = Command::new(cli_bin)
        .args([
            "optimize",
            "--preset",
            "smallest-size",
            "--output",
            output_path.to_str().unwrap(),
            "--json",
            input_path.to_str().unwrap(),
        ])
        .output()
        .expect("run clop-cli optimize");
    assert!(
        opt_out.status.success(),
        "clop-cli optimize failed: {}",
        String::from_utf8_lossy(&opt_out.stderr)
    );
    let opt_json: serde_json::Value =
        serde_json::from_slice(&opt_out.stdout).expect("parse optimize json");
    assert_eq!(opt_json["files_modified"].as_u64(), Some(1));
    assert!(opt_json["total_saved_bytes"].as_u64().unwrap() > 0);
    assert!(output_path.exists());
}

#[test]
fn test_mozjpeg_progressive_and_trellis_compression() {
    use clop_rs::optimizer::encode_jpeg_mozjpeg;

    let (w, h) = (320u32, 240u32);
    let rgba = generate_synthetic_screenshot(w, h);
    let rgb = image::DynamicImage::ImageRgba8(
        image::RgbaImage::from_raw(w, h, rgba).expect("valid rgba"),
    )
    .into_rgb8();

    // Baseline image crate JPEG encoder at Q85
    let mut baseline_jpg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut baseline_jpg, 85)
        .encode_image(&rgb)
        .expect("baseline jpeg encode");

    // MozJPEG progressive + trellis encoder at Q85
    let moz_prog =
        encode_jpeg_mozjpeg(w, h, rgb.as_raw(), 85, true).expect("mozjpeg progressive encode");
    assert!(moz_prog.starts_with(&[0xFF, 0xD8]), "valid JPEG SOI");
    // Progressive JPEG must contain an SOF2 (0xFF 0xC2) marker
    assert!(
        moz_prog.windows(2).any(|w| w == [0xFF, 0xC2]),
        "MozJPEG progressive output should contain SOF2 progressive marker"
    );
    assert!(
        moz_prog.len() < baseline_jpg.len(),
        "MozJPEG progressive ({} B) should be smaller than baseline image::JpegEncoder ({} B)",
        moz_prog.len(),
        baseline_jpg.len()
    );

    // Decodes cleanly back to original dimensions
    let decoded = image::load_from_memory_with_format(&moz_prog, image::ImageFormat::Jpeg)
        .expect("decode mozjpeg output");
    assert_eq!(decoded.width(), w);
    assert_eq!(decoded.height(), h);

    // MozJPEG baseline + trellis mode (used for PDF embedded streams)
    let moz_base =
        encode_jpeg_mozjpeg(w, h, rgb.as_raw(), 85, false).expect("mozjpeg baseline encode");
    assert!(moz_base.starts_with(&[0xFF, 0xD8]));
    assert!(
        moz_base.len() < baseline_jpg.len(),
        "MozJPEG baseline+trellis ({} B) should be smaller than image::JpegEncoder ({} B)",
        moz_base.len(),
        baseline_jpg.len()
    );
}
