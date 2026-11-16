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
