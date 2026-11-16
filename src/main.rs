#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

use anyhow::{bail, Result};
use clap::Parser;
use clop_rs::clipboard::ClipboardMonitor;
use clop_rs::config::{AppConfig, Cli, Commands};
use clop_rs::optimizer::{self, format_bytes};
use clop_rs::{platform, tray};
use std::thread;
use std::time::Duration;

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,oxipng=warn"))
        .format_timestamp_secs()
        .init();

    let cli = Cli::parse();
    let mut config = AppConfig::load().unwrap_or_default();
    config.apply_cli(&cli);

    // Handle subcommands first (`file`, `config`, `autostart`)
    if let Some(ref cmd) = cli.command {
        match cmd {
            Commands::File { paths, output } => {
                if output.is_some() && paths.len() > 1 {
                    bail!("--output can only be specified when optimizing a single file");
                }
                for path in paths {
                    let res = optimizer::optimize_file(path, output.as_deref(), &config)?;
                    if res.was_modified {
                        println!(
                            "✓ {}: {} -> {} (-{:.1}%, {}ms)",
                            path.display(),
                            format_bytes(res.original_size),
                            format_bytes(res.optimized_size),
                            res.saved_percent(),
                            res.elapsed.as_millis()
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

    // Default: lightweight native Menu Bar (macOS) / System Tray (Windows) mode
    tray::run_with_tray(config)
}

fn run_headless_daemon(config: AppConfig) -> Result<()> {
    log::info!(
        "clop-rs running in headless daemon mode (mode={:?}, interval={}ms)",
        config.mode,
        config.poll_interval_ms
    );
    let mut monitor = ClipboardMonitor::new();
    let interval = Duration::from_millis(config.poll_interval_ms.max(50));

    loop {
        if let Err(err) = monitor.poll_once(&config, false) {
            log::debug!("Clipboard poll error: {}", err);
        }
        thread::sleep(interval);
    }
}
