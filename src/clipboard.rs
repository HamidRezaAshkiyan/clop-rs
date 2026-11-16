use crate::config::AppConfig;
use crate::optimizer::{self, format_bytes};
use crate::platform;
use anyhow::{Context, Result};
use arboard::{Clipboard, ImageData};
use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

/// Summary of a single clipboard optimization event.
#[derive(Debug, Clone)]
pub struct OptimizationSummary {
    pub width: u32,
    pub height: u32,
    pub original_size: usize,
    pub optimized_size: usize,
    pub saved_bytes: usize,
    pub saved_percent: f64,
    pub elapsed: Duration,
}

impl OptimizationSummary {
    /// Human-readable one-line status string, e.g. `412.0 KB -> 108.4 KB (-73.7%, 42ms)`.
    pub fn display_short(&self) -> String {
        format!(
            "{} -> {} (-{:.1}%, {}ms)",
            format_bytes(self.original_size),
            format_bytes(self.optimized_size),
            self.saved_percent,
            self.elapsed.as_millis()
        )
    }
}

/// Cumulative statistics across the running session.
#[derive(Debug, Clone, Default)]
pub struct SessionStats {
    pub images_optimized: u64,
    pub total_original_bytes: u64,
    pub total_optimized_bytes: u64,
    pub last_summary: Option<OptimizationSummary>,
}

impl SessionStats {
    pub fn total_saved_bytes(&self) -> u64 {
        self.total_original_bytes
            .saturating_sub(self.total_optimized_bytes)
    }

    pub fn total_saved_percent(&self) -> f64 {
        if self.total_original_bytes == 0 || self.total_optimized_bytes >= self.total_original_bytes
        {
            return 0.0;
        }
        ((self.total_original_bytes - self.total_optimized_bytes) as f64
            / self.total_original_bytes as f64)
            * 100.0
    }
}

/// Stateful cross-platform clipboard monitor with loop-prevention hash tracking.
pub struct ClipboardMonitor {
    clipboard: Option<Clipboard>,
    last_change_count: Option<i64>,
    seen_rgba_hashes: [u64; 4],
    seen_png_hashes: [u64; 4],
    hash_cursor: usize,
    pub stats: SessionStats,
}

impl Default for ClipboardMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipboardMonitor {
    pub fn new() -> Self {
        let clipboard = Clipboard::new().ok();
        Self {
            clipboard,
            last_change_count: None,
            seen_rgba_hashes: [0; 4],
            seen_png_hashes: [0; 4],
            hash_cursor: 0,
            stats: SessionStats::default(),
        }
    }

    /// Checks the clipboard once. If a new unoptimized image is found, optimizes it in-place
    /// on the clipboard and returns `Ok(Some(OptimizationSummary))`.
    ///
    /// When `force` is `true`, bypasses the change-counter and hash deduplication checks.
    pub fn poll_once(
        &mut self,
        config: &AppConfig,
        force: bool,
    ) -> Result<Option<OptimizationSummary>> {
        // Fast O(1) OS clipboard sequence check (zero allocations when idle)
        let current_change = platform::clipboard_change_count();
        if !force {
            if let (Some(curr), Some(prev)) = (current_change, self.last_change_count) {
                if curr == prev {
                    return Ok(None);
                }
            }
        }
        self.last_change_count = current_change;

        // 1. Check if raw PNG data is available on the clipboard
        let raw_png = platform::read_raw_clipboard_png();
        if let Some(ref png_bytes) = raw_png {
            let png_hash = compute_bytes_hash(png_bytes);
            if !force && self.is_seen_png_hash(png_hash) {
                return Ok(None);
            }
        }

        // 2. Read RGBA image pixels from clipboard (via arboard, or decode from raw_png)
        if self.clipboard.is_none() {
            self.clipboard = Clipboard::new().ok();
        }

        let (width, height, rgba_bytes) = match self.read_clipboard_rgba(&raw_png) {
            Some(img) => img,
            None => return Ok(None),
        };

        let input_rgba_hash = compute_image_hash(width, height, &rgba_bytes);
        if !force && self.is_seen_rgba_hash(input_rgba_hash) {
            return Ok(None);
        }

        // Record input hashes so we never re-process this exact image even if optimization is a no-op
        self.record_rgba_hash(input_rgba_hash);
        if let Some(ref png_bytes) = raw_png {
            self.record_png_hash(compute_bytes_hash(png_bytes));
        }

        // 3. Optimize the image in memory
        let optimized =
            optimizer::optimize_rgba(width, height, &rgba_bytes, raw_png.as_deref(), config)
                .context("Clipboard image optimization failed")?;

        if !optimized.was_modified {
            log::debug!(
                "Clipboard image ({}x{}, {}) is already optimal",
                width,
                height,
                format_bytes(optimized.original_size)
            );
            return Ok(None);
        }

        // 4. Write the optimized image back to the clipboard
        self.write_optimized_to_clipboard(&optimized)?;

        // 5. Record post-write hashes and updated OS change count to prevent self-triggering loops
        let output_rgba_hash =
            compute_image_hash(optimized.width, optimized.height, &optimized.rgba_bytes);
        let output_png_hash = compute_bytes_hash(&optimized.png_bytes);
        self.record_rgba_hash(output_rgba_hash);
        self.record_png_hash(output_png_hash);
        self.last_change_count = platform::clipboard_change_count();

        let summary = OptimizationSummary {
            width: optimized.width,
            height: optimized.height,
            original_size: optimized.original_size,
            optimized_size: optimized.optimized_size,
            saved_bytes: optimized.saved_bytes(),
            saved_percent: optimized.saved_percent(),
            elapsed: optimized.elapsed,
        };

        self.stats.images_optimized += 1;
        self.stats.total_original_bytes += summary.original_size as u64;
        self.stats.total_optimized_bytes += summary.optimized_size as u64;
        self.stats.last_summary = Some(summary.clone());

        log::info!(
            "Optimized clipboard image ({}x{}): {}",
            summary.width,
            summary.height,
            summary.display_short()
        );

        Ok(Some(summary))
    }

    fn read_clipboard_rgba(&mut self, raw_png: &Option<Vec<u8>>) -> Option<(u32, u32, Vec<u8>)> {
        if let Some(ref png_bytes) = raw_png {
            if let Ok(decoded) =
                image::load_from_memory_with_format(png_bytes, image::ImageFormat::Png)
            {
                let rgba = decoded.into_rgba8();
                let (w, h) = rgba.dimensions();
                return Some((w, h, rgba.into_raw()));
            }
        }

        let cb = self.clipboard.as_mut()?;
        let img = cb.get_image().ok()?;
        Some((img.width as u32, img.height as u32, img.bytes.into_owned()))
    }

    fn write_optimized_to_clipboard(
        &mut self,
        optimized: &optimizer::OptimizedImage,
    ) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            // On macOS, writing `public.png` directly to NSPasteboard preserves 100% of our
            // indexed palette + oxipng compression without `arboard` re-encoding to 32-bit PNG.
            if platform::write_raw_clipboard_png(&optimized.png_bytes).unwrap_or(false) {
                return Ok(());
            }
        }

        if let Some(ref mut cb) = self.clipboard {
            let img_data = ImageData {
                width: optimized.width as usize,
                height: optimized.height as usize,
                bytes: Cow::Borrowed(&optimized.rgba_bytes),
            };
            cb.set_image(img_data)
                .context("Failed to write optimized image via arboard")?;
        }

        #[cfg(target_os = "windows")]
        {
            // On Windows, also attach the registered `"PNG"` clipboard format alongside `CF_DIBV5`
            // so browsers, Slack, Discord, Teams, and Electron apps read the compressed PNG bytes.
            let _ = platform::write_raw_clipboard_png(&optimized.png_bytes);
        }

        Ok(())
    }

    pub fn is_seen_rgba_hash(&self, hash: u64) -> bool {
        hash != 0 && self.seen_rgba_hashes.contains(&hash)
    }

    pub fn is_seen_png_hash(&self, hash: u64) -> bool {
        hash != 0 && self.seen_png_hashes.contains(&hash)
    }

    pub fn record_rgba_hash(&mut self, hash: u64) {
        self.seen_rgba_hashes[self.hash_cursor % self.seen_rgba_hashes.len()] = hash;
        self.hash_cursor = self.hash_cursor.wrapping_add(1);
    }

    pub fn record_png_hash(&mut self, hash: u64) {
        self.seen_png_hashes[self.hash_cursor % self.seen_png_hashes.len()] = hash;
    }
}

/// Computes a fast 64-bit hash of image dimensions and pixel data.
/// For large images (>64 KB), samples header, footer, and strided blocks to stay sub-millisecond.
pub fn compute_image_hash(width: u32, height: u32, rgba_bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    width.hash(&mut hasher);
    height.hash(&mut hasher);
    rgba_bytes.len().hash(&mut hasher);

    if rgba_bytes.len() <= 65_536 {
        rgba_bytes.hash(&mut hasher);
    } else {
        // Hash first 8 KB, last 8 KB, and 256 evenly-spaced 64-byte slices across the image
        rgba_bytes[..8192].hash(&mut hasher);
        rgba_bytes[rgba_bytes.len() - 8192..].hash(&mut hasher);
        let step = (rgba_bytes.len() / 256).max(64);
        let mut offset = 8192;
        while offset + 64 <= rgba_bytes.len() {
            rgba_bytes[offset..offset + 64].hash(&mut hasher);
            offset += step;
        }
    }

    let h = hasher.finish();
    if h == 0 {
        1
    } else {
        h
    }
}

/// Computes a 64-bit hash of an arbitrary byte slice (such as compressed PNG bytes).
pub fn compute_bytes_hash(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.len().hash(&mut hasher);
    bytes.hash(&mut hasher);
    let h = hasher.finish();
    if h == 0 {
        1
    } else {
        h
    }
}
