use crate::config::AppConfig;
use crate::optimizer::{self, encode_rgba_png, format_bytes, generate_thumbnail};
use crate::platform;
use anyhow::{Context, Result};
use arboard::{Clipboard, ImageData};
use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

/// Summary of a single optimization event (clipboard or file).
#[derive(Debug, Clone)]
pub struct OptimizationSummary {
    pub source_label: String,
    pub width: u32,
    pub height: u32,
    pub original_size: usize,
    pub optimized_size: usize,
    pub saved_bytes: usize,
    pub saved_percent: f64,
    pub exif_stripped: bool,
    pub elapsed: Duration,
    /// 64x64 RGBA thumbnail for the Floating HUD Panel.
    pub thumbnail_64: Vec<u8>,
}

impl OptimizationSummary {
    /// Human-readable one-line status string, e.g. `412.0 KB -> 108.4 KB (-73.7%, 42ms)`.
    pub fn display_short(&self) -> String {
        let exif_tag = if self.exif_stripped {
            " [EXIF stripped]"
        } else {
            ""
        };
        format!(
            "{} -> {} (-{:.1}%, {}ms){}",
            format_bytes(self.original_size),
            format_bytes(self.optimized_size),
            self.saved_percent,
            self.elapsed.as_millis(),
            exif_tag
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

/// Backup of the original unoptimized clipboard image so the user can click "Revert".
#[derive(Debug, Clone)]
pub struct ClipboardBackup {
    pub width: u32,
    pub height: u32,
    pub original_rgba: Vec<u8>,
    pub original_png: Option<Vec<u8>>,
    pub original_size: usize,
}

/// New clipboard image detected and ready to be optimized.
pub struct PendingClipboardImage {
    pub width: u32,
    pub height: u32,
    pub rgba_bytes: Vec<u8>,
    pub raw_png: Option<Vec<u8>>,
    pub estimated_size: usize,
    pub thumbnail_64: Vec<u8>,
}

/// Stateful cross-platform clipboard monitor with loop-prevention hash tracking and Revert support.
pub struct ClipboardMonitor {
    clipboard: Option<Clipboard>,
    last_change_count: Option<i64>,
    seen_rgba_hashes: [u64; 8],
    seen_png_hashes: [u64; 8],
    hash_cursor: usize,
    pub last_backup: Option<ClipboardBackup>,
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
            seen_rgba_hashes: [0; 8],
            seen_png_hashes: [0; 8],
            hash_cursor: 0,
            last_backup: None,
            stats: SessionStats::default(),
        }
    }

    /// Step 1: Checks if a new unoptimized image is on the clipboard.
    /// If found, returns `Some(PendingClipboardImage)` so callers (like the Floating HUD)
    /// can immediately display the "Optimizing..." progress bar before heavy compression runs.
    pub fn inspect_new_image(&mut self, force: bool) -> Option<PendingClipboardImage> {
        let current_change = platform::clipboard_change_count();
        if !force {
            if let (Some(curr), Some(prev)) = (current_change, self.last_change_count) {
                if curr == prev {
                    return None;
                }
            }
        }
        self.last_change_count = current_change;

        let raw_png = platform::read_raw_clipboard_png();
        if let Some(ref png_bytes) = raw_png {
            let png_hash = compute_bytes_hash(png_bytes);
            if !force && self.is_seen_png_hash(png_hash) {
                return None;
            }
        }

        if self.clipboard.is_none() {
            self.clipboard = Clipboard::new().ok();
        }

        let (width, height, rgba_bytes) = self.read_clipboard_rgba(&raw_png)?;
        let input_rgba_hash = compute_image_hash(width, height, &rgba_bytes);
        if !force && self.is_seen_rgba_hash(input_rgba_hash) {
            return None;
        }

        self.record_rgba_hash(input_rgba_hash);
        if let Some(ref png_bytes) = raw_png {
            self.record_png_hash(compute_bytes_hash(png_bytes));
        }

        let estimated_size = raw_png
            .as_ref()
            .map(|b| b.len())
            .unwrap_or_else(|| (width as usize * height as usize).saturating_mul(2));
        let thumbnail_64 = generate_thumbnail(width, height, &rgba_bytes, 64);

        Some(PendingClipboardImage {
            width,
            height,
            rgba_bytes,
            raw_png,
            estimated_size,
            thumbnail_64,
        })
    }

    /// Step 2: Runs the optimization pipeline on a `PendingClipboardImage` and updates the clipboard.
    pub fn optimize_pending(
        &mut self,
        pending: PendingClipboardImage,
        config: &AppConfig,
    ) -> Result<Option<OptimizationSummary>> {
        let optimized = optimizer::optimize_rgba(
            pending.width,
            pending.height,
            &pending.rgba_bytes,
            pending.raw_png.as_deref(),
            config,
        )
        .context("Clipboard image optimization failed")?;

        if !optimized.was_modified {
            log::debug!(
                "Clipboard image ({}x{}, {}) is already optimal",
                pending.width,
                pending.height,
                format_bytes(optimized.original_size)
            );
            return Ok(None);
        }

        // Save backup of original unoptimized image for one-click Revert
        let original_png = pending
            .raw_png
            .or_else(|| encode_rgba_png(pending.width, pending.height, &pending.rgba_bytes).ok());
        self.last_backup = Some(ClipboardBackup {
            width: pending.width,
            height: pending.height,
            original_rgba: pending.rgba_bytes,
            original_png,
            original_size: optimized.original_size,
        });

        self.write_optimized_to_clipboard(&optimized)?;

        let output_rgba_hash =
            compute_image_hash(optimized.width, optimized.height, &optimized.rgba_bytes);
        let output_png_hash = compute_bytes_hash(&optimized.png_bytes);
        self.record_rgba_hash(output_rgba_hash);
        self.record_png_hash(output_png_hash);
        self.last_change_count = platform::clipboard_change_count();

        let summary = OptimizationSummary {
            source_label: "Clipboard".to_string(),
            width: optimized.width,
            height: optimized.height,
            original_size: optimized.original_size,
            optimized_size: optimized.optimized_size,
            saved_bytes: optimized.saved_bytes(),
            saved_percent: optimized.saved_percent(),
            exif_stripped: optimized.exif_stripped,
            elapsed: optimized.elapsed,
            thumbnail_64: pending.thumbnail_64,
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

    /// Checks and optimizes the clipboard in a single call.
    pub fn poll_once(
        &mut self,
        config: &AppConfig,
        force: bool,
    ) -> Result<Option<OptimizationSummary>> {
        let Some(pending) = self.inspect_new_image(force) else {
            return Ok(None);
        };
        self.optimize_pending(pending, config)
    }

    /// Reverts the last clipboard optimization, restoring the exact original unoptimized image
    /// back to the system clipboard and marking its hash as seen so it won't be re-optimized.
    pub fn revert_last(&mut self) -> Result<Option<usize>> {
        let Some(backup) = self.last_backup.take() else {
            return Ok(None);
        };

        let rgba_hash = compute_image_hash(backup.width, backup.height, &backup.original_rgba);
        self.record_rgba_hash(rgba_hash);
        if let Some(ref png_bytes) = backup.original_png {
            self.record_png_hash(compute_bytes_hash(png_bytes));
        }

        #[cfg(target_os = "macos")]
        {
            if let Some(ref png_bytes) = backup.original_png {
                if platform::write_raw_clipboard_png(png_bytes).unwrap_or(false) {
                    self.last_change_count = platform::clipboard_change_count();
                    log::info!(
                        "Reverted clipboard image to original ({})",
                        format_bytes(backup.original_size)
                    );
                    return Ok(Some(backup.original_size));
                }
            }
        }

        if let Some(ref mut cb) = self.clipboard {
            let img_data = ImageData {
                width: backup.width as usize,
                height: backup.height as usize,
                bytes: Cow::Borrowed(&backup.original_rgba),
            };
            cb.set_image(img_data)
                .context("Failed to restore original image to clipboard")?;
        }

        #[cfg(target_os = "windows")]
        {
            if let Some(ref png_bytes) = backup.original_png {
                let _ = platform::write_raw_clipboard_png(png_bytes);
            }
        }

        self.last_change_count = platform::clipboard_change_count();
        log::info!(
            "Reverted clipboard image to original ({})",
            format_bytes(backup.original_size)
        );
        Ok(Some(backup.original_size))
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
        let len = self.seen_rgba_hashes.len();
        self.seen_rgba_hashes[self.hash_cursor % len] = hash;
        self.hash_cursor = self.hash_cursor.wrapping_add(1);
    }

    pub fn record_png_hash(&mut self, hash: u64) {
        let len = self.seen_png_hashes.len();
        self.seen_png_hashes[self.hash_cursor % len] = hash;
    }
}

/// Computes a fast 64-bit hash of image dimensions and pixel data.
pub fn compute_image_hash(width: u32, height: u32, rgba_bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    width.hash(&mut hasher);
    height.hash(&mut hasher);
    rgba_bytes.len().hash(&mut hasher);

    if rgba_bytes.len() <= 65_536 {
        rgba_bytes.hash(&mut hasher);
    } else {
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
