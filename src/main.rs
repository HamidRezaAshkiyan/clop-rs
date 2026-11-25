#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

use anyhow::{bail, Result};
use clap::Parser;
use clop_rs::clipboard::ClipboardMonitor;
use clop_rs::config::{AppConfig, Cli, Commands};
use clop_rs::downloads::DownloadsWatcher;
use clop_rs::history::{HistorySource, OptimizationHistory};
use clop_rs::optimizer::{self, format_bytes};
use clop_rs::{context_menu, platform, tray, updater};
use std::thread;
use std::time::Duration;

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,oxipng=warn"))
        .format_timestamp_secs()
        .init();

    let cli = Cli::parse();
    let mut config = AppConfig::load().unwrap_or_default();
    config.apply_cli(&cli);

    // Handle subcommands first (`ui`, `history`, `file`, `config`, `autostart`, `context-menu`)
    if let Some(ref cmd) = cli.command {
        match cmd {
            Commands::Ui => {
                return tray::run_with_tray(config, true);
            }
            Commands::Tui {
                path,
                watch,
                downloads,
                theme,
            } => {
                let opts = clop_rs::tui::TuiLaunchOptions {
                    work_dir: path
                        .clone()
                        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into())),
                    watch_clipboard: *watch,
                    watch_downloads: *downloads,
                    theme: clop_rs::tui::TuiTheme::from_name(theme)
                        .unwrap_or(clop_rs::tui::TuiTheme::OpenCodeDark),
                };
                return clop_rs::tui::run_tui(config, opts);
            }
            Commands::History {
                clipboard,
                clear,
                limit,
            } => {
                let mut history = OptimizationHistory::load();
                if *clear {
                    history.clear();
                    let path = history.save()?;
                    println!("Cleared optimization history ({})", path.display());
                    return Ok(());
                }
                let stats = history.stats();
                println!(
                    "clop-rs History: {} items ({} clipboard, {} files) | Saved {} ({:.1}%)",
                    stats.total_items,
                    stats.clipboard_items,
                    stats.file_items,
                    format_bytes(stats.total_saved_bytes as usize),
                    stats.overall_saved_percent
                );
                let entries: Vec<_> = history
                    .entries
                    .iter()
                    .filter(|e| !*clipboard || e.source == HistorySource::Clipboard)
                    .take(*limit)
                    .collect();
                if entries.is_empty() {
                    println!("(no entries)");
                    return Ok(());
                }
                for e in entries {
                    let exif = if e.exif_stripped { " [EXIF]" } else { "" };
                    println!(
                        "  [{}] {} — {} -> {} (-{:.1}%, {} saved, {}ms, {}){}",
                        e.source.as_str(),
                        e.label,
                        format_bytes(e.original_size),
                        format_bytes(e.optimized_size),
                        e.saved_percent,
                        format_bytes(e.saved_bytes),
                        e.elapsed_ms,
                        e.preset_label,
                        exif
                    );
                }
                return Ok(());
            }
            Commands::File { paths, output, hud } => {
                if output.is_some() && paths.len() > 1 {
                    bail!("--output can only be specified when optimizing a single file");
                }
                if *hud {
                    return tray::run_file_with_hud(paths.clone(), output.clone(), config);
                }
                let mut history = OptimizationHistory::load();
                for path in paths {
                    let res = optimizer::optimize_file(path, output.as_deref(), &config)?;
                    if res.was_modified {
                        let file_name = path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("File")
                            .to_string();
                        let summary = clop_rs::clipboard::OptimizationSummary {
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
                        history.record_summary(HistorySource::File, &summary, Some(path), &config);
                        let _ = history.save();

                        let exif_tag = if res.exif_stripped {
                            " [EXIF/metadata stripped]"
                        } else {
                            ""
                        };
                        println!(
                            "✓ {}: {} -> {} (-{:.1}%, {}ms){}",
                            path.display(),
                            format_bytes(res.original_size),
                            format_bytes(res.optimized_size),
                            res.saved_percent(),
                            res.elapsed.as_millis(),
                            exif_tag
                        );
                    } else {
                        println!(
                            "• {}: already optimal ({})",
                            path.display(),
                            format_bytes(res.original_size)
                        );
                    }
                }
                return Ok(());
            }
            Commands::Config { save, reset, path } => {
                if *path {
                    println!("{}", AppConfig::config_path()?.display());
                    return Ok(());
                }
                if *reset {
                    let default_cfg = AppConfig::default();
                    let saved_path = default_cfg.save()?;
                    println!(
                        "Reset configuration to defaults at {}",
                        saved_path.display()
                    );
                    return Ok(());
                }
                if *save {
                    let saved_path = config.save()?;
                    println!("Saved configuration to {}", saved_path.display());
                    return Ok(());
                }
                println!("{}", toml::to_string_pretty(&config)?);
                return Ok(());
            }
            Commands::Autostart { enable, disable } => {
                if *enable && *disable {
                    bail!("Specify either --enable or --disable, not both");
                }
                if *enable {
                    let path = platform::set_autostart(true)?;
                    println!("Enabled launch at login ({})", path.display());
                } else if *disable {
                    let path = platform::set_autostart(false)?;
                    println!("Disabled launch at login ({})", path.display());
                } else {
                    println!(
                        "Launch at login is currently: {}",
                        if platform::is_autostart_enabled() {
                            "enabled"
                        } else {
                            "disabled"
                        }
                    );
                }
                return Ok(());
            }
            Commands::ContextMenu { enable, disable } => {
                if *enable && *disable {
                    bail!("Specify either --enable or --disable, not both");
                }
                if *enable {
                    let path = context_menu::set_context_menu(true)?;
                    println!(
                        "Installed 'Optimize with clop-rs' right-click context menu ({})",
                        path.display()
                    );
                } else if *disable {
                    let path = context_menu::set_context_menu(false)?;
                    println!(
                        "Removed 'Optimize with clop-rs' right-click context menu ({})",
                        path.display()
                    );
                } else {
                    println!(
                        "Right-click context menu is currently: {}",
                        if context_menu::is_context_menu_enabled() {
                            "enabled"
                        } else {
                            "disabled"
                        }
                    );
                }
                return Ok(());
            }
            Commands::Update { json, open } => {
                let status = updater::check_for_updates();
                if status.error.is_none() {
                    let _ = updater::save_cached_status(&status);
                }
                if *json {
                    println!("{}", serde_json::to_string_pretty(&status)?);
                } else if let Some(ref err) = status.error {
                    eprintln!("✗ Update check failed: {}", err);
                    bail!("{}", err);
                } else if status.update_available {
                    let latest = status.latest_version.as_deref().unwrap_or("unknown");
                    println!(
                        "⬆ Update available: v{} -> v{}",
                        status.current_version, latest
                    );
                    println!("  Download: {}", status.release_url);
                    if let Some(ref notes) = status.release_notes {
                        println!("\n{}", notes);
                    }
                } else {
                    println!("✓ clop-rs v{} is up to date.", status.current_version);
                }
                if *open && status.update_available {
                    platform::open_url(&status.release_url)?;
                }
                return Ok(());
            }
        }
    }

    // One-shot clipboard optimization (`--once` / `-1`)
    if cli.once {
        let mut monitor = ClipboardMonitor::new();
        match monitor.poll_once(&config, true)? {
            Some(summary) => {
                let mut history = OptimizationHistory::load();
                history.record_summary(HistorySource::Clipboard, &summary, None, &config);
                let _ = history.save();
                println!(
                    "✓ Optimized clipboard image ({}x{}): {}",
                    summary.width,
                    summary.height,
                    summary.display_short()
                );
            }
            None => {
                println!("• Clipboard does not contain an image or is already optimal.");
            }
        }
        return Ok(());
    }

    // Headless daemon mode (`--headless` or configured in config.toml)
    if config.headless {
        return run_headless_daemon(config);
    }

    // Default: native Menu Bar (macOS) / System Tray (Windows) + Floating HUD Panel + Dashboard UI
    tray::run_with_tray(config, cli.open_ui)
}

fn run_headless_daemon(config: AppConfig) -> Result<()> {
    log::info!(
        "clop-rs running in headless daemon mode (preset={:?}, mode={:?}, strip_exif={}, watch_downloads={}, interval={}ms)",
        config.preset,
        config.mode,
        config.strip_metadata,
        config.watch_downloads,
        config.poll_interval_ms
    );
    let mut monitor = ClipboardMonitor::new();
    let mut history = OptimizationHistory::load();
    let mut downloads = DownloadsWatcher::new(if config.watch_downloads {
        config.effective_downloads_dir()
    } else {
        None
    });
    let interval = Duration::from_millis(config.poll_interval_ms.max(50));

    loop {
        match monitor.poll_once(&config, false) {
            Ok(Some(summary)) => {
                history.record_summary(HistorySource::Clipboard, &summary, None, &config);
                let _ = history.save();
            }
            Ok(None) => {}
            Err(err) => log::debug!("Clipboard poll error: {}", err),
        }
        if config.watch_downloads {
            if let Some(pending) = downloads.inspect_next_file(&config) {
                let path = pending.path.clone();
                match downloads.optimize_pending_file(pending, &config) {
                    Ok(Some(summary)) => {
                        history.record_summary(
                            HistorySource::Downloads,
                            &summary,
                            Some(&path),
                            &config,
                        );
                        let _ = history.save();
                    }
                    Ok(None) => {}
                    Err(err) => log::debug!("Downloads optimization error: {}", err),
                }
            }
        }
        thread::sleep(interval);
    }
}
