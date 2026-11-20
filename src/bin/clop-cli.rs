use anyhow::{bail, Result};
use clap::{Args, Parser, Subcommand};
use clop_rs::clipboard::{ClipboardMonitor, OptimizationSummary};
use clop_rs::config::{AppConfig, OptimizationMode, OptimizationPreset};
use clop_rs::downloads::{is_supported_media_path, DownloadsWatcher};
use clop_rs::history::{HistorySource, OptimizationHistory};
use clop_rs::optimizer::{self, format_bytes, supported_formats_info};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

#[derive(Debug, Parser)]
#[command(
    name = "clop-cli",
    version,
    about = "Command-line image, PDF, video & clipboard optimizer powered by clop-rs"
)]
struct ClopCli {
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Debug, Clone, Args)]
struct OptimizeFlags {
    /// Quality-vs-size preset (`lossless`, `high-quality`, `balanced`, `smallest-size`).
    #[arg(long, short = 'p', value_enum)]
    preset: Option<OptimizationPreset>,

    /// Target quality level (10 = smallest size, 100 = highest quality).
    #[arg(long, short = 'q')]
    quality: Option<u8>,

    /// Use strict bit-exact lossless compression.
    #[arg(long)]
    lossless: bool,

    /// Oxipng compression effort (0-6).
    #[arg(long, short = 'o')]
    oxipng_level: Option<u8>,

    /// Strip EXIF, GPS, and metadata chunks.
    #[arg(long)]
    strip_exif: bool,

    /// Preserve EXIF and metadata chunks.
    #[arg(long)]
    keep_exif: bool,

    /// Maximum image width in pixels (preserves aspect ratio).
    #[arg(long)]
    max_width: Option<u32>,

    /// Maximum image height in pixels (preserves aspect ratio).
    #[arg(long)]
    max_height: Option<u32>,

    /// Downscale factor (e.g. `2.0` to halve Retina 2x screenshots).
    #[arg(long)]
    scale_factor: Option<f32>,
}

impl OptimizeFlags {
    fn apply_to(&self, cfg: &mut AppConfig) {
        if let Some(preset) = self.preset {
            cfg.apply_preset(preset);
        }
        if self.lossless {
            cfg.apply_preset(OptimizationPreset::Lossless);
        }
        if let Some(q) = self.quality {
            cfg.apply_quality_level(q);
        }
        if let Some(level) = self.oxipng_level {
            cfg.oxipng_level = level.min(6);
        }
        if self.strip_exif {
            cfg.strip_metadata = true;
        }
        if self.keep_exif {
            cfg.strip_metadata = false;
        }
        if let Some(mw) = self.max_width {
            cfg.max_width = Some(mw);
        }
        if let Some(mh) = self.max_height {
            cfg.max_height = Some(mh);
        }
        if let Some(sf) = self.scale_factor {
            cfg.scale_factor = Some(sf);
        }
    }
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// Optimize one or more files or directories on disk.
    #[command(visible_alias = "opt")]
    Optimize {
        /// Files or directories to optimize.
        #[arg(required = true)]
        paths: Vec<PathBuf>,

        /// Recurse into subdirectories when a directory path is provided.
        #[arg(long, short = 'r')]
        recursive: bool,

        /// Write output to a separate path (only valid for a single input file).
        #[arg(long, short = 'O')]
        output: Option<PathBuf>,

        /// Preview which files would be processed without modifying anything.
        #[arg(long)]
        dry_run: bool,

        /// Output results as JSON.
        #[arg(long)]
        json: bool,

        #[command(flatten)]
        opts: OptimizeFlags,
    },

    /// Optimize the current clipboard image (or watch clipboard continuously).
    #[command(visible_alias = "cb")]
    Clipboard {
        /// Continuously watch the clipboard and optimize new images as they arrive.
        #[arg(long, short = 'w')]
        watch: bool,

        /// Output result as JSON (one-shot mode).
        #[arg(long)]
        json: bool,

        #[command(flatten)]
        opts: OptimizeFlags,
    },

    /// View or clear clipboard & file optimization history.
    #[command(visible_alias = "hist")]
    History {
        /// Show only clipboard optimization entries.
        #[arg(long)]
        clipboard: bool,

        /// Maximum number of entries to show.
        #[arg(long, short = 'n', default_value_t = 20)]
        limit: usize,

        /// Clear all stored history entries.
        #[arg(long)]
        clear: bool,

        /// Output history as JSON.
        #[arg(long)]
        json: bool,
    },

    /// View, set, or reset persistent clop-rs configuration.
    Config {
        #[command(subcommand)]
        action: Option<ConfigAction>,

        /// Output config as JSON (for `show`).
        #[arg(long)]
        json: bool,
    },

    /// List all supported file extensions and optional tool availability (FFmpeg, Ghostscript).
    Formats {
        /// Output supported formats as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Run a headless terminal watcher for clipboard and/or Downloads folder.
    Watch {
        /// Also watch and auto-optimize the Downloads folder.
        #[arg(long)]
        downloads: bool,

        /// Custom directory to watch (implies `--downloads`).
        #[arg(long)]
        downloads_dir: Option<PathBuf>,

        /// Polling interval in milliseconds.
        #[arg(long)]
        interval_ms: Option<u64>,

        #[command(flatten)]
        opts: OptimizeFlags,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigAction {
    /// Print the active configuration (default).
    Show,
    /// Print the path to `config.toml`.
    Path,
    /// Reset configuration to defaults.
    Reset,
    /// Set a configuration key to a new value and save to disk.
    Set {
        /// Setting key (e.g. `preset`, `quality`, `mode`, `oxipng_level`, `strip_metadata`, `max_width`, `max_height`, `scale_factor`, `watch_downloads`, `show_floating_hud`, `poll_interval_ms`).
        key: String,
        /// New value (use `none` to clear optional numeric/path values).
        value: String,
    },
}

#[derive(Debug, Serialize)]
struct FileOptReport {
    path: String,
    status: String,
    original_size: usize,
    optimized_size: usize,
    saved_bytes: usize,
    saved_percent: f64,
    exif_stripped: bool,
    elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .format_timestamp_secs()
        .init();

    let cli = ClopCli::parse();
    let mut config = AppConfig::load().unwrap_or_default();

    match cli.command {
        CliCommand::Optimize {
            paths,
            recursive,
            output,
            dry_run,
            json,
            opts,
        } => {
            opts.apply_to(&mut config);
            let files = collect_input_files(&paths, recursive)?;
            if files.is_empty() {
                bail!("No supported files found in the specified path(s). Run `clop-cli formats` to see supported extensions.");
            }
            if output.is_some() && files.len() > 1 {
                bail!("--output can only be used when optimizing a single file.");
            }

            if dry_run {
                if json {
                    let list: Vec<String> = files.iter().map(|p| p.display().to_string()).collect();
                    println!("{}", serde_json::to_string_pretty(&list)?);
                } else {
                    println!(
                        "Dry run ({}, {} files):",
                        config.preset_summary_label(),
                        files.len()
                    );
                    for f in &files {
                        let sz = fs::metadata(f).map(|m| m.len() as usize).unwrap_or(0);
                        println!("  • {} ({})", f.display(), format_bytes(sz));
                    }
                }
                return Ok(());
            }

            let mut history = OptimizationHistory::load();
            let mut reports = Vec::with_capacity(files.len());
            let mut total_orig = 0usize;
            let mut total_opt = 0usize;
            let mut modified_count = 0usize;

            for file in &files {
                match optimizer::optimize_file(file, output.as_deref(), &config) {
                    Ok(res) => {
                        total_orig += res.original_size;
                        total_opt += res.optimized_size;
                        let status = if res.was_modified {
                            modified_count += 1;
                            let file_name = file
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("File")
                                .to_string();
                            let summary = OptimizationSummary {
                                source_label: file_name,
                                width: res.width,
                                height: res.height,
                                original_size: res.original_size,
                                optimized_size: res.optimized_size,
                                saved_bytes: res.saved_bytes(),
                                saved_percent: res.saved_percent(),
                                exif_stripped: res.exif_stripped,
                                elapsed: res.elapsed,
                                thumbnail_64: Vec::new(),
                            };
                            history.record_summary(
                                HistorySource::Cli,
                                &summary,
                                Some(file),
                                &config,
                            );
                            "optimized"
                        } else {
                            "optimal"
                        };

                        let report = FileOptReport {
                            path: file.display().to_string(),
                            status: status.to_string(),
                            original_size: res.original_size,
                            optimized_size: res.optimized_size,
                            saved_bytes: res.saved_bytes(),
                            saved_percent: res.saved_percent(),
                            exif_stripped: res.exif_stripped,
                            elapsed_ms: res.elapsed.as_millis() as u64,
                            error: None,
                        };

                        if !json {
                            if res.was_modified {
                                let exif = if res.exif_stripped { " [EXIF]" } else { "" };
                                println!(
                                    "✓ {}: {} -> {} (-{:.1}%, {}ms){}",
                                    file.display(),
                                    format_bytes(res.original_size),
                                    format_bytes(res.optimized_size),
                                    res.saved_percent(),
                                    res.elapsed.as_millis(),
                                    exif
                                );
                            } else {
                                println!(
                                    "• {}: already optimal ({})",
                                    file.display(),
                                    format_bytes(res.original_size)
                                );
                            }
                        }
                        reports.push(report);
                    }
                    Err(err) => {
                        if !json {
                            eprintln!("✗ {}: {}", file.display(), err);
                        }
                        reports.push(FileOptReport {
                            path: file.display().to_string(),
                            status: "error".to_string(),
                            original_size: 0,
                            optimized_size: 0,
                            saved_bytes: 0,
                            saved_percent: 0.0,
                            exif_stripped: false,
                            elapsed_ms: 0,
                            error: Some(err.to_string()),
                        });
                    }
                }
            }

            if modified_count > 0 {
                let _ = history.save();
            }

            let total_saved = total_orig.saturating_sub(total_opt);
            let total_pct = if total_orig > 0 {
                (total_saved as f64 / total_orig as f64) * 100.0
            } else {
                0.0
            };

            if json {
                let output_obj = serde_json::json!({
                    "preset": config.preset_summary_label(),
                    "files_total": files.len(),
                    "files_modified": modified_count,
                    "total_original_bytes": total_orig,
                    "total_optimized_bytes": total_opt,
                    "total_saved_bytes": total_saved,
                    "total_saved_percent": total_pct,
                    "results": reports,
                });
                println!("{}", serde_json::to_string_pretty(&output_obj)?);
            } else if files.len() > 1 {
                println!(
                    "\nSummary: {}/{} files optimized | {} -> {} (saved {}, -{:.1}%)",
                    modified_count,
                    files.len(),
                    format_bytes(total_orig),
                    format_bytes(total_opt),
                    format_bytes(total_saved),
                    total_pct
                );
            }
        }

        CliCommand::Clipboard { watch, json, opts } => {
            opts.apply_to(&mut config);
            let mut monitor = ClipboardMonitor::new();
            let mut history = OptimizationHistory::load();

            if watch {
                println!(
                    "Watching clipboard ({}, strip_exif={})… Press Ctrl+C to stop.",
                    config.preset_summary_label(),
                    config.strip_metadata
                );
                let interval = Duration::from_millis(config.poll_interval_ms.max(100));
                loop {
                    if let Ok(Some(summary)) = monitor.poll_once(&config, false) {
                        history.record_summary(HistorySource::Clipboard, &summary, None, &config);
                        let _ = history.save();
                        println!(
                            "✓ Clipboard ({}x{}): {}",
                            summary.width,
                            summary.height,
                            summary.display_short()
                        );
                    }
                    thread::sleep(interval);
                }
            } else {
                match monitor.poll_once(&config, true)? {
                    Some(summary) => {
                        history.record_summary(HistorySource::Clipboard, &summary, None, &config);
                        let _ = history.save();
                        if json {
                            println!(
                                "{}",
                                serde_json::to_string_pretty(&serde_json::json!({
                                    "status": "optimized",
                                    "width": summary.width,
                                    "height": summary.height,
                                    "original_size": summary.original_size,
                                    "optimized_size": summary.optimized_size,
                                    "saved_bytes": summary.saved_bytes,
                                    "saved_percent": summary.saved_percent,
                                    "exif_stripped": summary.exif_stripped,
                                    "elapsed_ms": summary.elapsed.as_millis() as u64,
                                }))?
                            );
                        } else {
                            println!(
                                "✓ Optimized clipboard ({}x{}): {}",
                                summary.width,
                                summary.height,
                                summary.display_short()
                            );
                        }
                    }
                    None => {
                        if json {
                            println!(
                                "{}",
                                serde_json::to_string_pretty(&serde_json::json!({
                                    "status": "skipped",
                                    "message": "No image on clipboard or already optimal"
                                }))?
                            );
                        } else {
                            println!(
                                "• Clipboard does not contain an image or is already optimal."
                            );
                        }
                    }
                }
            }
        }

        CliCommand::History {
            clipboard,
            limit,
            clear,
            json,
        } => {
            let mut history = OptimizationHistory::load();
            if clear {
                history.clear();
                let path = history.save()?;
                if json {
                    println!(r#"{{"cleared": true}}"#);
                } else {
                    println!("Cleared optimization history ({})", path.display());
                }
                return Ok(());
            }

            let stats = history.stats();
            let entries: Vec<_> = history
                .entries
                .iter()
                .filter(|e| !clipboard || e.source == HistorySource::Clipboard)
                .take(limit)
                .cloned()
                .collect();

            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "stats": stats,
                        "entries": entries,
                    }))?
                );
            } else {
                println!(
                    "Optimization History: {} total ({} clipboard, {} files) | Saved {} ({:.1}%)",
                    stats.total_items,
                    stats.clipboard_items,
                    stats.file_items,
                    format_bytes(stats.total_saved_bytes as usize),
                    stats.overall_saved_percent
                );
                if entries.is_empty() {
                    println!("(no entries)");
                } else {
                    for e in entries {
                        let dims = if e.width > 0 && e.height > 0 {
                            format!(" {}x{}", e.width, e.height)
                        } else {
                            String::new()
                        };
                        let exif = if e.exif_stripped { " [EXIF]" } else { "" };
                        println!(
                            "  [{:<9}] {}{} — {} -> {} (-{:.1}%, {} saved, {}ms, {}){}",
                            e.source.as_str(),
                            e.label,
                            dims,
                            format_bytes(e.original_size),
                            format_bytes(e.optimized_size),
                            e.saved_percent,
                            format_bytes(e.saved_bytes),
                            e.elapsed_ms,
                            e.preset_label,
                            exif
                        );
                    }
                }
            }
        }

        CliCommand::Config { action, json } => match action.unwrap_or(ConfigAction::Show) {
            ConfigAction::Show => {
                if json {
                    println!("{}", serde_json::to_string_pretty(&config)?);
                } else {
                    println!("{}", toml::to_string_pretty(&config)?);
                }
            }
            ConfigAction::Path => {
                println!("{}", AppConfig::config_path()?.display());
            }
            ConfigAction::Reset => {
                let default_cfg = AppConfig::default();
                let p = default_cfg.save()?;
                println!("Reset configuration to defaults at {}", p.display());
            }
            ConfigAction::Set { key, value } => {
                apply_config_key(&mut config, &key, &value)?;
                let p = config.save()?;
                println!("Updated `{}` and saved to {}", key, p.display());
            }
        },

        CliCommand::Formats { json } => {
            let info = supported_formats_info();
            if json {
                println!("{}", serde_json::to_string_pretty(&info)?);
            } else {
                println!("Supported File Extensions:");
                println!(
                    "  Images (built-in):     {}",
                    info.images
                        .iter()
                        .map(|e| format!(".{}", e))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                println!(
                    "  Documents ({}):  {}",
                    if info.has_ghostscript {
                        "native + gs"
                    } else {
                        "native Rust"
                    },
                    info.documents
                        .iter()
                        .map(|e| format!(".{}", e))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                println!(
                    "  Videos ({}): {}",
                    if info.has_ffmpeg {
                        "FFmpeg ready"
                    } else {
                        "requires FFmpeg"
                    },
                    info.videos
                        .iter()
                        .map(|e| format!(".{}", e))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }

        CliCommand::Watch {
            downloads,
            downloads_dir,
            interval_ms,
            opts,
        } => {
            opts.apply_to(&mut config);
            if downloads {
                config.watch_downloads = true;
            }
            if let Some(dir) = downloads_dir {
                config.downloads_dir = Some(dir);
                config.watch_downloads = true;
            }
            if let Some(ms) = interval_ms {
                config.poll_interval_ms = ms.max(50);
            }

            let mut monitor = ClipboardMonitor::new();
            let mut history = OptimizationHistory::load();
            let mut dl = DownloadsWatcher::new(if config.watch_downloads {
                config.effective_downloads_dir()
            } else {
                None
            });

            println!(
                "clop-cli watching clipboard{} ({}, strip_exif={})… Press Ctrl+C to stop.",
                if config.watch_downloads {
                    format!(
                        " + {}",
                        config
                            .effective_downloads_dir()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "~/Downloads".to_string())
                    )
                } else {
                    String::new()
                },
                config.preset_summary_label(),
                config.strip_metadata
            );

            let interval = Duration::from_millis(config.poll_interval_ms.max(50));
            loop {
                if let Ok(Some(summary)) = monitor.poll_once(&config, false) {
                    history.record_summary(HistorySource::Clipboard, &summary, None, &config);
                    let _ = history.save();
                    println!(
                        "✓ [clipboard] {}x{}: {}",
                        summary.width,
                        summary.height,
                        summary.display_short()
                    );
                }
                if config.watch_downloads {
                    if let Some(pending) = dl.inspect_next_file(&config) {
                        let path = pending.path.clone();
                        if let Ok(Some(summary)) = dl.optimize_pending_file(pending, &config) {
                            history.record_summary(
                                HistorySource::Downloads,
                                &summary,
                                Some(&path),
                                &config,
                            );
                            let _ = history.save();
                            println!(
                                "✓ [downloads] {}: {}",
                                summary.source_label,
                                summary.display_short()
                            );
                        }
                    }
                }
                thread::sleep(interval);
            }
        }
    }

    Ok(())
}

fn collect_input_files(paths: &[PathBuf], recursive: bool) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for p in paths {
        if !p.exists() {
            bail!("Path does not exist: {}", p.display());
        }
        if p.is_file() {
            out.push(p.clone());
        } else if p.is_dir() {
            collect_dir(p, recursive, &mut out);
        }
    }
    Ok(out)
}

fn collect_dir(dir: &Path, recursive: bool, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if recursive
                && !path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with('.'))
            {
                collect_dir(&path, true, out);
            }
        } else if path.is_file() && is_supported_media_path(&path) {
            out.push(path);
        }
    }
}

fn apply_config_key(cfg: &mut AppConfig, key: &str, value: &str) -> Result<()> {
    let v = value.trim();
    match key.to_ascii_lowercase().as_str() {
        "preset" => {
            let preset = match v.to_ascii_lowercase().as_str() {
                "lossless" => OptimizationPreset::Lossless,
                "high-quality" | "high" => OptimizationPreset::HighQuality,
                "balanced" => OptimizationPreset::Balanced,
                "smallest-size" | "smallest" | "small" => OptimizationPreset::SmallestSize,
                "custom" => OptimizationPreset::Custom,
                _ => bail!(
                    "Invalid preset `{}`. Valid values: lossless, high-quality, balanced, smallest-size",
                    v
                ),
            };
            cfg.apply_preset(preset);
        }
        "quality" | "quality_level" => {
            let q: u8 = v.parse()?;
            cfg.apply_quality_level(q);
        }
        "mode" => match v.to_ascii_lowercase().as_str() {
            "smart" => cfg.mode = OptimizationMode::Smart,
            "lossless" => cfg.apply_preset(OptimizationPreset::Lossless),
            _ => bail!("Invalid mode `{}`. Valid values: smart, lossless", v),
        },
        "oxipng_level" => {
            let lvl: u8 = v.parse()?;
            cfg.oxipng_level = lvl.min(6);
        }
        "strip_metadata" | "strip_exif" => {
            cfg.strip_metadata = parse_bool(v)?;
        }
        "show_floating_hud" => {
            cfg.show_floating_hud = parse_bool(v)?;
        }
        "watch_downloads" => {
            cfg.watch_downloads = parse_bool(v)?;
        }
        "downloads_dir" => {
            if v.eq_ignore_ascii_case("none") || v.is_empty() {
                cfg.downloads_dir = None;
            } else {
                cfg.downloads_dir = Some(PathBuf::from(v));
            }
        }
        "max_width" => {
            cfg.max_width = parse_optional_u32(v)?;
        }
        "max_height" => {
            cfg.max_height = parse_optional_u32(v)?;
        }
        "scale_factor" => {
            if v.eq_ignore_ascii_case("none") || v.is_empty() {
                cfg.scale_factor = None;
            } else {
                let sf: f32 = v.parse()?;
                cfg.scale_factor = if sf > 1.0 { Some(sf) } else { None };
            }
        }
        "poll_interval_ms" => {
            let ms: u64 = v.parse()?;
            cfg.poll_interval_ms = ms.max(50);
        }
        _ => bail!(
            "Unknown config key `{}`. Supported keys: preset, quality, mode, oxipng_level, strip_metadata, show_floating_hud, watch_downloads, downloads_dir, max_width, max_height, scale_factor, poll_interval_ms",
            key
        ),
    }
    Ok(())
}

fn parse_bool(v: &str) -> Result<bool> {
    match v.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => bail!("Invalid boolean `{}` (use true/false)", v),
    }
}

fn parse_optional_u32(v: &str) -> Result<Option<u32>> {
    if v.eq_ignore_ascii_case("none") || v.is_empty() || v == "0" {
        Ok(None)
    } else {
        Ok(Some(v.parse()?))
    }
}
