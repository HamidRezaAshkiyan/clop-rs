#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

use anyhow::{bail, Result};
use clap::Parser;
use clop_rs::clipboard::ClipboardMonitor;
use clop_rs::config::{AppConfig, Cli, Commands};
use clop_rs::downloads::DownloadsWatcher;
use clop_rs::optimizer::{self, format_bytes};
use clop_rs::{context_menu, platform, tray};
use std::thread;
use std::time::Duration;

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,oxipng=warn"))
        .format_timestamp_secs()
        .init();

    let cli = Cli::parse();
    let mut config = AppConfig::load().unwrap_or_default();
    config.apply_cli(&cli);

    // Handle subcommands first (`file`, `config`, `autostart`, `context-menu`)
    if let Some(ref cmd) = cli.command {
        match cmd {
            Commands::File { paths, output, hud } => {
                if output.is_some() && paths.len() > 1 {
                    bail!("--output can only be specified when optimizing a single file");
                }
                if *hud {
                    return tray::run_file_with_hud(paths.clone(), output.clone(), config);
                }
                for path in paths {
                    let res = optimizer::optimize_file(path, output.as_deref(), &config)?;
                    if res.was_modified {
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
        }
    }

    // One-shot clipboard optimization (`--once` / `-1`)
    if cli.once {
        let mut monitor = ClipboardMonitor::new();
        match monitor.poll_once(&config, true)? {
            Some(summary) => {
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

    // Default: native Menu Bar (macOS) / System Tray (Windows) + Floating HUD Panel
    tray::run_with_tray(config)
}

fn run_headless_daemon(config: AppConfig) -> Result<()> {
    log::info!(
        "clop-rs running in headless daemon mode (mode={:?}, strip_exif={}, watch_downloads={}, interval={}ms)",
        config.mode,
        config.strip_metadata,
        config.watch_downloads,
        config.poll_interval_ms
    );
    let mut monitor = ClipboardMonitor::new();
    let mut downloads = DownloadsWatcher::new(if config.watch_downloads {
        config.effective_downloads_dir()
    } else {
        None
    });
    let interval = Duration::from_millis(config.poll_interval_ms.max(50));

    loop {
        if let Err(err) = monitor.poll_once(&config, false) {
            log::debug!("Clipboard poll error: {}", err);
        }
        if config.watch_downloads {
            if let Some(pending) = downloads.inspect_next_file(&config) {
                if let Err(err) = downloads.optimize_pending_file(pending, &config) {
                    log::debug!("Downloads optimization error: {}", err);
                }
            }
        }
        thread::sleep(interval);
    }
}
