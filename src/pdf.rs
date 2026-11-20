use crate::config::{AppConfig, OptimizationMode};
use crate::optimizer::{strip_jpeg_exif, OptimizedImage};
use anyhow::{bail, Context, Result};
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use image::RgbaImage;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

/// Returns `true` if `path` has a `.pdf` extension.
pub fn is_pdf_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
}

/// Optimizes a PDF file on disk using:
/// 1. Built-in pure-Rust PDF stream & embedded-image optimizer (re-compresses `/FlateDecode` streams,
///    optimizes embedded `/DCTDecode` JPEG images, strips XMP padding/metadata, and rebuilds `xref`).
/// 2. Optional system Ghostscript (`gs` / `gswin64c`) pass if installed, selecting whichever produces
///    the smallest valid PDF strictly smaller than the original.
pub fn optimize_pdf_file(
    input_path: &Path,
    output_path: Option<&Path>,
    config: &AppConfig,
) -> Result<OptimizedImage> {
    let start = Instant::now();
    let raw_bytes = fs::read(input_path)
        .with_context(|| format!("Failed to read PDF file: {}", input_path.display()))?;
    let original_size = raw_bytes.len();

    if original_size < 16 || !raw_bytes.starts_with(b"%PDF-") {
        bail!("Not a valid PDF file: {}", input_path.display());
    }

    let mut best_bytes = raw_bytes.clone();
    let mut metadata_stripped = false;

    // 1. Run built-in pure-Rust PDF stream & embedded-JPEG optimizer
    if let Some((rust_opt, stripped)) = optimize_pdf_bytes_native(&raw_bytes, config) {
        if rust_opt.len() < best_bytes.len() && rust_opt.starts_with(b"%PDF-") {
            best_bytes = rust_opt;
            metadata_stripped = stripped;
        }
    }

    // 2. Also try Ghostscript (`gs` / `gswin64c`) if installed on the system
    if let Some(gs_bytes) = try_ghostscript_pdf(input_path, config) {
        if gs_bytes.len() < best_bytes.len() && gs_bytes.starts_with(b"%PDF-") {
            best_bytes = gs_bytes;
            if config.strip_metadata {
                metadata_stripped = true;
            }
        }
    }

    let target = output_path.unwrap_or(input_path);
    let was_modified = best_bytes.len() < original_size;

    if was_modified || output_path.is_some() {
        fs::write(target, &best_bytes)
            .with_context(|| format!("Failed to write optimized PDF to {}", target.display()))?;
    }

    let optimized_size = if was_modified {
        best_bytes.len()
    } else {
        original_size
    };

    Ok(OptimizedImage {
        png_bytes: Vec::new(),
        rgba_bytes: procedural_pdf_icon_64(),
        width: 0,
        height: 0,
        original_size,
        optimized_size,
        was_modified,
        exif_stripped: metadata_stripped && was_modified,
        elapsed: start.elapsed(),
    })
}

/// Pure-Rust PDF stream optimizer:
/// - Locates indirect objects (`<id> <gen> obj ... endobj`)
/// - Re-compresses `/FlateDecode` streams at maximum zlib level (level 9)
/// - Optimizes embedded `/DCTDecode` JPEG streams (EXIF strip + perceptual JPEG re-encoding in Smart mode)
/// - Strips XMP `/Metadata` stream padding when `config.strip_metadata` is enabled
/// - Rebuilds the PDF `xref` table with exact updated byte offsets
pub fn optimize_pdf_bytes_native(pdf_bytes: &[u8], config: &AppConfig) -> Option<(Vec<u8>, bool)> {
    if !pdf_bytes.starts_with(b"%PDF-") {
        return None;
    }

    // Extract PDF header line (e.g. `%PDF-1.7\n%\xE2\xE3\xCF\xD3\n`)
    let first_nl = pdf_bytes.iter().position(|&b| b == b'\n')?;
    let mut out = Vec::with_capacity(pdf_bytes.len());
    out.extend_from_slice(&pdf_bytes[..=first_nl]);
    out.extend_from_slice(b"%\xE2\xE3\xCF\xD3\n");

    let objects = parse_indirect_objects(pdf_bytes);
    if objects.is_empty() {
        return None;
    }

    let mut offsets: BTreeMap<u32, usize> = BTreeMap::new();
    let mut root_ref: Option<u32> = None;
    let mut info_ref: Option<u32> = None;
    let mut metadata_stripped = false;

    for obj in objects {
        if config.strip_metadata && is_metadata_only_object(&obj.body) {
            metadata_stripped = true;
            continue;
        }

        if root_ref.is_none() && contains_subslice(&obj.body, b"/Type /Catalog")
            || contains_subslice(&obj.body, b"/Type/Catalog")
        {
            root_ref = Some(obj.id);
        }
        if !config.strip_metadata
            && info_ref.is_none()
            && (contains_subslice(&obj.body, b"/Title")
                || contains_subslice(&obj.body, b"/Producer")
                || contains_subslice(&obj.body, b"/Creator"))
            && !contains_subslice(&obj.body, b"stream")
        {
            info_ref = Some(obj.id);
        }

        let (optimized_body, stripped_here) = optimize_object_body(&obj.body, config);
        if stripped_here {
            metadata_stripped = true;
        }

        offsets.insert(obj.id, out.len());
        let _ = writeln!(out, "{} {} obj", obj.id, obj.gen);
        out.extend_from_slice(&optimized_body);
        if !optimized_body.ends_with(b"\n") {
            out.push(b'\n');
        }
        out.extend_from_slice(b"endobj\n");
    }

    // Find `/Root` reference from original trailer if catalog wasn't explicitly matched
    if root_ref.is_none() {
        root_ref = extract_root_id_from_trailer(pdf_bytes);
    }
    let root_id = root_ref?;

    // Write rebuilt `xref` table
    let max_id = *offsets.keys().next_back()?;
    let xref_offset = out.len();
    let _ = writeln!(out, "xref\n0 {}", max_id + 1);
    out.extend_from_slice(b"0000000000 65535 f \n");
    for id in 1..=max_id {
        if let Some(&off) = offsets.get(&id) {
            let _ = writeln!(out, "{:010} 00000 n ", off);
        } else {
            out.extend_from_slice(b"0000000000 65535 f \n");
        }
    }

    // Write `trailer` and `startxref`
    let _ = writeln!(
        out,
        "trailer\n<< /Size {} /Root {} 0 R",
        max_id + 1,
        root_id
    );
    if let Some(info_id) = info_ref {
        if !config.strip_metadata {
            let _ = writeln!(out, "/Info {} 0 R", info_id);
        }
    }
    let _ = writeln!(out, ">>\nstartxref\n{}\n%%EOF", xref_offset);

    Some((out, metadata_stripped))
}

struct PdfObject {
    id: u32,
    gen: u32,
    body: Vec<u8>,
}

fn parse_indirect_objects(pdf: &[u8]) -> Vec<PdfObject> {
    let mut objs = Vec::new();
    let mut pos = 0;

    while pos < pdf.len() {
        // Look for `<id> <gen> obj` at line/whitespace boundaries
        if let Some((obj_start, body_start, id, gen)) = find_next_obj_header(pdf, pos) {
            let _ = obj_start;
            if let Some(end_rel) = find_subslice(&pdf[body_start..], b"endobj") {
                let mut body = pdf[body_start..body_start + end_rel].to_vec();
                while body.starts_with(b"\r") || body.starts_with(b"\n") {
                    body.remove(0);
                }
                while body.ends_with(b"\r") || body.ends_with(b"\n") {
                    body.pop();
                }
                objs.push(PdfObject { id, gen, body });
                pos = body_start + end_rel + 6;
                continue;
            }
        }
        break;
    }

    objs
}

fn find_next_obj_header(pdf: &[u8], mut i: usize) -> Option<(usize, usize, u32, u32)> {
    while i < pdf.len() {
        // Must be at start or preceded by whitespace/delimiter
        if i > 0 && !pdf[i - 1].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        let Some((id, next_i)) = parse_ascii_u32(pdf, i) else {
            i += 1;
            continue;
        };
        i = next_i;
        if i >= pdf.len() || !pdf[i].is_ascii_whitespace() {
            i = start + 1;
            continue;
        }
        while i < pdf.len() && pdf[i].is_ascii_whitespace() {
            i += 1;
        }
        let Some((gen, next_i2)) = parse_ascii_u32(pdf, i) else {
            i = start + 1;
            continue;
        };
        i = next_i2;
        if i >= pdf.len() || !pdf[i].is_ascii_whitespace() {
            i = start + 1;
            continue;
        }
        while i < pdf.len() && pdf[i].is_ascii_whitespace() {
            i += 1;
        }
        if pdf[i..].starts_with(b"obj")
            && (i + 3 == pdf.len() || pdf[i + 3].is_ascii_whitespace() || pdf[i + 3] == b'<')
        {
            return Some((start, i + 3, id, gen));
        }
        i = start + 1;
    }
    None
}

fn parse_ascii_u32(bytes: &[u8], start: usize) -> Option<(u32, usize)> {
    let mut i = start;
    let mut val: u32 = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        val = val.checked_mul(10)?.checked_add((bytes[i] - b'0') as u32)?;
        i += 1;
    }
    if i > start {
        Some((val, i))
    } else {
        None
    }
}

fn is_metadata_only_object(body: &[u8]) -> bool {
    (contains_subslice(body, b"/Type /Metadata") || contains_subslice(body, b"/Type/Metadata"))
        && (contains_subslice(body, b"/Subtype /XML") || contains_subslice(body, b"/Subtype/XML"))
}

fn optimize_object_body(body: &[u8], config: &AppConfig) -> (Vec<u8>, bool) {
    let Some(stream_kw) = find_subslice(body, b"stream") else {
        // Non-stream object: strip `/Metadata X 0 R` links if stripping metadata
        if config.strip_metadata && contains_subslice(body, b"/Metadata") {
            let cleaned = remove_metadata_ref(body);
            let stripped = cleaned.len() < body.len();
            return (cleaned, stripped);
        }
        return (body.to_vec(), false);
    };

    let dict_bytes = &body[..stream_kw];
    let mut data_start = stream_kw + 6;
    if data_start < body.len() && body[data_start] == b'\r' {
        data_start += 1;
    }
    if data_start < body.len() && body[data_start] == b'\n' {
        data_start += 1;
    }

    let Some(endstream_rel) = find_subslice(&body[data_start..], b"endstream") else {
        return (body.to_vec(), false);
    };
    let mut data_end = data_start + endstream_rel;
    if data_end > data_start && body[data_end - 1] == b'\n' {
        data_end -= 1;
    }
    if data_end > data_start && body[data_end - 1] == b'\r' {
        data_end -= 1;
    }

    let raw_stream = &body[data_start..data_end];
    let mut new_stream: Option<Vec<u8>> = None;
    let mut added_flate_filter = false;
    let mut stripped_meta = false;

    let is_dct = contains_subslice(dict_bytes, b"/DCTDecode");
    let is_flate = contains_subslice(dict_bytes, b"/FlateDecode");
    let has_other_filter = contains_subslice(dict_bytes, b"/Filter");

    if is_dct {
        // Embedded JPEG stream inside PDF!
        let mut best_jpeg = raw_stream.to_vec();
        if config.strip_metadata {
            if let Some(stripped) = strip_jpeg_exif(&best_jpeg) {
                if stripped.len() < best_jpeg.len() {
                    best_jpeg = stripped;
                    stripped_meta = true;
                }
            }
        }
        if config.mode == OptimizationMode::Smart {
            if let Ok(decoded) =
                image::load_from_memory_with_format(&best_jpeg, image::ImageFormat::Jpeg)
            {
                let rgba = decoded.into_rgba8();
                let (w, h) = rgba.dimensions();
                let rgb = image::DynamicImage::ImageRgba8(
                    RgbaImage::from_raw(w, h, rgba.into_raw()).unwrap(),
                )
                .into_rgb8();
                let mut reencoded = Vec::new();
                let q = config.effective_pdf_jpeg_quality();
                let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut reencoded, q);
                if enc.encode_image(&rgb).is_ok() && reencoded.len() < best_jpeg.len() {
                    best_jpeg = reencoded;
                }
            }
        }
        if best_jpeg.len() < raw_stream.len() {
            new_stream = Some(best_jpeg);
        }
    } else if is_flate {
        // Decompress existing FlateDecode stream and re-compress at level 9
        let mut decoder = ZlibDecoder::new(raw_stream);
        let mut decompressed = Vec::new();
        if decoder.read_to_end(&mut decompressed).is_ok() && !decompressed.is_empty() {
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
            if encoder.write_all(&decompressed).is_ok() {
                if let Ok(recompressed) = encoder.finish() {
                    if recompressed.len() < raw_stream.len() {
                        new_stream = Some(recompressed);
                    }
                }
            }
        }
    } else if !has_other_filter && raw_stream.len() > 64 {
        // Uncompressed PDF content stream: compress with FlateDecode!
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
        if encoder.write_all(raw_stream).is_ok() {
            if let Ok(compressed) = encoder.finish() {
                if compressed.len() + 24 < raw_stream.len() {
                    new_stream = Some(compressed);
                    added_flate_filter = true;
                }
            }
        }
    }

    let Some(final_stream) = new_stream else {
        return (body.to_vec(), stripped_meta);
    };

    let updated_dict = update_stream_dictionary(dict_bytes, final_stream.len(), added_flate_filter);
    let mut rebuilt = Vec::with_capacity(updated_dict.len() + final_stream.len() + 24);
    rebuilt.extend_from_slice(&updated_dict);
    if !rebuilt.ends_with(b"\n") && !rebuilt.ends_with(b" ") {
        rebuilt.push(b'\n');
    }
    rebuilt.extend_from_slice(b"stream\n");
    rebuilt.extend_from_slice(&final_stream);
    rebuilt.extend_from_slice(b"\nendstream");

    (rebuilt, stripped_meta)
}

fn update_stream_dictionary(dict_bytes: &[u8], new_len: usize, add_flate: bool) -> Vec<u8> {
    let dict_str = String::from_utf8_lossy(dict_bytes);
    let mut out = String::with_capacity(dict_str.len() + 32);

    // Replace direct `/Length <number>` with updated length
    let mut replaced_len = false;
    let mut rest = dict_str.as_ref();
    while let Some(idx) = rest.find("/Length") {
        out.push_str(&rest[..idx]);
        let after = &rest[idx + 7..];
        let trimmed = after.trim_start();
        let num_end = trimmed
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(trimmed.len());
        let after_num = trimmed[num_end..].trim_start();
        // Ensure it's a direct integer `/Length 1234` and not an indirect `/Length 5 0 R`
        if num_end > 0 && !after_num.starts_with("0 R") && !replaced_len {
            out.push_str(&format!("/Length {}", new_len));
            replaced_len = true;
            rest = &trimmed[num_end..];
        } else {
            out.push_str("/Length");
            rest = after;
        }
    }
    out.push_str(rest);

    if add_flate {
        if let Some(end_dict) = out.rfind(">>") {
            out.insert_str(end_dict, " /Filter /FlateDecode ");
        }
    }

    out.into_bytes()
}

fn remove_metadata_ref(body: &[u8]) -> Vec<u8> {
    let s = String::from_utf8_lossy(body);
    if let Some(idx) = s.find("/Metadata") {
        let after = &s[idx + 9..];
        if let Some(r_idx) = after.find(" R") {
            let mut out = String::with_capacity(s.len());
            out.push_str(&s[..idx]);
            out.push_str(&after[r_idx + 2..]);
            return out.into_bytes();
        }
    }
    body.to_vec()
}

fn extract_root_id_from_trailer(pdf: &[u8]) -> Option<u32> {
    let pos = find_subslice(pdf, b"/Root")?;
    let mut i = pos + 5;
    while i < pdf.len() && pdf[i].is_ascii_whitespace() {
        i += 1;
    }
    let (id, _) = parse_ascii_u32(pdf, i)?;
    Some(id)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    find_subslice(haystack, needle).is_some()
}

fn try_ghostscript_pdf(input_path: &Path, config: &AppConfig) -> Option<Vec<u8>> {
    let gs_bin = find_ghostscript()?;
    let tmp_out = env::temp_dir().join(format!("clop_rs_pdf_{}.pdf", std::process::id()));
    let _ = fs::remove_file(&tmp_out);

    let setting = config.effective_gs_pdf_setting();

    let status = Command::new(gs_bin)
        .args([
            "-sDEVICE=pdfwrite",
            "-dCompatibilityLevel=1.5",
            &format!("-dPDFSETTINGS={setting}"),
            "-dNOPAUSE",
            "-dQUIET",
            "-dBATCH",
            &format!("-sOutputFile={}", tmp_out.display()),
        ])
        .arg(input_path)
        .status()
        .ok()?;

    if !status.success() || !tmp_out.exists() {
        let _ = fs::remove_file(&tmp_out);
        return None;
    }

    let bytes = fs::read(&tmp_out).ok();
    let _ = fs::remove_file(&tmp_out);
    bytes
}

pub fn find_ghostscript() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        for candidate in ["/opt/homebrew/bin/gs", "/usr/local/bin/gs"] {
            let p = PathBuf::from(candidate);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    let names = if cfg!(target_os = "windows") {
        &["gswin64c.exe", "gswin32c.exe", "gs.exe"][..]
    } else {
        &["gs"][..]
    };
    if let Some(path_var) = env::var_os("PATH") {
        for dir in env::split_paths(&path_var) {
            for name in names {
                let full = dir.join(name);
                if full.is_file() {
                    return Some(full);
                }
            }
        }
    }
    None
}

/// Generates a 64x64 RGBA icon showing a crisp document page with a red "PDF" banner.
pub fn procedural_pdf_icon_64() -> Vec<u8> {
    let mut rgba = vec![0u8; 64 * 64 * 4];
    for y in 0..64u32 {
        for x in 0..64u32 {
            let idx = ((y * 64 + x) * 4) as usize;
            let in_page = (14..=50).contains(&x) && (8..=56).contains(&y);
            let in_banner = (10..=54).contains(&x) && (34..=46).contains(&y);
            let color = if in_banner {
                [220, 38, 38, 255]
            } else if in_page {
                if x == 14 || x == 50 || y == 8 || y == 56 {
                    [148, 163, 184, 255]
                } else if (20..=44).contains(&x) && (y == 18 || y == 24) {
                    [100, 116, 139, 255]
                } else {
                    [241, 245, 249, 255]
                }
            } else {
                [15, 23, 42, 255]
            };
            rgba[idx..idx + 4].copy_from_slice(&color);
        }
    }
    rgba
}
