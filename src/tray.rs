use crate::clipboard::{ClipboardMonitor, OptimizationSummary, SessionStats};
use crate::config::{AppConfig, OptimizationMode, OptimizationPreset};
use crate::context_menu;
use crate::downloads::DownloadsWatcher;
use crate::history::{HistorySource, OptimizationHistory};
use crate::hud::{FloatingHud, HudAction, HudPhase, RevertSource};
use crate::optimizer::{self, format_bytes, generate_thumbnail};
use crate::ui::{self, AppEvent, DashboardWindow, UiCommand};
use crate::{pdf, platform, video};
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};

/// Runs the background event loop with:
/// - Native macOS Menu Bar / Windows System Tray icon
/// - Graphical Dashboard & Settings window (Settings, Clipboard History, Drag & Drop File Optimizer)
/// - Floating HUD Panel (progress bar, before/after size comparison, `[REVERT]` button)
/// - Automatic Clipboard & Downloads folder optimization (Images, PDFs, Videos)
/// - Finder / Windows Explorer Right-Click Context Menu toggle
pub fn run_with_tray(mut config: AppConfig, open_ui_on_start: bool) -> Result<()> {
    let event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    platform::configure_background_app();

    // Ensure the "Optimize with clop-rs" right-click context menu is installed by default
    if !context_menu::is_context_menu_enabled() {
        let _ = context_menu::set_context_menu(true);
    }

    let menu = Menu::new();

    let open_dashboard_item = MenuItem::new("Open Dashboard & Settings…", true, None);
    let status_item = MenuItem::new("clop-rs — Watching Clipboard", false, None);
    let session_item = MenuItem::new("Session: 0 files/images (0 B saved)", false, None);
    let optimize_now_item = MenuItem::new("Optimize Clipboard Now", true, None);
    let revert_item = MenuItem::new("Revert Last Optimization", false, None);

    let pause_item = CheckMenuItem::new("Pause Auto-Optimization", true, false, None);
    let smart_mode_item = CheckMenuItem::new(
        "Smart Mode (Palette + Oxipng)",
        true,
        config.mode == OptimizationMode::Smart,
        None,
    );
    let strip_exif_item =
        CheckMenuItem::new("Strip EXIF & Metadata", true, config.strip_metadata, None);
    let watch_downloads_item = CheckMenuItem::new(
        "Auto-Optimize Downloads Folder",
        true,
        config.watch_downloads,
        None,
    );
    let context_menu_item = CheckMenuItem::new(
        "Right-Click 'Optimize with clop-rs'",
        true,
        context_menu::is_context_menu_enabled(),
        None,
    );
    let show_hud_item = CheckMenuItem::new(
        "Show Floating HUD Panel",
        true,
        config.show_floating_hud,
        None,
    );
    let autostart_item = CheckMenuItem::new(
        "Launch at Login",
        true,
        platform::is_autostart_enabled(),
        None,
    );
    let version_item = MenuItem::new(format!("clop-rs v{}", env!("CARGO_PKG_VERSION")), false, None);
    let quit_item = MenuItem::new("Quit clop-rs", true, None);

    menu.append(&open_dashboard_item)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&status_item)?;
    menu.append(&session_item)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&optimize_now_item)?;
    menu.append(&revert_item)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&pause_item)?;
    menu.append(&smart_mode_item)?;
    menu.append(&strip_exif_item)?;
    menu.append(&watch_downloads_item)?;
    menu.append(&context_menu_item)?;
    menu.append(&show_hud_item)?;
    menu.append(&autostart_item)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&version_item)?;
    menu.append(&quit_item)?;

    let icon = build_tray_icon()?;
    let _tray_icon = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(format!("clop-rs v{} — Image, Video & PDF Optimizer", env!("CARGO_PKG_VERSION")))
        .with_icon(icon)
        .build()
        .context("Failed to create system tray / menu bar icon")?;

    let mut hud = FloatingHud::new(&event_loop, config.show_floating_hud);
    let mut dashboard = DashboardWindow::new(proxy);
    let mut history = OptimizationHistory::load();
    let mut monitor = ClipboardMonitor::new();
    let mut downloads = DownloadsWatcher::new(if config.watch_downloads {
        config.effective_downloads_dir()
    } else {
        None
    });

    if open_ui_on_start {
        dashboard.show(&event_loop);
    }

    let mut last_revert_source: Option<RevertSource> = None;
    let mut paused = false;
    let mut poll_interval = Duration::from_millis(config.poll_interval_ms.max(100));
    let hud_tick_interval = Duration::from_millis(80);
    let mut next_poll = Instant::now();

    let open_dashboard_id = open_dashboard_item.id().clone();
    let optimize_now_id = optimize_now_item.id().clone();
    let revert_id = revert_item.id().clone();
    let pause_id = pause_item.id().clone();
    let smart_mode_id = smart_mode_item.id().clone();
    let strip_exif_id = strip_exif_item.id().clone();
    let watch_downloads_id = watch_downloads_item.id().clone();
    let context_menu_id = context_menu_item.id().clone();
    let show_hud_id = show_hud_item.id().clone();
    let autostart_id = autostart_item.id().clone();
    let quit_id = quit_item.id().clone();

    log::info!(
        "clop-rs started (preset={:?}, mode={:?}, strip_exif={}, watch_downloads={}, hud={})",
        config.preset,
        config.mode,
        config.strip_metadata,
        config.watch_downloads,
        config.show_floating_hud
    );

    event_loop.run(move |event, elwt, control_flow| {
        let wakeup = if matches!(hud.phase, HudPhase::Hidden) {
            next_poll
        } else {
            next_poll.min(Instant::now() + hud_tick_interval)
        };
        *control_flow = ControlFlow::WaitUntil(wakeup);

        // 0. Handle custom events from Dashboard WebView, Drag & Drop, and Background File Optimizer
        if let Event::UserEvent(user_event) = event {
            match user_event {
                AppEvent::UiFileHover(active) => {
                    dashboard.send_drag_hover(active);
                }
                AppEvent::UiFileDrop(paths) => {
                    dashboard.send_files_added(paths);
                }
                AppEvent::FileProgress(mut update) => {
                    if let Some(backup) = update.backup_bytes.take() {
                        dashboard.file_backups.insert(update.path.clone(), backup);
                    }
                    if let Some(summary) = update.summary.take() {
                        history.record_summary(
                            HistorySource::File,
                            &summary,
                            Some(Path::new(&update.path)),
                            &config,
                        );
                        let _ = history.save();
                        monitor.stats.images_optimized += 1;
                        monitor.stats.total_original_bytes += summary.original_size as u64;
                        monitor.stats.total_optimized_bytes += summary.optimized_size as u64;
                        monitor.stats.last_summary = Some(summary.clone());
                        update_tray_labels(
                            &status_item,
                            &session_item,
                            &summary.display_short(),
                            &monitor.stats,
                        );
                    }
                    dashboard.send_file_progress(&update);
                    dashboard.push_state(&config, &history, paused, monitor.last_backup.is_some());
                }
                AppEvent::UiIpc(raw_json) => match serde_json::from_str::<UiCommand>(&raw_json) {
                    Ok(cmd) => match cmd {
                        UiCommand::Ready => {
                            dashboard.push_state(
                                &config,
                                &history,
                                paused,
                                monitor.last_backup.is_some(),
                            );
                        }
                        UiCommand::UpdateConfig { config: new_cfg } => {
                            config = new_cfg;
                            poll_interval = Duration::from_millis(config.poll_interval_ms.max(100));
                            smart_mode_item.set_checked(config.mode == OptimizationMode::Smart);
                            strip_exif_item.set_checked(config.strip_metadata);
                            watch_downloads_item.set_checked(config.watch_downloads);
                            show_hud_item.set_checked(config.show_floating_hud);
                            hud.set_enabled(config.show_floating_hud, elwt);
                            if config.watch_downloads {
                                downloads.set_directory(config.effective_downloads_dir());
                            } else {
                                downloads.set_directory(None);
                            }
                            let _ = config.save();
                            dashboard.push_state(
                                &config,
                                &history,
                                paused,
                                monitor.last_backup.is_some(),
                            );
                        }
                        UiCommand::ApplyPreset { preset } => {
                            config.apply_preset(preset);
                            smart_mode_item.set_checked(config.mode == OptimizationMode::Smart);
                            let _ = config.save();
                            dashboard.push_state(
                                &config,
                                &history,
                                paused,
                                monitor.last_backup.is_some(),
                            );
                            dashboard.send_toast(
                                &format!("Preset set to {}", preset.display_name()),
                                "success",
                            );
                        }
                        UiCommand::ApplyQuality { quality } => {
                            config.apply_quality_level(quality);
                            smart_mode_item.set_checked(true);
                            let _ = config.save();
                            dashboard.push_state(
                                &config,
                                &history,
                                paused,
                                monitor.last_backup.is_some(),
                            );
                        }
                        UiCommand::ResetConfig => {
                            config = AppConfig::default();
                            poll_interval = Duration::from_millis(config.poll_interval_ms.max(100));
                            smart_mode_item.set_checked(config.mode == OptimizationMode::Smart);
                            strip_exif_item.set_checked(config.strip_metadata);
                            watch_downloads_item.set_checked(config.watch_downloads);
                            show_hud_item.set_checked(config.show_floating_hud);
                            hud.set_enabled(config.show_floating_hud, elwt);
                            downloads.set_directory(None);
                            let _ = config.save();
                            dashboard.push_state(
                                &config,
                                &history,
                                paused,
                                monitor.last_backup.is_some(),
                            );
                            dashboard.send_toast("Reset settings to defaults", "info");
                        }
                        UiCommand::ToggleAutostart { enabled } => {
                            match platform::set_autostart(enabled) {
                                Ok(_) => autostart_item.set_checked(enabled),
                                Err(err) => {
                                    log::error!("Failed to toggle autostart: {}", err);
                                    dashboard
                                        .send_toast("Failed to update Launch at Login", "error");
                                }
                            }
                            dashboard.push_state(
                                &config,
                                &history,
                                paused,
                                monitor.last_backup.is_some(),
                            );
                        }
                        UiCommand::ToggleContextMenu { enabled } => {
                            match context_menu::set_context_menu(enabled) {
                                Ok(_) => context_menu_item.set_checked(enabled),
                                Err(err) => {
                                    log::error!("Failed to toggle context menu: {}", err);
                                    dashboard.send_toast(
                                        "Failed to update right-click context menu",
                                        "error",
                                    );
                                }
                            }
                            dashboard.push_state(
                                &config,
                                &history,
                                paused,
                                monitor.last_backup.is_some(),
                            );
                        }
                        UiCommand::TogglePause { paused: new_paused } => {
                            paused = new_paused;
                            pause_item.set_checked(paused);
                            if paused {
                                status_item.set_text("clop-rs — Paused");
                            } else {
                                status_item.set_text("clop-rs — Watching Clipboard");
                            }
                            dashboard.push_state(
                                &config,
                                &history,
                                paused,
                                monitor.last_backup.is_some(),
                            );
                        }
                        UiCommand::PickDownloadsDir => {
                            if let Some(dir) = ui::pick_folder("Select Folder to Auto-Optimize") {
                                config.downloads_dir = Some(dir.clone());
                                if config.watch_downloads {
                                    downloads.set_directory(Some(dir));
                                }
                                let _ = config.save();
                                dashboard.push_state(
                                    &config,
                                    &history,
                                    paused,
                                    monitor.last_backup.is_some(),
                                );
                            }
                        }
                        UiCommand::PickFiles => {
                            let picked = ui::pick_supported_files();
                            if !picked.is_empty() {
                                dashboard.send_files_added(picked);
                            }
                        }
                        UiCommand::PickFolder => {
                            if let Some(dir) = ui::pick_folder("Select Folder of Files to Optimize")
                            {
                                dashboard.send_files_added(vec![dir]);
                            }
                        }
                        UiCommand::OptimizeFiles { paths } => {
                            dashboard.spawn_optimize_files(paths, config.clone());
                        }
                        UiCommand::RevertFile { path } => {
                            if let Some(orig_bytes) = dashboard.file_backups.remove(&path) {
                                let len = orig_bytes.len();
                                match fs::write(&path, &orig_bytes) {
                                    Ok(_) => {
                                        dashboard.send_file_reverted(&path, len);
                                    }
                                    Err(err) => {
                                        dashboard.send_toast(
                                            &format!("Failed to revert: {}", err),
                                            "error",
                                        );
                                    }
                                }
                            }
                        }
                        UiCommand::OptimizeClipboardNow => {
                            if let Some(pending) = monitor.inspect_new_image(true) {
                                hud.show_optimizing(
                                    "Clipboard",
                                    pending.width,
                                    pending.height,
                                    pending.estimated_size,
                                    pending.thumbnail_64.clone(),
                                );
                                match monitor.optimize_pending(pending, &config) {
                                    Ok(Some(summary)) => {
                                        last_revert_source = Some(RevertSource::Clipboard);
                                        revert_item.set_enabled(true);
                                        history.record_summary(
                                            HistorySource::Clipboard,
                                            &summary,
                                            None,
                                            &config,
                                        );
                                        let _ = history.save();
                                        update_tray_labels(
                                            &status_item,
                                            &session_item,
                                            &summary.display_short(),
                                            &monitor.stats,
                                        );
                                        dashboard.send_toast(
                                            &format!(
                                                "Clipboard optimized: {}",
                                                summary.display_short()
                                            ),
                                            "success",
                                        );
                                        hud.show_completed(summary, RevertSource::Clipboard);
                                    }
                                    Ok(None) => {
                                        hud.hide();
                                        dashboard.send_toast(
                                            "Clipboard image is already optimal",
                                            "info",
                                        );
                                    }
                                    Err(err) => {
                                        hud.hide();
                                        dashboard.send_toast(
                                            &format!("Optimization error: {}", err),
                                            "error",
                                        );
                                    }
                                }
                            } else {
                                dashboard.send_toast("No image found on clipboard", "info");
                            }
                            dashboard.push_state(
                                &config,
                                &history,
                                paused,
                                monitor.last_backup.is_some(),
                            );
                        }
                        UiCommand::RevertClipboard => {
                            perform_revert(
                                RevertSource::Clipboard,
                                &mut monitor,
                                &mut downloads,
                                &mut hud,
                                &status_item,
                                &revert_item,
                            );
                            last_revert_source = None;
                            dashboard.push_state(
                                &config,
                                &history,
                                paused,
                                monitor.last_backup.is_some(),
                            );
                            dashboard.send_toast("Reverted clipboard to original", "info");
                        }
                        UiCommand::ClearHistory => {
                            history.clear();
                            let _ = history.save();
                            dashboard.push_state(
                                &config,
                                &history,
                                paused,
                                monitor.last_backup.is_some(),
                            );
                            dashboard.send_toast("Cleared optimization history", "info");
                        }
                    },
                    Err(err) => {
                        log::debug!("Ignored unknown UI IPC command: {}", err);
                    }
                },
            }
            return;
        }

        // 1. Handle Floating HUD & Dashboard window events
        if let Event::WindowEvent {
            window_id,
            ref event,
            ..
        } = event
        {
            if Some(window_id) == dashboard.window_id() {
                if let WindowEvent::CloseRequested = event {
                    dashboard.hide();
                }
            } else if Some(window_id) == hud.window_id() {
                match hud.handle_window_event(event) {
                    HudAction::Revert(source) => {
                        perform_revert(
                            source,
                            &mut monitor,
                            &mut downloads,
                            &mut hud,
                            &status_item,
                            &revert_item,
                        );
                        last_revert_source = None;
                        if dashboard.is_open() {
                            dashboard.push_state(
                                &config,
                                &history,
                                paused,
                                monitor.last_backup.is_some(),
                            );
                        }
                    }
                    HudAction::Dismiss | HudAction::None => {}
                }
            }
        }

        // 2. Handle System Tray / Menu Bar clicks
        while let Ok(menu_event) = MenuEvent::receiver().try_recv() {
            if menu_event.id == quit_id {
                log::info!("Quitting clop-rs via tray menu");
                *control_flow = ControlFlow::Exit;
                return;
            } else if menu_event.id == open_dashboard_id {
                dashboard.show(elwt);
                dashboard.push_state(&config, &history, paused, monitor.last_backup.is_some());
            } else if menu_event.id == pause_id {
                paused = pause_item.is_checked();
                if paused {
                    status_item.set_text("clop-rs — Paused");
                } else {
                    status_item.set_text("clop-rs — Watching Clipboard");
                }
                if dashboard.is_open() {
                    dashboard.push_state(&config, &history, paused, monitor.last_backup.is_some());
                }
            } else if menu_event.id == smart_mode_id {
                if smart_mode_item.is_checked() {
                    config.apply_preset(OptimizationPreset::Balanced);
                } else {
                    config.apply_preset(OptimizationPreset::Lossless);
                }
                let _ = config.save();
                if dashboard.is_open() {
                    dashboard.push_state(&config, &history, paused, monitor.last_backup.is_some());
                }
                log::info!("Switched optimization mode to {:?}", config.mode);
            } else if menu_event.id == strip_exif_id {
                config.strip_metadata = strip_exif_item.is_checked();
                let _ = config.save();
                if dashboard.is_open() {
                    dashboard.push_state(&config, &history, paused, monitor.last_backup.is_some());
                }
                log::info!("Set strip_metadata to {}", config.strip_metadata);
            } else if menu_event.id == watch_downloads_id {
                config.watch_downloads = watch_downloads_item.is_checked();
                if config.watch_downloads {
                    downloads.set_directory(config.effective_downloads_dir());
                } else {
                    downloads.set_directory(None);
                }
                let _ = config.save();
                if dashboard.is_open() {
                    dashboard.push_state(&config, &history, paused, monitor.last_backup.is_some());
                }
                log::info!("Set watch_downloads to {}", config.watch_downloads);
            } else if menu_event.id == context_menu_id {
                let enable = context_menu_item.is_checked();
                if let Err(err) = context_menu::set_context_menu(enable) {
                    log::error!("Failed to toggle context menu: {}", err);
                    context_menu_item.set_checked(!enable);
                }
                if dashboard.is_open() {
                    dashboard.push_state(&config, &history, paused, monitor.last_backup.is_some());
                }
            } else if menu_event.id == show_hud_id {
                config.show_floating_hud = show_hud_item.is_checked();
                hud.set_enabled(config.show_floating_hud, elwt);
                let _ = config.save();
                if dashboard.is_open() {
                    dashboard.push_state(&config, &history, paused, monitor.last_backup.is_some());
                }
            } else if menu_event.id == autostart_id {
                let enable = autostart_item.is_checked();
                if let Err(err) = platform::set_autostart(enable) {
                    log::error!("Failed to toggle Launch at Login: {}", err);
                    autostart_item.set_checked(!enable);
                }
                if dashboard.is_open() {
                    dashboard.push_state(&config, &history, paused, monitor.last_backup.is_some());
                }
            } else if menu_event.id == revert_id {
                if let Some(src) = last_revert_source.take() {
                    perform_revert(
                        src,
                        &mut monitor,
                        &mut downloads,
                        &mut hud,
                        &status_item,
                        &revert_item,
                    );
                    if dashboard.is_open() {
                        dashboard.push_state(
                            &config,
                            &history,
                            paused,
                            monitor.last_backup.is_some(),
                        );
                    }
                }
            } else if menu_event.id == optimize_now_id {
                if let Some(pending) = monitor.inspect_new_image(true) {
                    hud.show_optimizing(
                        "Clipboard",
                        pending.width,
                        pending.height,
                        pending.estimated_size,
                        pending.thumbnail_64.clone(),
                    );
                    match monitor.optimize_pending(pending, &config) {
                        Ok(Some(summary)) => {
                            last_revert_source = Some(RevertSource::Clipboard);
                            revert_item.set_enabled(true);
                            history.record_summary(
                                HistorySource::Clipboard,
                                &summary,
                                None,
                                &config,
                            );
                            let _ = history.save();
                            update_tray_labels(
                                &status_item,
                                &session_item,
                                &summary.display_short(),
                                &monitor.stats,
                            );
                            hud.show_completed(summary, RevertSource::Clipboard);
                            if dashboard.is_open() {
                                dashboard.push_state(
                                    &config,
                                    &history,
                                    paused,
                                    monitor.last_backup.is_some(),
                                );
                            }
                        }
                        Ok(None) => {
                            hud.hide();
                            status_item.set_text("Last: Clipboard already optimal");
                        }
                        Err(err) => {
                            hud.hide();
                            log::warn!("Manual clipboard optimization error: {}", err);
                        }
                    }
                } else {
                    status_item.set_text("Last: No image on clipboard");
                }
            }
        }

        // 3. Periodic poll & HUD animation tick
        if let Event::NewEvents(_) | Event::MainEventsCleared = event {
            hud.tick();

            let now = Instant::now();
            if now >= next_poll {
                if !paused {
                    if let Some(pending) = monitor.inspect_new_image(false) {
                        hud.show_optimizing(
                            "Clipboard",
                            pending.width,
                            pending.height,
                            pending.estimated_size,
                            pending.thumbnail_64.clone(),
                        );
                        match monitor.optimize_pending(pending, &config) {
                            Ok(Some(summary)) => {
                                last_revert_source = Some(RevertSource::Clipboard);
                                revert_item.set_enabled(true);
                                history.record_summary(
                                    HistorySource::Clipboard,
                                    &summary,
                                    None,
                                    &config,
                                );
                                let _ = history.save();
                                update_tray_labels(
                                    &status_item,
                                    &session_item,
                                    &summary.display_short(),
                                    &monitor.stats,
                                );
                                hud.show_completed(summary, RevertSource::Clipboard);
                                if dashboard.is_open() {
                                    dashboard.push_state(
                                        &config,
                                        &history,
                                        paused,
                                        monitor.last_backup.is_some(),
                                    );
                                }
                            }
                            Ok(None) => {
                                hud.hide();
                            }
                            Err(err) => {
                                hud.hide();
                                log::debug!("Clipboard optimization warning: {}", err);
                            }
                        }
                    }

                    if config.watch_downloads {
                        if let Some(pending_file) = downloads.inspect_next_file(&config) {
                            let file_path = pending_file.path.clone();
                            hud.show_optimizing(
                                &pending_file.file_name,
                                pending_file.width,
                                pending_file.height,
                                pending_file.original_bytes.len(),
                                pending_file.thumbnail_64.clone(),
                            );
                            match downloads.optimize_pending_file(pending_file, &config) {
                                Ok(Some(summary)) => {
                                    last_revert_source = Some(RevertSource::Downloads);
                                    revert_item.set_enabled(true);
                                    history.record_summary(
                                        HistorySource::Downloads,
                                        &summary,
                                        Some(&file_path),
                                        &config,
                                    );
                                    let _ = history.save();
                                    monitor.stats.images_optimized += 1;
                                    monitor.stats.total_original_bytes +=
                                        summary.original_size as u64;
                                    monitor.stats.total_optimized_bytes +=
                                        summary.optimized_size as u64;
                                    monitor.stats.last_summary = Some(summary.clone());
                                    update_tray_labels(
                                        &status_item,
                                        &session_item,
                                        &summary.display_short(),
                                        &monitor.stats,
                                    );
                                    hud.show_completed(summary, RevertSource::Downloads);
                                    if dashboard.is_open() {
                                        dashboard.push_state(
                                            &config,
                                            &history,
                                            paused,
                                            monitor.last_backup.is_some(),
                                        );
                                    }
                                }
                                Ok(None) => {
                                    hud.hide();
                                }
                                Err(err) => {
                                    hud.hide();
                                    log::debug!("Downloads optimization warning: {}", err);
                                }
                            }
                        }
                    }
                }

                next_poll = Instant::now() + poll_interval;
                let wakeup = if matches!(hud.phase, HudPhase::Hidden) {
                    next_poll
                } else {
                    next_poll.min(Instant::now() + hud_tick_interval)
                };
                *control_flow = ControlFlow::WaitUntil(wakeup);
            }
        }
    });
}

/// Runs a one-shot file optimization (triggered from the Finder / Explorer Right-Click Menu
/// `Optimize with clop-rs` or `clop-rs file --hud <paths>`) with the Floating HUD Panel!
pub fn run_file_with_hud(
    paths: Vec<PathBuf>,
    output: Option<PathBuf>,
    config: AppConfig,
) -> Result<()> {
    let event_loop = EventLoopBuilder::new().build();
    platform::configure_background_app();

    let mut hud = FloatingHud::new(&event_loop, true);
    let mut history = OptimizationHistory::load();
    let mut backups: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    let mut started = false;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(80));

        if !started {
            started = true;
            let mut last_summary: Option<OptimizationSummary> = None;

            for path in &paths {
                let file_name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("File")
                    .to_string();
                let orig_bytes = fs::read(path).unwrap_or_default();
                let orig_len = orig_bytes.len();

                let thumb = if pdf::is_pdf_path(path) {
                    pdf::procedural_pdf_icon_64()
                } else if video::is_video_path(path) {
                    video::extract_video_thumbnail_64(path)
                } else if let Ok(img) = image::load_from_memory(&orig_bytes) {
                    let rgba = img.into_rgba8();
                    generate_thumbnail(rgba.width(), rgba.height(), rgba.as_raw(), 64)
                } else {
                    vec![0u8; 64 * 64 * 4]
                };

                hud.show_optimizing(&file_name, 0, 0, orig_len, thumb.clone());

                if let Ok(res) = optimizer::optimize_file(path, output.as_deref(), &config) {
                    let target_path = output.clone().unwrap_or_else(|| path.clone());
                    if res.was_modified && output.is_none() && !orig_bytes.is_empty() {
                        backups.push((target_path.clone(), orig_bytes));
                    }
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
                        thumbnail_64: thumb,
                    };
                    if res.was_modified {
                        history.record_summary(
                            HistorySource::File,
                            &summary,
                            Some(&target_path),
                            &config,
                        );
                        let _ = history.save();
                    }
                    last_summary = Some(summary);
                }
            }

            if let Some(summary) = last_summary {
                hud.show_completed(summary, RevertSource::Downloads);
            } else {
                *control_flow = ControlFlow::Exit;
                return;
            }
        }

        if let Event::WindowEvent {
            window_id,
            ref event,
            ..
        } = event
        {
            if Some(window_id) == hud.window_id() {
                match hud.handle_window_event(event) {
                    HudAction::Revert(_) => {
                        let mut total_restored = 0usize;
                        for (path, orig_bytes) in backups.drain(..) {
                            total_restored += orig_bytes.len();
                            let _ = fs::write(path, orig_bytes);
                        }
                        if total_restored > 0 {
                            hud.show_reverted("File", total_restored);
                        }
                    }
                    HudAction::Dismiss => {
                        *control_flow = ControlFlow::Exit;
                        return;
                    }
                    HudAction::None => {}
                }
            }
        }

        if let Event::NewEvents(_) | Event::MainEventsCleared = event {
            hud.tick();
            if matches!(hud.phase, HudPhase::Hidden) {
                *control_flow = ControlFlow::Exit;
            }
        }
    });
}

fn perform_revert(
    source: RevertSource,
    monitor: &mut ClipboardMonitor,
    downloads: &mut DownloadsWatcher,
    hud: &mut FloatingHud,
    status_item: &MenuItem,
    revert_item: &MenuItem,
) {
    match source {
        RevertSource::Clipboard => match monitor.revert_last() {
            Ok(Some(orig_size)) => {
                revert_item.set_enabled(false);
                status_item.set_text(format!(
                    "Reverted clipboard to original ({})",
                    format_bytes(orig_size)
                ));
                hud.show_reverted("Clipboard", orig_size);
            }
            Ok(None) => {}
            Err(err) => log::warn!("Failed to revert clipboard image: {}", err),
        },
        RevertSource::Downloads => match downloads.revert_last() {
            Ok(Some((path, orig_size))) => {
                revert_item.set_enabled(false);
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("Download");
                status_item.set_text(format!(
                    "Reverted {} to original ({})",
                    name,
                    format_bytes(orig_size)
                ));
                hud.show_reverted(name, orig_size);
            }
            Ok(None) => {}
            Err(err) => log::warn!("Failed to revert Downloads file: {}", err),
        },
    }
}

fn update_tray_labels(
    status_item: &MenuItem,
    session_item: &MenuItem,
    last_short: &str,
    stats: &SessionStats,
) {
    status_item.set_text(format!("Last: {}", last_short));
    session_item.set_text(format!(
        "Session: {} items ({} / -{:.1}%)",
        stats.images_optimized,
        format_bytes(stats.total_saved_bytes() as usize),
        stats.total_saved_percent()
    ));
}

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

    for x in 5..=16 {
        set_px(x, 4, 240, 245, 250, 255);
        set_px(x, 19, 240, 245, 250, 255);
    }
    for y in 5..=18 {
        set_px(4, y, 240, 245, 250, 255);
        set_px(17, y, 240, 245, 250, 255);
    }
    for x in 8..=13 {
        set_px(x, 2, 240, 245, 250, 255);
        set_px(x, 3, 240, 245, 250, 255);
    }

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
