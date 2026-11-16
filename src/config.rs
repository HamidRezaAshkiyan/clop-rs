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

/// Persistent configuration stored in `~/.config/clop-rs/config.toml` (macOS/Linux)
/// or `%APPDATA%\clop-rs\config.toml` (Windows).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// Clipboard polling interval in milliseconds.
    pub poll_interval_ms: u64,
    /// Optimization mode: `smart` (lossy palette + lossless deflate) or `lossless`.
    pub mode: OptimizationMode,
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
    /// Strip all non-essential PNG metadata chunks (EXIF, tEXt, iCCP, etc.).
    pub strip_metadata: bool,
    /// Run without a system tray / menu bar icon.
    pub headless: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            poll_interval_ms: 400,
            mode: OptimizationMode::Smart,
            quality_min: 65,
            quality_max: 95,
            oxipng_level: 2,
            max_width: None,
            max_height: None,
            scale_factor: None,
            strip_metadata: true,
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

    /// Saves the configuration to disk.
    pub fn save(&self) -> Result<PathBuf> {
        let path = Self::config_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let contents = toml::to_string_pretty(self)?;
        fs::write(&path, contents)?;
        Ok(path)
    }

    /// Applies CLI flag overrides on top of the loaded configuration.
    pub fn apply_cli(&mut self, cli: &Cli) {
        if let Some(interval) = cli.interval_ms {
            self.poll_interval_ms = interval.max(50);
        }
        if cli.lossless {
            self.mode = OptimizationMode::Lossless;
        } else if let Some(mode) = cli.mode {
            self.mode = mode;
        }
        if let Some(qmax) = cli.quality {
            self.quality_max = qmax.clamp(10, 100);
            self.quality_min = self.quality_min.min(self.quality_max);
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
    about = "Minimal-footprint cross-platform (macOS & Windows) clipboard image optimizer inspired by Clop."
)]
pub struct Cli {
    /// Run as a pure headless background daemon without a system tray / menu bar icon.
    #[arg(long)]
    pub headless: bool,

    /// Optimize the current clipboard image once and exit immediately.
    #[arg(long, short = '1')]
    pub once: bool,

    /// Use strict lossless optimization only (disables perceptual palette quantization).
    #[arg(long)]
    pub lossless: bool,

    /// Optimization mode (`smart` or `lossless`).
    #[arg(long, value_enum)]
    pub mode: Option<OptimizationMode>,

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

    /// Clipboard polling interval in milliseconds (default: 400).
    #[arg(long)]
    pub interval_ms: Option<u64>,

    /// Optional subcommand (`file`, `config`, `autostart`).
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Optimize one or more image files on disk.
    File {
        /// Paths to image files to optimize in-place (or to `--output`).
        #[arg(required = true)]
        paths: Vec<PathBuf>,

        /// Optional output path (only valid when optimizing a single file).
        #[arg(long, short = 'O')]
        output: Option<PathBuf>,
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
}
