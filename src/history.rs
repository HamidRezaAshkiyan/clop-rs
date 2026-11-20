use crate::clipboard::OptimizationSummary;
use crate::config::AppConfig;
use crate::optimizer::encode_rgba_png;
use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_HISTORY_ENTRIES: usize = 200;

/// Origin of an optimization history entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HistorySource {
    Clipboard,
    Downloads,
    File,
    Cli,
}

impl HistorySource {
    pub fn as_str(&self) -> &'static str {
        match self {
            HistorySource::Clipboard => "clipboard",
            HistorySource::Downloads => "downloads",
            HistorySource::File => "file",
            HistorySource::Cli => "cli",
        }
    }
}

/// A single recorded optimization event with size metrics and optional thumbnail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: u64,
    pub timestamp_unix: u64,
    pub source: HistorySource,
    pub label: String,
    pub file_path: Option<String>,
    pub width: u32,
    pub height: u32,
    pub original_size: usize,
    pub optimized_size: usize,
    pub saved_bytes: usize,
    pub saved_percent: f64,
    pub exif_stripped: bool,
    pub elapsed_ms: u64,
    pub preset_label: String,
    /// Optional base64-encoded 64x64 PNG data URI (`data:image/png;base64,...`) for UI previews.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail_data_url: Option<String>,
}

/// Aggregate savings across a set of history entries.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HistoryStats {
    pub total_items: usize,
    pub clipboard_items: usize,
    pub file_items: usize,
    pub total_original_bytes: u64,
    pub total_optimized_bytes: u64,
    pub total_saved_bytes: u64,
    pub overall_saved_percent: f64,
}

/// Persistent optimization history stored in `~/.config/clop-rs/history.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OptimizationHistory {
    pub entries: Vec<HistoryEntry>,
    #[serde(default)]
    next_id: u64,
}

impl OptimizationHistory {
    /// Returns the platform-appropriate history file path.
    pub fn history_path() -> Result<PathBuf> {
        let base = dirs::config_dir().context("Could not determine OS configuration directory")?;
        Ok(base.join("clop-rs").join("history.json"))
    }

    /// Loads history from disk, returning an empty history if the file doesn't exist yet.
    pub fn load() -> Self {
        Self::history_path()
            .ok()
            .and_then(|p| Self::load_from_path(&p).ok())
            .unwrap_or_default()
    }

    /// Loads history from an explicit path (useful for tests).
    pub fn load_from_path(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path)
            .with_context(|| format!("Failed to read history file at {}", path.display()))?;
        let mut hist: Self = serde_json::from_str(&raw)
            .with_context(|| format!("Failed to parse history JSON at {}", path.display()))?;
        let max_id = hist.entries.iter().map(|e| e.id).max().unwrap_or(0);
        if hist.next_id <= max_id {
            hist.next_id = max_id + 1;
        }
        Ok(hist)
    }

    /// Persists history to the default OS config path.
    pub fn save(&self) -> Result<PathBuf> {
        let path = Self::history_path()?;
        self.save_to_path(&path)?;
        Ok(path)
    }

    /// Persists history to an explicit path.
    pub fn save_to_path(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        fs::write(path, json)?;
        Ok(())
    }

    /// Records a new optimization event at the front (newest first) and trims to `MAX_HISTORY_ENTRIES`.
    pub fn record_summary(
        &mut self,
        source: HistorySource,
        summary: &OptimizationSummary,
        file_path: Option<&Path>,
        config: &AppConfig,
    ) -> HistoryEntry {
        self.next_id = self.next_id.max(1);
        let id = self.next_id;
        self.next_id += 1;

        let timestamp_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let thumbnail_data_url = encode_thumbnail_data_url(&summary.thumbnail_64);

        let entry = HistoryEntry {
            id,
            timestamp_unix,
            source,
            label: summary.source_label.clone(),
            file_path: file_path.map(|p| p.display().to_string()),
            width: summary.width,
            height: summary.height,
            original_size: summary.original_size,
            optimized_size: summary.optimized_size,
            saved_bytes: summary.saved_bytes,
            saved_percent: summary.saved_percent,
            exif_stripped: summary.exif_stripped,
            elapsed_ms: summary.elapsed.as_millis() as u64,
            preset_label: config.preset_summary_label(),
            thumbnail_data_url,
        };

        self.entries.insert(0, entry.clone());
        if self.entries.len() > MAX_HISTORY_ENTRIES {
            self.entries.truncate(MAX_HISTORY_ENTRIES);
        }
        entry
    }

    /// Clears all history entries.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Returns aggregate statistics across all stored entries.
    pub fn stats(&self) -> HistoryStats {
        let mut total_original = 0u64;
        let mut total_optimized = 0u64;
        let mut clipboard_items = 0usize;
        let mut file_items = 0usize;

        for e in &self.entries {
            total_original += e.original_size as u64;
            total_optimized += e.optimized_size as u64;
            if e.source == HistorySource::Clipboard {
                clipboard_items += 1;
            } else {
                file_items += 1;
            }
        }

        let total_saved = total_original.saturating_sub(total_optimized);
        let overall_saved_percent = if total_original > 0 {
            (total_saved as f64 / total_original as f64) * 100.0
        } else {
            0.0
        };

        HistoryStats {
            total_items: self.entries.len(),
            clipboard_items,
            file_items,
            total_original_bytes: total_original,
            total_optimized_bytes: total_optimized,
            total_saved_bytes: total_saved,
            overall_saved_percent,
        }
    }
}

/// Encodes a 64x64 RGBA thumbnail buffer into a compact `data:image/png;base64,...` URL.
pub fn encode_thumbnail_data_url(rgba_64: &[u8]) -> Option<String> {
    if rgba_64.len() != 64 * 64 * 4 {
        return None;
    }
    // Skip encoding completely transparent/empty thumbnails
    if rgba_64.iter().all(|&b| b == 0) {
        return None;
    }
    let png_bytes = encode_rgba_png(64, 64, rgba_64).ok()?;
    Some(format!(
        "data:image/png;base64,{}",
        BASE64.encode(&png_bytes)
    ))
}
