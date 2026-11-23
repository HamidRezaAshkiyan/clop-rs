use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// Optimization strategy for images.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum OptimizationMode {
    /// Smart (Aggressive) mode: uses high-quality 8-bit perceptual palette quantization
    /// (`imagequant`) followed by `oxipng` deflate optimization. Achieves 60-85% reduction
    /// with virtually zero visible difference (just like Clop's default behavior).
    #[default]
    Smart,
    /// Strict lossless mode: uses `oxipng` only, preserving 100% bit-exact RGBA pixel values.
    Lossless,
}

/// High-level optimization preset balancing visual quality vs. file size.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum OptimizationPreset {
    /// 100% bit-exact lossless compression (largest file size, zero visual loss).
    Lossless,
    /// High visual fidelity with gentle compression (ideal for photography & design assets).
    HighQuality,
    /// Recommended balance of visual quality and strong space savings (60-80% smaller).
    #[default]
    Balanced,
    /// Maximum space savings with more aggressive compression (smallest file size).
    SmallestSize,
    /// User-defined quality/size slider value.
    Custom,
}

impl OptimizationPreset {
    pub fn display_name(&self) -> &'static str {
        match self {
            OptimizationPreset::Lossless => "Lossless",
            OptimizationPreset::HighQuality => "High Quality",
            OptimizationPreset::Balanced => "Balanced",
            OptimizationPreset::SmallestSize => "Smallest Size",
            OptimizationPreset::Custom => "Custom",
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            OptimizationPreset::Lossless => "lossless",
            OptimizationPreset::HighQuality => "high-quality",
            OptimizationPreset::Balanced => "balanced",
            OptimizationPreset::SmallestSize => "smallest-size",
            OptimizationPreset::Custom => "custom",
        }
    }
}

pub const CONFIG_SCHEMA_VERSION: u32 = 1;

fn default_schema_version() -> u32 {
    CONFIG_SCHEMA_VERSION
}

/// Persistent configuration stored in `~/Library/Application Support/clop-rs/config.toml` (macOS),
/// `~/.config/clop-rs/config.toml` (Linux), or `%APPDATA%\clop-rs\config.toml` (Windows).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// Configuration schema version for forward-compatible v1.x migrations.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Clipboard polling interval in milliseconds.
    pub poll_interval_ms: u64,
    /// Optimization mode: `smart` (lossy palette + lossless deflate) or `lossless`.
    pub mode: OptimizationMode,
    /// High-level quality vs. size preset (`lossless`, `high-quality`, `balanced`, `smallest-size`, `custom`).
    pub preset: OptimizationPreset,
    /// Unified quality-vs-size preference (10 = smallest size, 100 = highest quality).
    pub quality_level: u8,
    /// Minimum quality for `imagequant` in smart mode (0-100).
    pub quality_min: u8,
    /// Target maximum quality for `imagequant` in smart mode (0-100).
    pub quality_max: u8,
    /// `oxipng` optimization preset level (0-6). Level 2-3 offers the best speed/compression ratio.
    pub oxipng_level: u8,
    /// Optional maximum width (in pixels) to automatically downscale oversized images (e.g. 4K/Retina screenshots).
    pub max_width: Option<u32>,
    /// Optional maximum height (in pixels) to automatically downscale oversized images.
    pub max_height: Option<u32>,
    /// Optional downscale factor (e.g. 2 to halve Retina 2x screenshots to 1x).
    pub scale_factor: Option<f32>,
    /// Strip EXIF, GPS, camera metadata, and non-essential PNG/JPEG metadata chunks.
    pub strip_metadata: bool,
    /// Show the floating HUD panel with progress bar, size reduction, and Revert button.
    pub show_floating_hud: bool,
    /// Automatically watch and optimize new images saved to the user's Downloads folder.
    pub watch_downloads: bool,
    /// Optional custom directory to watch instead of the OS default Downloads directory.
    pub downloads_dir: Option<PathBuf>,
    /// Additional directories to watch alongside `downloads_dir` (e.g. `~/Desktop` for screenshots).
    #[serde(default)]
    pub extra_watch_dirs: Vec<PathBuf>,
    /// Case-insensitive wildcard filename patterns to skip during folder watching (e.g. `*-raw.*`, `secret_*`).
    #[serde(default)]
    pub ignore_patterns: Vec<String>,
    /// Run without a system tray / menu bar icon.
    pub headless: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            poll_interval_ms: 350,
            mode: OptimizationMode::Smart,
            preset: OptimizationPreset::Balanced,
            quality_level: 85,
            quality_min: 65,
            quality_max: 95,
            oxipng_level: 2,
            max_width: None,
            max_height: None,
            scale_factor: None,
            strip_metadata: true,
            show_floating_hud: true,
            watch_downloads: false,
            downloads_dir: None,
            extra_watch_dirs: Vec::new(),
            ignore_patterns: Vec::new(),
            headless: false,
        }
    }
}

impl AppConfig {
    /// Returns the platform-appropriate configuration file path.
    pub fn config_path() -> Result<PathBuf> {
        let base = dirs::config_dir().context("Could not determine OS configuration directory")?;
        Ok(base.join("clop-rs").join("config.toml"))
    }

    /// Loads configuration from disk if it exists, otherwise returns default config.
    pub fn load() -> Result<Self> {
        let path = Self::config_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read config file at {}", path.display()))?;
        let cfg: Self = toml::from_str(&raw)
            .with_context(|| format!("Failed to parse TOML config at {}", path.display()))?;
        Ok(cfg)
    }

    /// Saves the configuration atomically to disk.
    pub fn save(&self) -> Result<PathBuf> {
        let path = Self::config_path()?;
        let contents = toml::to_string_pretty(self)?;
        crate::optimizer::atomic_write_file(&path, contents.as_bytes())?;
        Ok(path)
    }

    /// Returns the effective primary Downloads folder to watch.
    pub fn effective_downloads_dir(&self) -> Option<PathBuf> {
        self.downloads_dir.clone().or_else(dirs::download_dir)
    }

    /// Returns all deduplicated directories to watch when folder auto-optimization is enabled.
    pub fn all_watch_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        if let Some(primary) = self.effective_downloads_dir() {
            dirs.push(primary);
        }
        for extra in &self.extra_watch_dirs {
            if !dirs.contains(extra) {
                dirs.push(extra.clone());
            }
        }
        dirs
    }

    /// Returns `true` if `path`'s filename matches any configured wildcard in `ignore_patterns`.
    pub fn is_path_ignored(&self, path: &std::path::Path) -> bool {
        if self.ignore_patterns.is_empty() {
            return false;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return false;
        };
        let name_lower = name.to_ascii_lowercase();
        self.ignore_patterns.iter().any(|pat| {
            let pat_lower = pat.trim().to_ascii_lowercase();
            !pat_lower.is_empty() && wildcard_match(&pat_lower, &name_lower)
        })
    }

    /// Applies a high-level quality-vs-size preset, updating `mode`, `quality_level`,
    /// `quality_min`, `quality_max`, and `oxipng_level`.
    pub fn apply_preset(&mut self, preset: OptimizationPreset) {
        self.preset = preset;
        match preset {
            OptimizationPreset::Lossless => {
                self.mode = OptimizationMode::Lossless;
                self.quality_level = 100;
                self.quality_min = 100;
                self.quality_max = 100;
                self.oxipng_level = 3;
            }
            OptimizationPreset::HighQuality => {
                self.mode = OptimizationMode::Smart;
                self.quality_level = 94;
                self.quality_min = 82;
                self.quality_max = 98;
                self.oxipng_level = 2;
            }
            OptimizationPreset::Balanced => {
                self.mode = OptimizationMode::Smart;
                self.quality_level = 85;
                self.quality_min = 65;
                self.quality_max = 95;
                self.oxipng_level = 2;
            }
            OptimizationPreset::SmallestSize => {
                self.mode = OptimizationMode::Smart;
                self.quality_level = 50;
                self.quality_min = 35;
                self.quality_max = 72;
                self.oxipng_level = 4;
            }
            OptimizationPreset::Custom => {
                // Keep existing custom parameters
                self.mode = OptimizationMode::Smart;
            }
        }
    }

    /// Applies a unified quality-vs-size slider value (`10` = smallest size, `100` = highest quality).
    pub fn apply_quality_level(&mut self, level: u8) {
        let clamped = level.clamp(10, 100);
        self.quality_level = clamped;
        self.mode = OptimizationMode::Smart;
        self.preset = OptimizationPreset::Custom;
        self.quality_max = clamped;
        self.quality_min = clamped.saturating_sub(25).max(10);
        self.oxipng_level = if clamped <= 55 {
            4
        } else if clamped <= 80 {
            3
        } else {
            2
        };
    }

    /// Effective JPEG encoder quality (35-96) derived from `quality_max`.
    pub fn effective_jpeg_quality(&self) -> u8 {
        self.quality_max.clamp(35, 96)
    }

    /// Effective embedded-JPEG quality inside PDFs (35-88) derived from `quality_max`.
    pub fn effective_pdf_jpeg_quality(&self) -> u8 {
        self.quality_max.saturating_sub(8).clamp(35, 88)
    }

    /// Effective Ghostscript `-dPDFSETTINGS` profile based on optimization mode & quality.
    pub fn effective_gs_pdf_setting(&self) -> &'static str {
        if self.mode == OptimizationMode::Lossless {
            return "/prepress";
        }
        match self.quality_max {
            92..=100 => "/printer",
            75..=91 => "/ebook",
            _ => "/screen",
        }
    }

    /// Effective FFmpeg `libx264` / `libvpx-vp9` CRF value (lower = higher quality, larger size).
    pub fn effective_video_crf(&self) -> &'static str {
        if self.mode == OptimizationMode::Lossless {
            return "18";
        }
        match self.quality_max {
            92..=100 => "21",
            80..=91 => "24",
            65..=79 => "27",
            _ => "30",
        }
    }

    /// Effective macOS `h264_videotoolbox` `-q:v` quality (1-100, higher = better quality).
    pub fn effective_videotoolbox_q(&self) -> &'static str {
        if self.mode == OptimizationMode::Lossless {
            return "75";
        }
        match self.quality_max {
            92..=100 => "65",
            80..=91 => "55",
            65..=79 => "45",
            _ => "38",
        }
    }

    /// Short human-readable label describing the active quality/size setting (e.g., `Balanced (Q95)` or `Lossless`).
    pub fn preset_summary_label(&self) -> String {
        if self.mode == OptimizationMode::Lossless || self.preset == OptimizationPreset::Lossless {
            "Lossless".to_string()
        } else {
            format!("{} (Q{})", self.preset.display_name(), self.quality_max)
        }
    }

    /// Applies CLI flag overrides on top of the loaded configuration.
    pub fn apply_cli(&mut self, cli: &Cli) {
        if let Some(interval) = cli.interval_ms {
            self.poll_interval_ms = interval.max(50);
        }
        if let Some(preset) = cli.preset {
            self.apply_preset(preset);
        }
        if cli.lossless {
            self.apply_preset(OptimizationPreset::Lossless);
        } else if let Some(mode) = cli.mode {
            self.mode = mode;
            if mode == OptimizationMode::Lossless {
                self.preset = OptimizationPreset::Lossless;
            }
        }
        if let Some(qmax) = cli.quality {
            self.apply_quality_level(qmax);
        }
        if let Some(level) = cli.oxipng_level {
            self.oxipng_level = level.min(6);
        }
        if let Some(mw) = cli.max_width {
            self.max_width = Some(mw);
        }
        if let Some(mh) = cli.max_height {
            self.max_height = Some(mh);
        }
        if let Some(sf) = cli.scale_factor {
            self.scale_factor = Some(sf);
        }
        if cli.strip_exif {
            self.strip_metadata = true;
        }
        if cli.keep_exif {
            self.strip_metadata = false;
        }
        if cli.watch_downloads {
            self.watch_downloads = true;
        }
        if let Some(ref dir) = cli.downloads_dir {
            self.downloads_dir = Some(dir.clone());
            self.watch_downloads = true;
        }
        if cli.no_hud {
            self.show_floating_hud = false;
        }
        if cli.headless {
            self.headless = true;
        }
    }
}

/// Command-line interface for `clop-rs`.
#[derive(Debug, Parser)]
#[command(
    name = "clop-rs",
    version,
    about = "Minimal-footprint cross-platform (macOS & Windows) clipboard & Downloads image optimizer inspired by Clop."
)]
pub struct Cli {
    /// Run as a pure headless background daemon without a system tray / menu bar icon.
    #[arg(long)]
    pub headless: bool,

    /// Disable the floating HUD panel notification.
    #[arg(long)]
    pub no_hud: bool,

    /// Optimize the current clipboard image once and exit immediately.
    #[arg(long, short = '1')]
    pub once: bool,

    /// Revert the last clipboard optimization (if backed up) or specify via tray/HUD.
    #[arg(long)]
    pub lossless: bool,

    /// Open the Dashboard / Settings UI window immediately on startup.
    #[arg(long)]
    pub open_ui: bool,

    /// Optimization quality-vs-size preset (`lossless`, `high-quality`, `balanced`, `smallest-size`).
    #[arg(long, short = 'p', value_enum)]
    pub preset: Option<OptimizationPreset>,

    /// Optimization mode (`smart` or `lossless`).
    #[arg(long, value_enum)]
    pub mode: Option<OptimizationMode>,

    /// Strip EXIF, GPS, and camera metadata chunks (enabled by default).
    #[arg(long)]
    pub strip_exif: bool,

    /// Preserve EXIF and metadata chunks during optimization.
    #[arg(long)]
    pub keep_exif: bool,

    /// Automatically watch and optimize new images in the Downloads folder.
    #[arg(long)]
    pub watch_downloads: bool,

    /// Custom directory to watch for new images (implies `--watch-downloads`).
    #[arg(long)]
    pub downloads_dir: Option<PathBuf>,

    /// Target maximum quality (10-100) in smart mode (default: 95).
    #[arg(long, short = 'q')]
    pub quality: Option<u8>,

    /// Oxipng compression level (0-6, default: 2).
    #[arg(long, short = 'o')]
    pub oxipng_level: Option<u8>,

    /// Maximum image width in pixels (automatically downscales wider images preserving aspect ratio).
    #[arg(long)]
    pub max_width: Option<u32>,

    /// Maximum image height in pixels (automatically downscales taller images preserving aspect ratio).
    #[arg(long)]
    pub max_height: Option<u32>,

    /// Downscale factor (e.g. `2.0` to scale Retina 2x screenshots down by half).
    #[arg(long)]
    pub scale_factor: Option<f32>,

    /// Clipboard polling interval in milliseconds (default: 350).
    #[arg(long)]
    pub interval_ms: Option<u64>,

    /// Optional subcommand (`ui`, `file`, `history`, `config`, `autostart`, `context-menu`).
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Open the graphical Dashboard, Settings, Clipboard History & File Optimizer window.
    Ui,
    /// Launch the interactive OpenCode-style Terminal User Interface (TUI).
    Tui {
        /// Optional working directory for `@file` picking and `!shell` commands.
        path: Option<PathBuf>,
        /// Start with live clipboard watching enabled.
        #[arg(long, short = 'w')]
        watch: bool,
        /// Start with Downloads folder watching enabled.
        #[arg(long)]
        downloads: bool,
        /// Color theme (`dark`, `tokyo`, `catppuccin`, `gruvbox`).
        #[arg(long, default_value = "dark")]
        theme: String,
    },
    /// View or clear optimization history (clipboard & files).
    History {
        /// Show only clipboard history entries.
        #[arg(long)]
        clipboard: bool,
        /// Clear all saved optimization history.
        #[arg(long)]
        clear: bool,
        /// Maximum number of entries to display (default: 20).
        #[arg(long, short = 'n', default_value_t = 20)]
        limit: usize,
    },
    /// Optimize one or more image, PDF, or video files on disk.
    File {
        /// Paths to files to optimize in-place (or to `--output`).
        #[arg(required = true)]
        paths: Vec<PathBuf>,

        /// Optional output path (only valid when optimizing a single file).
        #[arg(long, short = 'O')]
        output: Option<PathBuf>,

        /// Show the Floating HUD Panel with progress bar, before/after size, and Revert button.
        #[arg(long)]
        hud: bool,
    },
    /// View or reset persistent configuration.
    Config {
        /// Write current CLI flags as the new default configuration file.
        #[arg(long)]
        save: bool,
        /// Reset configuration file to built-in defaults.
        #[arg(long)]
        reset: bool,
        /// Print path to the configuration file.
        #[arg(long)]
        path: bool,
    },
    /// Enable or disable automatic launch at login on macOS (LaunchAgent) or Windows (Startup).
    Autostart {
        /// Enable launch at login.
        #[arg(long)]
        enable: bool,
        /// Disable launch at login.
        #[arg(long)]
        disable: bool,
    },
    /// Install or remove the "Optimize with clop-rs" right-click context menu in Finder / Windows Explorer.
    ContextMenu {
        /// Install the right-click context menu.
        #[arg(long)]
        enable: bool,
        /// Remove the right-click context menu.
        #[arg(long)]
        disable: bool,
    },
}

/// Simple wildcard matcher supporting `*` (zero or more chars) and `?` (single char).
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star_idx: Option<usize> = None;
    let mut match_idx: usize = 0;

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_idx = Some(pi);
            match_idx = ti;
            pi += 1;
        } else if let Some(s) = star_idx {
            pi = s + 1;
            match_idx += 1;
            ti = match_idx;
        } else {
            return false;
        }
    }

    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }

    pi == p.len()
}
