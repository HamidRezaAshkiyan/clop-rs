use crate::clipboard::OptimizationSummary;
use crate::config::AppConfig;
use crate::optimizer::{self, format_bytes, generate_thumbnail, DiskBackup};
use crate::{pdf, video};
use anyhow::Result;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Newly arrived image, PDF, or video file in the Downloads directory ready for optimization.
pub struct PendingDownloadFile {
    pub path: PathBuf,
    pub file_name: String,
    pub original_size: usize,
    pub width: u32,
    pub height: u32,
    pub thumbnail_64: Vec<u8>,
}

/// Watches the user's configured directories (Downloads + any `extra_watch_dirs`) for newly added
/// image, PDF, and video files and optimizes them automatically.
pub struct DownloadsWatcher {
    watched_dirs: Vec<PathBuf>,
    seen_files: HashMap<PathBuf, (SystemTime, u64)>,
    pub last_backup: Option<DiskBackup>,
}

impl Default for DownloadsWatcher {
    fn default() -> Self {
        Self::new(None)
    }
}

impl DownloadsWatcher {
    pub fn new(dir: Option<PathBuf>) -> Self {
        let mut watcher = Self {
            watched_dirs: Vec::new(),
            seen_files: HashMap::new(),
            last_backup: None,
        };
        if let Some(d) = dir {
            watcher.set_directory(Some(d));
        }
        watcher
    }

    /// Updates the watched directory to a single optional directory and snapshots existing files.
    pub fn set_directory(&mut self, dir: Option<PathBuf>) {
        let dirs = dir.into_iter().collect();
        self.set_directories(dirs);
    }

    /// Updates the set of watched directories and snapshots existing files so only *new* or
    /// *newly modified* files arriving after enabling the watcher are optimized.
    pub fn set_directories(&mut self, dirs: Vec<PathBuf>) {
        if self.watched_dirs == dirs {
            return;
        }
        self.watched_dirs = dirs;
        self.seen_files.clear();
        let to_snapshot = self.watched_dirs.clone();
        for d in to_snapshot {
            self.snapshot_existing(&d);
        }
    }

    fn snapshot_existing(&mut self, dir: &Path) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if is_supported_media_path(&path) {
                if let Ok(meta) = entry.metadata() {
                    if let Ok(mtime) = meta.modified() {
                        self.seen_files.insert(path, (mtime, meta.len()));
                    }
                }
            }
        }
    }

    /// Scans all watched directories for the next new unoptimized image, PDF, or video file.
    pub fn inspect_next_file(&mut self, config: &AppConfig) -> Option<PendingDownloadFile> {
        if !config.watch_downloads {
            return None;
        }
        let target_dirs = config.all_watch_dirs();
        if target_dirs.is_empty() {
            return None;
        }
        if self.watched_dirs != target_dirs {
            self.set_directories(target_dirs.clone());
        }

        let now = SystemTime::now();

        for target_dir in &target_dirs {
            let Ok(entries) = fs::read_dir(target_dir) else {
                continue;
            };

            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file()
                    || !is_supported_media_path(&path)
                    || config.is_path_ignored(&path)
                {
                    continue;
                }
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                let len = meta.len();
                if len == 0 || len > 500 * 1024 * 1024 {
                    continue;
                }
                let Ok(mtime) = meta.modified() else {
                    continue;
                };

                // Wait at least 300ms after the last write so browser downloads finish flushing
                if let Ok(age) = now.duration_since(mtime) {
                    if age < Duration::from_millis(300) {
                        continue;
                    }
                }

                if let Some(&(prev_mtime, prev_len)) = self.seen_files.get(&path) {
                    if prev_mtime == mtime && prev_len == len {
                        continue;
                    }
                }

                // Mark seen before reading so unreadable/corrupt files aren't retried in a tight loop
                self.seen_files.insert(path.clone(), (mtime, len));

                let (width, height, thumbnail_64) = if pdf::is_pdf_path(&path) {
                    (0, 0, pdf::procedural_pdf_icon_64())
                } else if video::is_video_path(&path) {
                    (0, 0, video::extract_video_thumbnail_64(&path))
                } else {
                    let Ok(decoded) = image::open(&path) else {
                        continue;
                    };
                    let rgba = decoded.into_rgba8();
                    let (w, h) = rgba.dimensions();
                    let thumb = generate_thumbnail(w, h, rgba.as_raw(), 64);
                    (w, h, thumb)
                };

                let file_name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("Download")
                    .to_string();

                return Some(PendingDownloadFile {
                    path,
                    file_name,
                    original_size: len as usize,
                    width,
                    height,
                    thumbnail_64,
                });
            }
        }

        None
    }

    /// Optimizes a pending file from the Downloads directory and stores a disk-backed backup for one-click Revert.
    pub fn optimize_pending_file(
        &mut self,
        pending: PendingDownloadFile,
        config: &AppConfig,
    ) -> Result<Option<OptimizationSummary>> {
        let backup = DiskBackup::create(&pending.path).ok();
        let res = optimizer::optimize_file(&pending.path, None, config)?;
        if !res.was_modified {
            // `backup` drops here and removes its cached copy automatically
            return Ok(None);
        }

        // Update seen metadata so we don't re-trigger on our own write
        if let Ok(meta) = fs::metadata(&pending.path) {
            if let Ok(mtime) = meta.modified() {
                self.seen_files
                    .insert(pending.path.clone(), (mtime, meta.len()));
            }
        }

        self.last_backup = backup;

        let summary = OptimizationSummary {
            source_label: pending.file_name,
            width: res.width,
            height: res.height,
            original_size: res.original_size,
            optimized_size: res.optimized_size,
            saved_bytes: res.saved_bytes(),
            saved_percent: res.saved_percent(),
            exif_stripped: res.exif_stripped,
            elapsed: res.elapsed,
            thumbnail_64: pending.thumbnail_64,
        };

        log::info!(
            "Optimized Downloads file '{}': {}",
            summary.source_label,
            summary.display_short()
        );

        Ok(Some(summary))
    }

    /// Reverts the last optimized file in the Downloads directory back to its original unoptimized bytes.
    pub fn revert_last(&mut self) -> Result<Option<(PathBuf, usize)>> {
        let Some(backup) = self.last_backup.take() else {
            return Ok(None);
        };
        let (path, orig_len) = backup.restore()?;

        if let Ok(meta) = fs::metadata(&path) {
            if let Ok(mtime) = meta.modified() {
                self.seen_files.insert(path.clone(), (mtime, meta.len()));
            }
        }

        log::info!(
            "Reverted Downloads file '{}' to original ({})",
            path.display(),
            format_bytes(orig_len)
        );
        Ok(Some((path, orig_len)))
    }
}

pub fn is_supported_media_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if name.starts_with('.')
        || name.ends_with(".crdownload")
        || name.ends_with(".part")
        || name.ends_with(".download")
        || name.ends_with(".tmp")
    {
        return false;
    }
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "png"
            | "jpg"
            | "jpeg"
            | "webp"
            | "bmp"
            | "tiff"
            | "pdf"
            | "mp4"
            | "mov"
            | "mkv"
            | "webm"
            | "m4v"
            | "avi"
    )
}
