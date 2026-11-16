use crate::config::{AppConfig, OptimizationMode};
use anyhow::{bail, Context, Result};
use image::{imageops::FilterType, ImageFormat, RgbaImage};
use oxipng::{Deflaters, InFile, Options, OutFile, StripChunks};
use std::fs;
use std::io::Cursor;
use std::path::Path;
use std::time::{Duration, Instant};

/// Result of optimizing an in-memory or clipboard image.
#[derive(Debug, Clone)]
pub struct OptimizedImage {
    /// Exact optimized image file bytes (PNG for clipboard/PNG files, or JPEG if lossless JPEG strip).
    pub png_bytes: Vec<u8>,
    /// Raw RGBA pixel buffer (`width * height * 4` bytes) of the resulting image.
    pub rgba_bytes: Vec<u8>,
    /// Final width in pixels.
    pub width: u32,
    /// Final height in pixels.
    pub height: u32,
    /// Original byte size before optimization.
    pub original_size: usize,
    /// Final byte size after optimization.
    pub optimized_size: usize,
    /// Whether the image was reduced in size, dimensions, or had EXIF stripped.
    pub was_modified: bool,
    /// Whether EXIF or ancillary metadata chunks were stripped during optimization.
    pub exif_stripped: bool,
    /// Wall-clock duration spent optimizing.
    pub elapsed: Duration,
}

impl OptimizedImage {
    /// Bytes saved (`original_size - optimized_size`).
    pub fn saved_bytes(&self) -> usize {
        self.original_size.saturating_sub(self.optimized_size)
    }

    /// Percentage reduction (`0.0..=100.0`).
    pub fn saved_percent(&self) -> f64 {
        if self.original_size == 0 || self.optimized_size >= self.original_size {
            return 0.0;
        }
        ((self.original_size - self.optimized_size) as f64 / self.original_size as f64) * 100.0
    }
}

/// Optimizes an image starting from raw RGBA pixels and an optional original PNG byte buffer.
pub fn optimize_rgba(
    width: u32,
    height: u32,
    rgba_bytes: &[u8],
    original_png: Option<&[u8]>,
    config: &AppConfig,
) -> Result<OptimizedImage> {
    let start = Instant::now();
    let expected_len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|px| px.checked_mul(4))
        .context("Image dimensions overflow")?;

    if width == 0 || height == 0 || rgba_bytes.len() != expected_len {
        bail!(
            "Invalid RGBA buffer: dimensions {}x{}, expected {} bytes, got {}",
            width,
            height,
            expected_len,
            rgba_bytes.len()
        );
    }

    let had_metadata = config.strip_metadata
        && original_png
            .filter(|b| !b.is_empty())
            .is_some_and(has_png_metadata);

    // 1. Optional resolution downscaling (scale_factor, max_width, max_height)
    let (work_w, work_h, work_rgba, resized) = maybe_downscale(width, height, rgba_bytes, config)?;

    // 2. Establish baseline PNG bytes and size
    let baseline_png = match (original_png, resized) {
        (Some(raw), false) if !raw.is_empty() => raw.to_vec(),
        _ => encode_rgba_png(work_w, work_h, &work_rgba)?,
    };
    let original_size = original_png
        .filter(|b| !b.is_empty())
        .map(|b| b.len())
        .unwrap_or_else(|| baseline_png.len());

    // 3. Run Smart (imagequant palette + oxipng) or Lossless (oxipng) pipeline.
    // If the input PNG is already an 8-bit indexed palette PNG (and wasn't resized),
    // skip lossy re-quantization to preserve palette quality and prevent generation loss.
    let already_indexed = !resized
        && original_png
            .filter(|b| !b.is_empty())
            .is_some_and(is_indexed_png);

    let candidate_png = match (config.mode, already_indexed) {
        (OptimizationMode::Smart, false) => {
            match quantize_and_oxipng(work_w, work_h, &work_rgba, config) {
                Ok(quantized_png) => quantized_png,
                Err(err) => {
                    log::debug!(
                        "Perceptual quantization skipped ({}), falling back to lossless oxipng",
                        err
                    );
                    run_oxipng(&baseline_png, config)?
                }
            }
        }
        _ => run_oxipng(&baseline_png, config)?,
    };

    // If EXIF/metadata stripping was requested and `candidate_png` still had metadata or wasn't smaller,
    // also try direct PNG metadata chunk stripping.
    let candidate_png = if config.strip_metadata && has_png_metadata(&candidate_png) {
        strip_png_metadata_chunks(&candidate_png).unwrap_or(candidate_png)
    } else {
        candidate_png
    };

    let exif_stripped = had_metadata && !has_png_metadata(&candidate_png);

    // 4. Size guard: never produce a larger PNG unless the user explicitly downscaled dimensions
    let (final_png, was_modified) = if candidate_png.len() < baseline_png.len()
        || resized
        || (exif_stripped && candidate_png.len() <= baseline_png.len())
    {
        (candidate_png, true)
    } else {
        (baseline_png, false)
    };

    // Decode final RGBA so callers that also set bitmap/RGBA representations match the quantized output
    let final_rgba = if was_modified && config.mode == OptimizationMode::Smart {
        decode_png_to_rgba(&final_png).unwrap_or(work_rgba)
    } else {
        work_rgba
    };

    let optimized_size = final_png.len();
    Ok(OptimizedImage {
        png_bytes: final_png,
        rgba_bytes: final_rgba,
        width: work_w,
        height: work_h,
        original_size,
        optimized_size,
        was_modified: was_modified && (optimized_size < original_size || resized || exif_stripped),
        exif_stripped,
        elapsed: start.elapsed(),
    })
}

/// Optimizes an existing PNG byte buffer directly.
pub fn optimize_png_bytes(png_bytes: &[u8], config: &AppConfig) -> Result<OptimizedImage> {
    let img = image::load_from_memory_with_format(png_bytes, ImageFormat::Png)
        .context("Failed to decode PNG from memory")?
        .into_rgba8();
    let (w, h) = img.dimensions();
    optimize_rgba(w, h, img.as_raw(), Some(png_bytes), config)
}

/// Optimizes an image file on disk (in-place or written to `output_path`).
pub fn optimize_file(
    input_path: &Path,
    output_path: Option<&Path>,
    config: &AppConfig,
) -> Result<OptimizedImage> {
    let start = Instant::now();
    let raw_bytes = fs::read(input_path)
        .with_context(|| format!("Failed to read image file: {}", input_path.display()))?;
    let original_size = raw_bytes.len();

    let format = image::guess_format(&raw_bytes).with_context(|| {
        format!(
            "Unsupported or unrecognized image format: {}",
            input_path.display()
        )
    })?;

    let decoded = image::load_from_memory_with_format(&raw_bytes, format)
        .with_context(|| format!("Failed to decode image: {}", input_path.display()))?
        .into_rgba8();
    let (w, h) = decoded.dimensions();

    // Special handling for JPEG files when target is still a `.jpg`/`.jpeg` file:
    // Strip EXIF/APP markers losslessly and/or re-encode if downscaling or smart mode improves size.
    let target = output_path.unwrap_or(input_path);
    let target_is_jpeg = target
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jpg") || ext.eq_ignore_ascii_case("jpeg"));

    if format == ImageFormat::Jpeg && target_is_jpeg {
        let (work_w, work_h, work_rgba, resized) = maybe_downscale(w, h, decoded.as_raw(), config)?;
        let (stripped_jpeg, exif_removed) = if config.strip_metadata {
            match strip_jpeg_exif(&raw_bytes) {
                Some(stripped) => {
                    let removed = stripped.len() < raw_bytes.len();
                    (stripped, removed)
                }
                None => (raw_bytes.clone(), false),
            }
        } else {
            (raw_bytes.clone(), false)
        };

        let mut best_bytes = stripped_jpeg;
        if resized || config.mode == OptimizationMode::Smart {
            let quality = config.quality_max.clamp(60, 95);
            let mut reencoded = Vec::new();
            let rgb_img = image::DynamicImage::ImageRgba8(
                RgbaImage::from_raw(work_w, work_h, work_rgba.clone()).unwrap(),
            )
            .into_rgb8();
            let mut encoder =
                image::codecs::jpeg::JpegEncoder::new_with_quality(&mut reencoded, quality);
            if encoder.encode_image(&rgb_img).is_ok()
                && (resized || reencoded.len() < best_bytes.len())
            {
                best_bytes = reencoded;
            }
        }

        let was_modified = best_bytes.len() < original_size || resized || exif_removed;
        if was_modified || output_path.is_some() {
            fs::write(target, &best_bytes).with_context(|| {
                format!("Failed to write optimized JPEG to {}", target.display())
            })?;
        }
        let optimized_size = if was_modified {
            best_bytes.len()
        } else {
            original_size
        };
        return Ok(OptimizedImage {
            png_bytes: best_bytes,
            rgba_bytes: work_rgba,
            width: work_w,
            height: work_h,
            original_size,
            optimized_size,
            was_modified,
            exif_stripped: exif_removed,
            elapsed: start.elapsed(),
        });
    }

    let original_png_ref = if format == ImageFormat::Png {
        Some(raw_bytes.as_slice())
    } else {
        None
    };

    let mut result = optimize_rgba(w, h, decoded.as_raw(), original_png_ref, config)?;
    result.original_size = original_size;

    if result.png_bytes.len() < original_size || result.exif_stripped || output_path.is_some() {
        fs::write(target, &result.png_bytes)
            .with_context(|| format!("Failed to write optimized image to {}", target.display()))?;
        result.optimized_size = result.png_bytes.len();
        result.was_modified = result.optimized_size < original_size || result.exif_stripped;
    } else {
        result.optimized_size = original_size;
        result.was_modified = false;
    }

    Ok(result)
}

/// Generates a square RGBA thumbnail (`thumb_size x thumb_size`) with aspect-preserving fit
/// for rendering inside the Floating HUD Panel.
pub fn generate_thumbnail(width: u32, height: u32, rgba_bytes: &[u8], thumb_size: u32) -> Vec<u8> {
    let mut canvas = vec![0u8; (thumb_size * thumb_size * 4) as usize];
    if width == 0 || height == 0 || rgba_bytes.len() < (width * height * 4) as usize {
        return canvas;
    }

    let scale = (thumb_size as f64 / width as f64).min(thumb_size as f64 / height as f64);
    let dst_w = ((width as f64 * scale).round() as u32).clamp(1, thumb_size);
    let dst_h = ((height as f64 * scale).round() as u32).clamp(1, thumb_size);
    let pad_x = (thumb_size - dst_w) / 2;
    let pad_y = (thumb_size - dst_h) / 2;

    if let Some(src_img) = RgbaImage::from_raw(width, height, rgba_bytes.to_vec()) {
        let resized = image::imageops::resize(&src_img, dst_w, dst_h, FilterType::Triangle);
        for y in 0..dst_h {
            for x in 0..dst_w {
                let px = resized.get_pixel(x, y);
                let cx = pad_x + x;
                let cy = pad_y + y;
                let idx = ((cy * thumb_size + cx) * 4) as usize;
                canvas[idx..idx + 4].copy_from_slice(&px.0);
            }
        }
    }
    canvas
}

/// Checks if a PNG byte buffer contains EXIF or ancillary text/time metadata chunks.
pub fn has_png_metadata(png_bytes: &[u8]) -> bool {
    const PNG_SIG: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if png_bytes.len() < 8 || &png_bytes[..8] != PNG_SIG {
        return false;
    }
    let mut pos = 8;
    while pos + 12 <= png_bytes.len() {
        let len = u32::from_be_bytes([
            png_bytes[pos],
            png_bytes[pos + 1],
            png_bytes[pos + 2],
            png_bytes[pos + 3],
        ]) as usize;
        let chunk_type = &png_bytes[pos + 4..pos + 8];
        if matches!(chunk_type, b"eXIf" | b"tEXt" | b"zTXt" | b"iTXt" | b"tIME") {
            return true;
        }
        pos = pos.saturating_add(12).saturating_add(len);
    }
    false
}

/// Directly strips `eXIf`, `tEXt`, `zTXt`, `iTXt`, and `tIME` chunks from a PNG byte stream.
pub fn strip_png_metadata_chunks(png_bytes: &[u8]) -> Option<Vec<u8>> {
    const PNG_SIG: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if png_bytes.len() < 8 || &png_bytes[..8] != PNG_SIG {
        return None;
    }
    let mut out = Vec::with_capacity(png_bytes.len());
    out.extend_from_slice(PNG_SIG);
    let mut pos = 8;
    let mut stripped_any = false;

    while pos + 12 <= png_bytes.len() {
        let len = u32::from_be_bytes([
            png_bytes[pos],
            png_bytes[pos + 1],
            png_bytes[pos + 2],
            png_bytes[pos + 3],
        ]) as usize;
        let total_chunk = 12usize.checked_add(len)?;
        if pos + total_chunk > png_bytes.len() {
            return None;
        }
        let chunk_type = &png_bytes[pos + 4..pos + 8];
        if matches!(chunk_type, b"eXIf" | b"tEXt" | b"zTXt" | b"iTXt" | b"tIME") {
            stripped_any = true;
        } else {
            out.extend_from_slice(&png_bytes[pos..pos + total_chunk]);
        }
        pos += total_chunk;
    }

    if stripped_any {
        Some(out)
    } else {
        None
    }
}

/// Losslessly strips EXIF (`APP1`), ICC/XMP (`APP2`), IPTC (`APP13`), and comment (`COM`)
/// segments from a JPEG byte stream without re-encoding image DCT blocks.
pub fn strip_jpeg_exif(jpeg_bytes: &[u8]) -> Option<Vec<u8>> {
    if jpeg_bytes.len() < 4 || jpeg_bytes[0] != 0xFF || jpeg_bytes[1] != 0xD8 {
        return None;
    }
    let mut out = Vec::with_capacity(jpeg_bytes.len());
    out.extend_from_slice(&[0xFF, 0xD8]);
    let mut pos = 2;
    let mut stripped_any = false;

    while pos + 4 <= jpeg_bytes.len() {
        if jpeg_bytes[pos] != 0xFF {
            return None;
        }
        let marker = jpeg_bytes[pos + 1];
        // SOS (Start of Scan) or EOI: copy remaining entropy-coded image stream verbatim
        if marker == 0xDA || marker == 0xD9 {
            out.extend_from_slice(&jpeg_bytes[pos..]);
            break;
        }
        // Standalone markers without length
        if marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            out.extend_from_slice(&jpeg_bytes[pos..pos + 2]);
            pos += 2;
            continue;
        }
        let seg_len = u16::from_be_bytes([jpeg_bytes[pos + 2], jpeg_bytes[pos + 3]]) as usize;
        if seg_len < 2 || pos + 2 + seg_len > jpeg_bytes.len() {
            return None;
        }
        // Strip APP1 (EXIF/XMP), APP2, APP12, APP13 (Photoshop/IPTC), and COM (0xFE)
        if matches!(marker, 0xE1 | 0xE2 | 0xEC | 0xED | 0xFE) {
            stripped_any = true;
        } else {
            out.extend_from_slice(&jpeg_bytes[pos..pos + 2 + seg_len]);
        }
        pos += 2 + seg_len;
    }

    if stripped_any {
        Some(out)
    } else {
        None
    }
}

/// Checks if the image should be downscaled according to `scale_factor`, `max_width`, or `max_height`.
fn maybe_downscale(
    width: u32,
    height: u32,
    rgba_bytes: &[u8],
    config: &AppConfig,
) -> Result<(u32, u32, Vec<u8>, bool)> {
    let mut target_w = width as f64;
    let mut target_h = height as f64;

    if let Some(scale) = config.scale_factor {
        if scale > 1.0 {
            target_w /= scale as f64;
            target_h /= scale as f64;
        }
    }

    if let Some(max_w) = config.max_width {
        if target_w > max_w as f64 && max_w > 0 {
            let ratio = max_w as f64 / target_w;
            target_w = max_w as f64;
            target_h *= ratio;
        }
    }

    if let Some(max_height) = config.max_height {
        if target_h > max_height as f64 && max_height > 0 {
            let ratio = max_height as f64 / target_h;
            target_h = max_height as f64;
            target_w *= ratio;
        }
    }

    let final_w = (target_w.round() as u32).max(1);
    let final_h = (target_h.round() as u32).max(1);

    if final_w == width && final_h == height {
        return Ok((width, height, rgba_bytes.to_vec(), false));
    }

    let src_img = RgbaImage::from_raw(width, height, rgba_bytes.to_vec())
        .context("Failed to construct RgbaImage for resizing")?;
    let resized = image::imageops::resize(&src_img, final_w, final_h, FilterType::Lanczos3);
    Ok((final_w, final_h, resized.into_raw(), true))
}

/// Quantizes 32-bit RGBA pixels into an 8-bit indexed palette PNG using `imagequant`
/// and then optimizes deflate streams with `oxipng`.
fn quantize_and_oxipng(
    width: u32,
    height: u32,
    rgba_bytes: &[u8],
    config: &AppConfig,
) -> Result<Vec<u8>> {
    let mut liq = imagequant::new();
    let q_min = config.quality_min.min(config.quality_max).min(100);
    let q_max = config.quality_max.min(100).max(q_min);
    liq.set_quality(q_min, q_max)
        .context("Failed to configure imagequant quality range")?;
    liq.set_speed(4)
        .context("Failed to configure imagequant speed")?;

    #[allow(clippy::chunks_exact_to_as_chunks)]
    let pixels: Vec<imagequant::RGBA> = rgba_bytes
        .chunks_exact(4)
        .map(|px| imagequant::RGBA::new(px[0], px[1], px[2], px[3]))
        .collect();

    let mut img = liq
        .new_image(pixels, width as usize, height as usize, 0.0)
        .context("Failed to create imagequant image buffer")?;

    let mut res = liq
        .quantize(&mut img)
        .context("Image quantization failed to meet quality threshold")?;

    res.set_dithering_level(0.8)
        .context("Failed to set dithering level")?;

    let (palette, indexed_pixels) = res
        .remapped(&mut img)
        .context("Failed to remap pixels to quantized palette")?;

    let indexed_png = encode_indexed_png(width, height, &palette, &indexed_pixels)?;
    run_oxipng(&indexed_png, config)
}

/// Encodes an 8-bit indexed palette PNG with RGB `PLTE` and alpha `tRNS` chunks.
fn encode_indexed_png(
    width: u32,
    height: u32,
    palette: &[imagequant::RGBA],
    indices: &[u8],
) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(indices.len() / 2 + 1024);
    {
        let mut encoder = png::Encoder::new(&mut buf, width, height);
        encoder.set_color(png::ColorType::Indexed);
        encoder.set_depth(png::BitDepth::Eight);

        let mut plte = Vec::with_capacity(palette.len() * 3);
        let mut trns = Vec::with_capacity(palette.len());
        let mut has_transparency = false;

        for color in palette {
            plte.push(color.r);
            plte.push(color.g);
            plte.push(color.b);
            trns.push(color.a);
            if color.a < 255 {
                has_transparency = true;
            }
        }

        encoder.set_palette(plte);
        if has_transparency {
            encoder.set_trns(trns);
        }

        let mut writer = encoder
            .write_header()
            .context("Failed to write indexed PNG header")?;
        writer
            .write_image_data(indices)
            .context("Failed to write indexed PNG image data")?;
    }
    Ok(buf)
}

/// Encodes raw 32-bit RGBA pixels into a baseline PNG in memory.
pub fn encode_rgba_png(width: u32, height: u32, rgba_bytes: &[u8]) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(rgba_bytes.len() / 2 + 1024);
    {
        let mut encoder = png::Encoder::new(&mut buf, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .context("Failed to write RGBA PNG header")?;
        writer
            .write_image_data(rgba_bytes)
            .context("Failed to write RGBA PNG data")?;
    }
    Ok(buf)
}

/// Checks the PNG `IHDR` header to see if the image is already an 8-bit indexed-palette PNG.
fn is_indexed_png(png_bytes: &[u8]) -> bool {
    let decoder = png::Decoder::new(Cursor::new(png_bytes));
    if let Ok(reader) = decoder.read_info() {
        return reader.info().color_type == png::ColorType::Indexed;
    }
    false
}

/// Decodes a PNG byte buffer into raw 32-bit RGBA pixels.
fn decode_png_to_rgba(png_bytes: &[u8]) -> Result<Vec<u8>> {
    let decoder = png::Decoder::new(Cursor::new(png_bytes));
    let reader = decoder.read_info()?;
    let info = reader.info();
    let width = info.width;
    let height = info.height;
    drop(reader);

    let img = image::load_from_memory_with_format(png_bytes, ImageFormat::Png)?.into_rgba8();
    if img.width() == width && img.height() == height {
        Ok(img.into_raw())
    } else {
        bail!("Decoded dimensions mismatch")
    }
}

/// Runs `oxipng` lossless optimization in-memory.
fn run_oxipng(png_bytes: &[u8], config: &AppConfig) -> Result<Vec<u8>> {
    let mut opts = Options::from_preset(config.oxipng_level.min(6));
    if config.strip_metadata {
        opts.strip = StripChunks::Safe;
    } else {
        opts.strip = StripChunks::None;
    }
    if config.oxipng_level < 6 {
        opts.deflate = Deflaters::Libdeflater {
            compression: (config.oxipng_level.clamp(1, 5) * 2 + 2).min(12),
        };
    }

    let optimized = oxipng::optimize_from_memory(png_bytes, &opts)
        .context("oxipng in-memory optimization failed")?;

    if optimized.len() < png_bytes.len() {
        Ok(optimized)
    } else {
        Ok(png_bytes.to_vec())
    }
}

/// Formats a byte count into a human-readable string (e.g., `142.5 KB`, `1.8 MB`).
pub fn format_bytes(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    if bytes as f64 >= MB {
        format!("{:.2} MB", bytes as f64 / MB)
    } else if bytes as f64 >= KB {
        format!("{:.1} KB", bytes as f64 / KB)
    } else {
        format!("{} B", bytes)
    }
}

#[allow(dead_code)]
fn _unused_outfile_imports() -> (InFile, OutFile) {
    (InFile::StdIn, OutFile::StdOut)
}
