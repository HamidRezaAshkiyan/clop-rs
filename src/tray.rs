use crate::clipboard::{ClipboardMonitor, SessionStats};
use crate::config::{AppConfig, OptimizationMode};
use crate::optimizer::format_bytes;
use crate::platform;
use anyhow::{Context, Result};
use std::time::{Duration, Instant};
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};

/// Runs the lightweight background event loop with a native macOS Menu Bar / Windows System Tray icon.
pub fn run_with_tray(mut config: AppConfig) -> Result<()> {
    let event_loop = EventLoopBuilder::new().build();

    // Hide Dock icon on macOS so we only appear in the top menu bar
    platform::configure_background_app();

    let menu = Menu::new();

    let status_item = MenuItem::new("clop-rs — Watching Clipboard", false, None);
    let session_item = MenuItem::new("Session: 0 images (0 B saved)", false, None);
    let optimize_now_item = MenuItem::new("Optimize Clipboard Now", true, None);
    let pause_item = CheckMenuItem::new("Pause Auto-Optimization", true, false, None);
    let smart_mode_item = CheckMenuItem::new(
        "Smart Mode (Palette + Oxipng)",
        true,
        config.mode == OptimizationMode::Smart,
        None,
    );
    let autostart_item = CheckMenuItem::new(
        "Launch at Login",
        true,
        platform::is_autostart_enabled(),
        None,
    );
    let quit_item = MenuItem::new("Quit clop-rs", true, None);

    menu.append(&status_item)?;
    menu.append(&session_item)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&optimize_now_item)?;
    menu.append(&pause_item)?;
    menu.append(&smart_mode_item)?;
    menu.append(&autostart_item)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&quit_item)?;

    let icon = build_tray_icon()?;
    let _tray_icon = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("clop-rs — Clipboard Image Optimizer")
        .with_icon(icon)
        .build()
        .context("Failed to create system tray / menu bar icon")?;

    let mut monitor = ClipboardMonitor::new();
    let mut paused = false;
    let poll_interval = Duration::from_millis(config.poll_interval_ms.max(100));
    let mut next_poll = Instant::now();

    let optimize_now_id = optimize_now_item.id().clone();
    let pause_id = pause_item.id().clone();
    let smart_mode_id = smart_mode_item.id().clone();
    let autostart_id = autostart_item.id().clone();
    let quit_id = quit_item.id().clone();

    log::info!(
        "clop-rs started in system tray mode (mode={:?}, interval={}ms)",
        config.mode,
        config.poll_interval_ms
    );

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(next_poll);

        // Handle tray menu clicks
        while let Ok(menu_event) = MenuEvent::receiver().try_recv() {
            if menu_event.id == quit_id {
                log::info!("Quitting clop-rs via tray menu");
                *control_flow = ControlFlow::Exit;
                return;
            } else if menu_event.id == pause_id {
                paused = pause_item.is_checked();
                if paused {
                    status_item.set_text("clop-rs — Paused");
                } else {
                    status_item.set_text("clop-rs — Watching Clipboard");
                }
            } else if menu_event.id == smart_mode_id {
                config.mode = if smart_mode_item.is_checked() {
                    OptimizationMode::Smart
                } else {
                    OptimizationMode::Lossless
                };
                let _ = config.save();
                log::info!("Switched optimization mode to {:?}", config.mode);
            } else if menu_event.id == autostart_id {
                let enable = autostart_item.is_checked();
                if let Err(err) = platform::set_autostart(enable) {
                    log::error!("Failed to toggle Launch at Login: {}", err);
                    autostart_item.set_checked(!enable);
                }
            } else if menu_event.id == optimize_now_id {
                match monitor.poll_once(&config, true) {
                    Ok(Some(summary)) => {
                        update_tray_labels(
                            &status_item,
                            &session_item,
                            &summary.display_short(),
                            &monitor.stats,
                        );
                    }
                    Ok(None) => {
                        status_item.set_text("Last: Clipboard already optimal or no image");
                    }
                    Err(err) => {
                        log::warn!("Manual clipboard optimization error: {}", err);
                    }
                }
            }
        }

        if let Event::NewEvents(_) | Event::MainEventsCleared = event {
            let now = Instant::now();
            if now >= next_poll {
                if !paused {
                    match monitor.poll_once(&config, false) {
                        Ok(Some(summary)) => {
                            update_tray_labels(
                                &status_item,
                                &session_item,
                                &summary.display_short(),
                                &monitor.stats,
                            );
                        }
                        Ok(None) => {}
                        Err(err) => {
                            log::debug!("Clipboard poll warning: {}", err);
                        }
                    }
                }
                next_poll = Instant::now() + poll_interval;
                *control_flow = ControlFlow::WaitUntil(next_poll);
            }
        }
    });
}

fn update_tray_labels(
    status_item: &MenuItem,
    session_item: &MenuItem,
    last_short: &str,
    stats: &SessionStats,
) {
    status_item.set_text(format!("Last: {}", last_short));
    session_item.set_text(format!(
        "Session: {} img ({} / -{:.1}%)",
        stats.images_optimized,
        format_bytes(stats.total_saved_bytes() as usize),
        stats.total_saved_percent()
    ));
}

/// Generates a crisp 22x22 RGBA icon in memory (zero external asset files required).
/// Draws a rounded clipboard frame with an inward compression spark/arrow motif.
fn build_tray_icon() -> Result<Icon> {
    const SIZE: u32 = 22;
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];

    let mut set_px = |x: u32, y: u32, r: u8, g: u8, b: u8, a: u8| {
        if x < SIZE && y < SIZE {
            let idx = ((y * SIZE + x) * 4) as usize;
            rgba[idx] = r;
            rgba[idx + 1] = g;
            rgba[idx + 2] = b;
            rgba[idx + 3] = a;
        }
    };

    // Clipboard outer border (x: 4..=17, y: 4..=19)
    for x in 5..=16 {
        set_px(x, 4, 240, 245, 250, 255);
        set_px(x, 19, 240, 245, 250, 255);
    }
    for y in 5..=18 {
        set_px(4, y, 240, 245, 250, 255);
        set_px(17, y, 240, 245, 250, 255);
    }

    // Top clipboard clip (x: 8..=13, y: 2..=4)
    for x in 8..=13 {
        set_px(x, 2, 240, 245, 250, 255);
        set_px(x, 3, 240, 245, 250, 255);
    }

    // Inner compression arrows / bolt (emerald accent)
    let accent = (52u8, 211u8, 153u8, 255u8);
    for (x, y) in [
        (11, 7),
        (10, 8),
        (11, 8),
        (9, 9),
        (10, 9),
        (8, 10),
        (9, 10),
        (10, 10),
        (11, 10),
        (12, 10),
        (13, 10),
        (10, 11),
        (11, 11),
        (12, 11),
        (10, 12),
        (11, 12),
        (9, 13),
        (10, 13),
        (9, 14),
    ] {
        set_px(x, y, accent.0, accent.1, accent.2, accent.3);
    }

    Icon::from_rgba(rgba, SIZE, SIZE).context("Failed to construct RGBA tray icon")
}
