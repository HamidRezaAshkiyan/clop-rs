use crate::clipboard::{ClipboardMonitor, OptimizationSummary, SessionStats};
use crate::config::{AppConfig, OptimizationMode, OptimizationPreset};
use crate::context_menu;
use crate::downloads::DownloadsWatcher;
use crate::history::{HistorySource, OptimizationHistory};
use crate::hud::{FloatingHud, HudAction, HudPhase, RevertSource};
use crate::optimizer::{self, format_bytes, generate_thumbnail, DiskBackup};
use crate::ui::{self, AppEvent, DashboardWindow, SystemToggleKind, UiCommand, WorkerOutcome};
use crate::{pdf, platform, video};
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};

enum WorkerCmd {
    UpdateConfig(AppConfig),
    SetPaused(bool),
    OptimizeClipboardNow { from_ui: bool },
    Revert { source: RevertSource, from_ui: bool },
    Shutdown,
}

/// Runs the background event loop with:
/// - Native macOS Menu Bar / Windows System Tray icon (instant event dispatch)
/// - Graphical Dashboard & Settings window (Settings, Clipboard History, Drag & Drop File Optimizer)
/// - Floating HUD Panel (progress bar, before/after size comparison, `[REVERT]` button)
/// - Fully asynchronous background worker for Clipboard & Downloads folder optimization
/// - Single-instance guard (second launch focuses the existing Dashboard)
pub fn run_with_tray(mut config: AppConfig, open_ui_on_start: bool) -> Result<()> {
    let event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let activate_proxy = event_loop.create_proxy();
    let Some(_instance_guard) = platform::acquire_single_instance(move || {
        let _ = activate_proxy.send_event(AppEvent::ActivateExistingInstance);
    }) else {
        log::info!("Another clop-rs instance is already running; activated existing Dashboard.");
        return Ok(());
    };

    platform::configure_background_app();

    // Wire tray menu clicks directly into the tao event loop for zero-latency menu response
    let menu_proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let _ = menu_proxy.send_event(AppEvent::TrayMenu(event.id));
    }));

    let mut autostart_enabled = platform::is_autostart_enabled();
    let mut context_menu_enabled = context_menu::is_context_menu_enabled();

    // Ensure right-click context menu is installed by default on a background thread
    // (avoids blocking startup on macOS `pbs -update` or Windows `reg`).
    if !context_menu_enabled {
        let bg_proxy = event_loop.create_proxy();
        thread::spawn(move || {
            let res = context_menu::set_context_menu(true);
            let _ = bg_proxy.send_event(AppEvent::SystemToggleResult {
                kind: SystemToggleKind::ContextMenu,
                enabled: res.is_ok(),
                error: res.err().map(|e| e.to_string()),
            });
        });
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
        context_menu_enabled,
        None,
    );
    let show_hud_item = CheckMenuItem::new(
        "Show Floating HUD Panel",
        true,
        config.show_floating_hud,
        None,
    );
    let autostart_item = CheckMenuItem::new("Launch at Login", true, autostart_enabled, None);
    let version_item = MenuItem::new(
        format!("clop-rs v{}", env!("CARGO_PKG_VERSION")),
        false,
        None,
    );
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
        .with_tooltip(format!(
            "clop-rs v{} — Image, Video & PDF Optimizer",
            env!("CARGO_PKG_VERSION")
        ))
        .with_icon(icon)
        .build()
        .context("Failed to create system tray / menu bar icon")?;

    let mut hud = FloatingHud::new(&event_loop, config.show_floating_hud);
    let mut dashboard = DashboardWindow::new(proxy);
    let mut history = OptimizationHistory::load();
    let mut session_stats = SessionStats::default();

    // Spawn dedicated background worker thread for Clipboard + Downloads watching & optimization
    let (worker_tx, worker_rx) = mpsc::channel::<WorkerCmd>();
    let worker_proxy = event_loop.create_proxy();
    let initial_worker_cfg = config.clone();
    thread::spawn(move || run_background_worker(initial_worker_cfg, worker_rx, worker_proxy));

    if open_ui_on_start {
        dashboard.show(&event_loop);
    }

    let mut last_revert_source: Option<RevertSource> = None;
    let mut can_revert_clipboard = false;
    let mut paused = false;
    let hud_tick_interval = Duration::from_millis(40);

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
        // Keep the single-instance guard alive for the lifetime of the event loop
        let _ = &_instance_guard;

        // Zero-polling Wait when HUD is hidden; 25fps tick only while HUD is visible
        *control_flow = if matches!(hud.phase, HudPhase::Hidden) {
            ControlFlow::Wait
        } else {
            ControlFlow::WaitUntil(Instant::now() + hud_tick_interval)
        };

        // 0. Handle custom events from Tray Menu, WebView IPC, Single-Instance, and Background Workers
        if let Event::UserEvent(user_event) = event {
            match user_event {
                AppEvent::ActivateExistingInstance => {
                    dashboard.show(elwt);
                    dashboard.push_state(
                        &config,
                        &history,
                        paused,
                        can_revert_clipboard,
                        autostart_enabled,
                        context_menu_enabled,
                    );
                }
                AppEvent::SystemToggleResult {
                    kind,
                    enabled,
                    error,
                } => {
                    match kind {
                        SystemToggleKind::Autostart => {
                            if let Some(err) = error {
                                log::error!("Failed to toggle Launch at Login: {}", err);
                                autostart_item.set_checked(autostart_enabled);
                                dashboard.send_toast("Failed to update Launch at Login", "error");
                            } else {
                                autostart_enabled = enabled;
                                autostart_item.set_checked(enabled);
                            }
                        }
                        SystemToggleKind::ContextMenu => {
                            if let Some(err) = error {
                                log::error!("Failed to toggle context menu: {}", err);
                                context_menu_item.set_checked(context_menu_enabled);
                                dashboard.send_toast(
                                    "Failed to update right-click context menu",
                                    "error",
                                );
                            } else {
                                context_menu_enabled = enabled;
                                context_menu_item.set_checked(enabled);
                            }
                        }
                    }
                    if dashboard.is_open() {
                        dashboard.push_light_state(
                            &config,
                            paused,
                            can_revert_clipboard,
                            autostart_enabled,
                            context_menu_enabled,
                        );
                    }
                }
                AppEvent::Worker(outcome) => match *outcome {
                    WorkerOutcome::Optimizing {
                        label,
                        width,
                        height,
                        estimated_size,
                        thumbnail_64,
                    } => {
                        hud.show_optimizing(&label, width, height, estimated_size, thumbnail_64);
                    }
                    WorkerOutcome::Optimized {
                        source,
                        revert_source,
                        file_path,
                        summary,
                        stats,
                        can_revert_clipboard: can_rev_cb,
                        manual_from_ui,
                    } => {
                        last_revert_source = Some(revert_source);
                        can_revert_clipboard = can_rev_cb;
                        revert_item.set_enabled(true);
                        session_stats = *stats;
                        let summary = *summary;
                        history.record_summary(source, &summary, file_path.as_deref(), &config);
                        let _ = history.save();
                        update_tray_labels(
                            &status_item,
                            &session_item,
                            &summary.display_short(),
                            &session_stats,
                        );
                        if manual_from_ui {
                            dashboard.send_toast(
                                &format!("Clipboard optimized: {}", summary.display_short()),
                                "success",
                            );
                        }
                        hud.show_completed(summary, revert_source);
                        if dashboard.is_open() {
                            dashboard.push_state(
                                &config,
                                &history,
                                paused,
                                can_revert_clipboard,
                                autostart_enabled,
                                context_menu_enabled,
                            );
                        }
                    }
                    WorkerOutcome::Skipped {
                        manual_from_ui,
                        manual_from_tray,
                        message,
                    } => {
                        hud.hide();
                        if manual_from_ui {
                            dashboard.send_toast(&message, "info");
                        }
                        if manual_from_tray {
                            status_item.set_text(format!("Last: {}", message));
                        }
                    }
                    WorkerOutcome::Error {
                        manual_from_ui,
                        error,
                    } => {
                        hud.hide();
                        log::warn!("Background optimization error: {}", error);
                        if manual_from_ui {
                            dashboard
                                .send_toast(&format!("Optimization error: {}", error), "error");
                        }
                    }
                    WorkerOutcome::Reverted {
                        label,
                        restored_size,
                        can_revert_clipboard: can_rev_cb,
                        from_ui,
                    } => {
                        can_revert_clipboard = can_rev_cb;
                        last_revert_source = None;
                        revert_item.set_enabled(false);
                        status_item.set_text(format!(
                            "Reverted {} to original ({})",
                            label,
                            format_bytes(restored_size)
                        ));
                        hud.show_reverted(&label, restored_size);
                        if from_ui {
                            dashboard
                                .send_toast(&format!("Reverted {} to original", label), "info");
                        }
                        if dashboard.is_open() {
                            dashboard.push_light_state(
                                &config,
                                paused,
                                can_revert_clipboard,
                                autostart_enabled,
                                context_menu_enabled,
                            );
                        }
                    }
                },
                AppEvent::TrayMenu(menu_id) => {
                    if menu_id == quit_id {
                        log::info!("Quitting clop-rs via tray menu");
                        let _ = worker_tx.send(WorkerCmd::Shutdown);
                        *control_flow = ControlFlow::Exit;
                        return;
                    } else if menu_id == open_dashboard_id {
                        dashboard.show(elwt);
                        dashboard.push_state(
                            &config,
                            &history,
                            paused,
                            can_revert_clipboard,
                            autostart_enabled,
                            context_menu_enabled,
                        );
                    } else if menu_id == pause_id {
                        paused = pause_item.is_checked();
                        let _ = worker_tx.send(WorkerCmd::SetPaused(paused));
                        if paused {
                            status_item.set_text("clop-rs — Paused");
                        } else {
                            status_item.set_text("clop-rs — Watching Clipboard");
                        }
                        if dashboard.is_open() {
                            dashboard.push_light_state(
                                &config,
                                paused,
                                can_revert_clipboard,
                                autostart_enabled,
                                context_menu_enabled,
                            );
                        }
                    } else if menu_id == smart_mode_id {
                        if smart_mode_item.is_checked() {
                            config.apply_preset(OptimizationPreset::Balanced);
                        } else {
                            config.apply_preset(OptimizationPreset::Lossless);
                        }
                        let _ = worker_tx.send(WorkerCmd::UpdateConfig(config.clone()));
                        let _ = config.save();
                        if dashboard.is_open() {
                            dashboard.push_light_state(
                                &config,
                                paused,
                                can_revert_clipboard,
                                autostart_enabled,
                                context_menu_enabled,
                            );
                        }
                        log::info!("Switched optimization mode to {:?}", config.mode);
                    } else if menu_id == strip_exif_id {
                        config.strip_metadata = strip_exif_item.is_checked();
                        let _ = worker_tx.send(WorkerCmd::UpdateConfig(config.clone()));
                        let _ = config.save();
                        if dashboard.is_open() {
                            dashboard.push_light_state(
                                &config,
                                paused,
                                can_revert_clipboard,
                                autostart_enabled,
                                context_menu_enabled,
                            );
                        }
                    } else if menu_id == watch_downloads_id {
                        config.watch_downloads = watch_downloads_item.is_checked();
                        let _ = worker_tx.send(WorkerCmd::UpdateConfig(config.clone()));
                        let _ = config.save();
                        if dashboard.is_open() {
                            dashboard.push_light_state(
                                &config,
                                paused,
                                can_revert_clipboard,
                                autostart_enabled,
                                context_menu_enabled,
                            );
                        }
                    } else if menu_id == context_menu_id {
                        let enable = context_menu_item.is_checked();
                        let bg_proxy = dashboard.proxy_clone();
                        thread::spawn(move || {
                            let res = context_menu::set_context_menu(enable);
                            let _ = bg_proxy.send_event(AppEvent::SystemToggleResult {
                                kind: SystemToggleKind::ContextMenu,
                                enabled: enable,
                                error: res.err().map(|e| e.to_string()),
                            });
                        });
                    } else if menu_id == show_hud_id {
                        config.show_floating_hud = show_hud_item.is_checked();
                        hud.set_enabled(config.show_floating_hud, elwt);
                        let _ = config.save();
                        if dashboard.is_open() {
                            dashboard.push_light_state(
                                &config,
                                paused,
                                can_revert_clipboard,
                                autostart_enabled,
                                context_menu_enabled,
                            );
                        }
                    } else if menu_id == autostart_id {
                        let enable = autostart_item.is_checked();
                        let bg_proxy = dashboard.proxy_clone();
                        thread::spawn(move || {
                            let res = platform::set_autostart(enable);
                            let _ = bg_proxy.send_event(AppEvent::SystemToggleResult {
                                kind: SystemToggleKind::Autostart,
                                enabled: enable,
                                error: res.err().map(|e| e.to_string()),
                            });
                        });
                    } else if menu_id == revert_id {
                        if let Some(src) = last_revert_source.take() {
                            let _ = worker_tx.send(WorkerCmd::Revert {
                                source: src,
                                from_ui: false,
                            });
                        }
                    } else if menu_id == optimize_now_id {
                        let _ = worker_tx.send(WorkerCmd::OptimizeClipboardNow { from_ui: false });
                    }
                }
                AppEvent::UiFileHover(active) => {
                    dashboard.send_drag_hover(active);
                }
                AppEvent::UiFileDrop(paths) => {
                    dashboard.send_files_added(paths);
                }
                AppEvent::FileProgress(mut update) => {
                    if let Some(backup) = update.backup.take() {
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
                        session_stats.images_optimized += 1;
                        session_stats.total_original_bytes += summary.original_size as u64;
                        session_stats.total_optimized_bytes += summary.optimized_size as u64;
                        session_stats.last_summary = Some(summary.clone());
                        update_tray_labels(
                            &status_item,
                            &session_item,
                            &summary.display_short(),
                            &session_stats,
                        );
                    }
                    dashboard.send_file_progress(&update);
                    dashboard.push_state(
                        &config,
                        &history,
                        paused,
                        can_revert_clipboard,
                        autostart_enabled,
                        context_menu_enabled,
                    );
                }
                AppEvent::UiIpc(raw_json) => match serde_json::from_str::<UiCommand>(&raw_json) {
                    Ok(cmd) => match cmd {
                        UiCommand::Ready => {
                            dashboard.push_state(
                                &config,
                                &history,
                                paused,
                                can_revert_clipboard,
                                autostart_enabled,
                                context_menu_enabled,
                            );
                        }
                        UiCommand::UpdateConfig { config: new_cfg } => {
                            config = new_cfg;
                            smart_mode_item.set_checked(config.mode == OptimizationMode::Smart);
                            strip_exif_item.set_checked(config.strip_metadata);
                            watch_downloads_item.set_checked(config.watch_downloads);
                            show_hud_item.set_checked(config.show_floating_hud);
                            hud.set_enabled(config.show_floating_hud, elwt);
                            let _ = worker_tx.send(WorkerCmd::UpdateConfig(config.clone()));
                            let _ = config.save();
                            dashboard.push_light_state(
                                &config,
                                paused,
                                can_revert_clipboard,
                                autostart_enabled,
                                context_menu_enabled,
                            );
                        }
                        UiCommand::ApplyPreset { preset } => {
                            config.apply_preset(preset);
                            smart_mode_item.set_checked(config.mode == OptimizationMode::Smart);
                            let _ = worker_tx.send(WorkerCmd::UpdateConfig(config.clone()));
                            let _ = config.save();
                            dashboard.push_light_state(
                                &config,
                                paused,
                                can_revert_clipboard,
                                autostart_enabled,
                                context_menu_enabled,
                            );
                            dashboard.send_toast(
                                &format!("Preset set to {}", preset.display_name()),
                                "success",
                            );
                        }
                        UiCommand::ApplyQuality { quality } => {
                            config.apply_quality_level(quality);
                            smart_mode_item.set_checked(true);
                            let _ = worker_tx.send(WorkerCmd::UpdateConfig(config.clone()));
                            let _ = config.save();
                            dashboard.push_light_state(
                                &config,
                                paused,
                                can_revert_clipboard,
                                autostart_enabled,
                                context_menu_enabled,
                            );
                        }
                        UiCommand::ResetConfig => {
                            config = AppConfig::default();
                            smart_mode_item.set_checked(config.mode == OptimizationMode::Smart);
                            strip_exif_item.set_checked(config.strip_metadata);
                            watch_downloads_item.set_checked(config.watch_downloads);
                            show_hud_item.set_checked(config.show_floating_hud);
                            hud.set_enabled(config.show_floating_hud, elwt);
                            let _ = worker_tx.send(WorkerCmd::UpdateConfig(config.clone()));
                            let _ = config.save();
                            dashboard.push_light_state(
                                &config,
                                paused,
                                can_revert_clipboard,
                                autostart_enabled,
                                context_menu_enabled,
                            );
                            dashboard.send_toast("Reset settings to defaults", "info");
                        }
                        UiCommand::ToggleAutostart { enabled } => {
                            let bg_proxy = dashboard.proxy_clone();
                            thread::spawn(move || {
                                let res = platform::set_autostart(enabled);
                                let _ = bg_proxy.send_event(AppEvent::SystemToggleResult {
                                    kind: SystemToggleKind::Autostart,
                                    enabled,
                                    error: res.err().map(|e| e.to_string()),
                                });
                            });
                        }
                        UiCommand::ToggleContextMenu { enabled } => {
                            let bg_proxy = dashboard.proxy_clone();
                            thread::spawn(move || {
                                let res = context_menu::set_context_menu(enabled);
                                let _ = bg_proxy.send_event(AppEvent::SystemToggleResult {
                                    kind: SystemToggleKind::ContextMenu,
                                    enabled,
                                    error: res.err().map(|e| e.to_string()),
                                });
                            });
                        }
                        UiCommand::TogglePause { paused: new_paused } => {
                            paused = new_paused;
                            pause_item.set_checked(paused);
                            let _ = worker_tx.send(WorkerCmd::SetPaused(paused));
                            if paused {
                                status_item.set_text("clop-rs — Paused");
                            } else {
                                status_item.set_text("clop-rs — Watching Clipboard");
                            }
                            dashboard.push_light_state(
                                &config,
                                paused,
                                can_revert_clipboard,
                                autostart_enabled,
                                context_menu_enabled,
                            );
                        }
                        UiCommand::PickDownloadsDir => {
                            if let Some(dir) = ui::pick_folder("Select Folder to Auto-Optimize") {
                                config.downloads_dir = Some(dir);
                                let _ = worker_tx.send(WorkerCmd::UpdateConfig(config.clone()));
                                let _ = config.save();
                                dashboard.push_light_state(
                                    &config,
                                    paused,
                                    can_revert_clipboard,
                                    autostart_enabled,
                                    context_menu_enabled,
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
                            if let Some(backup) = dashboard.file_backups.remove(&path) {
                                match backup.restore() {
                                    Ok((_, len)) => {
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
                            let _ =
                                worker_tx.send(WorkerCmd::OptimizeClipboardNow { from_ui: true });
                        }
                        UiCommand::RevertClipboard => {
                            let _ = worker_tx.send(WorkerCmd::Revert {
                                source: RevertSource::Clipboard,
                                from_ui: true,
                            });
                        }
                        UiCommand::ClearHistory => {
                            history.clear();
                            let _ = history.save();
                            dashboard.push_state(
                                &config,
                                &history,
                                paused,
                                can_revert_clipboard,
                                autostart_enabled,
                                context_menu_enabled,
                            );
                            dashboard.send_toast("Cleared optimization history", "info");
                        }
                    },
                    Err(err) => {
                        log::debug!("Ignored unknown UI IPC command: {}", err);
                    }
                },
            }
            // Re-evaluate control flow if HUD just became visible
            if !matches!(hud.phase, HudPhase::Hidden) {
                *control_flow = ControlFlow::WaitUntil(Instant::now() + hud_tick_interval);
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
                        let _ = worker_tx.send(WorkerCmd::Revert {
                            source,
                            from_ui: false,
                        });
                    }
                    HudAction::Dismiss | HudAction::None => {}
                }
            }
        }

        // 2. Smooth HUD animation tick (only runs when HUD is visible)
        if let Event::NewEvents(_) | Event::MainEventsCleared = event {
            if !matches!(hud.phase, HudPhase::Hidden) {
                hud.tick();
                if matches!(hud.phase, HudPhase::Hidden) {
                    *control_flow = ControlFlow::Wait;
                }
            }
        }
    });
}

fn run_background_worker(
    mut config: AppConfig,
    rx: mpsc::Receiver<WorkerCmd>,
    proxy: EventLoopProxy<AppEvent>,
) {
    let mut monitor = ClipboardMonitor::new();
    let mut downloads = DownloadsWatcher::new(if config.watch_downloads {
        config.effective_downloads_dir()
    } else {
        None
    });
    let mut paused = false;

    loop {
        let poll_interval = Duration::from_millis(config.poll_interval_ms.max(100));
        match rx.recv_timeout(poll_interval) {
            Ok(WorkerCmd::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(WorkerCmd::UpdateConfig(new_cfg)) => {
                config = new_cfg;
                if config.watch_downloads {
                    downloads.set_directory(config.effective_downloads_dir());
                } else {
                    downloads.set_directory(None);
                }
            }
            Ok(WorkerCmd::SetPaused(p)) => {
                paused = p;
            }
            Ok(WorkerCmd::OptimizeClipboardNow { from_ui }) => {
                handle_clipboard_optimize(&mut monitor, &config, &proxy, true, from_ui, !from_ui);
            }
            Ok(WorkerCmd::Revert { source, from_ui }) => match source {
                RevertSource::Clipboard => match monitor.revert_last() {
                    Ok(Some(orig_size)) => {
                        let _ =
                            proxy.send_event(AppEvent::Worker(Box::new(WorkerOutcome::Reverted {
                                label: "Clipboard".to_string(),
                                restored_size: orig_size,
                                can_revert_clipboard: monitor.last_backup.is_some(),
                                from_ui,
                            })));
                    }
                    Ok(None) => {}
                    Err(err) => log::warn!("Failed to revert clipboard image: {}", err),
                },
                RevertSource::Downloads => match downloads.revert_last() {
                    Ok(Some((path, orig_size))) => {
                        let name = path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("Download")
                            .to_string();
                        let _ =
                            proxy.send_event(AppEvent::Worker(Box::new(WorkerOutcome::Reverted {
                                label: name,
                                restored_size: orig_size,
                                can_revert_clipboard: monitor.last_backup.is_some(),
                                from_ui,
                            })));
                    }
                    Ok(None) => {}
                    Err(err) => log::warn!("Failed to revert Downloads file: {}", err),
                },
            },
            Err(RecvTimeoutError::Timeout) => {
                if paused {
                    continue;
                }
                handle_clipboard_optimize(&mut monitor, &config, &proxy, false, false, false);

                if config.watch_downloads {
                    if let Some(pending_file) = downloads.inspect_next_file(&config) {
                        let file_path = pending_file.path.clone();
                        let _ = proxy.send_event(AppEvent::Worker(Box::new(
                            WorkerOutcome::Optimizing {
                                label: pending_file.file_name.clone(),
                                width: pending_file.width,
                                height: pending_file.height,
                                estimated_size: pending_file.original_size,
                                thumbnail_64: pending_file.thumbnail_64.clone(),
                            },
                        )));

                        match downloads.optimize_pending_file(pending_file, &config) {
                            Ok(Some(summary)) => {
                                monitor.stats.images_optimized += 1;
                                monitor.stats.total_original_bytes += summary.original_size as u64;
                                monitor.stats.total_optimized_bytes +=
                                    summary.optimized_size as u64;
                                monitor.stats.last_summary = Some(summary.clone());
                                let _ = proxy.send_event(AppEvent::Worker(Box::new(
                                    WorkerOutcome::Optimized {
                                        source: HistorySource::Downloads,
                                        revert_source: RevertSource::Downloads,
                                        file_path: Some(file_path),
                                        summary: Box::new(summary),
                                        stats: Box::new(monitor.stats.clone()),
                                        can_revert_clipboard: monitor.last_backup.is_some(),
                                        manual_from_ui: false,
                                    },
                                )));
                            }
                            Ok(None) => {
                                let _ = proxy.send_event(AppEvent::Worker(Box::new(
                                    WorkerOutcome::Skipped {
                                        manual_from_ui: false,
                                        manual_from_tray: false,
                                        message: String::new(),
                                    },
                                )));
                            }
                            Err(err) => {
                                let _ = proxy.send_event(AppEvent::Worker(Box::new(
                                    WorkerOutcome::Error {
                                        manual_from_ui: false,
                                        error: err.to_string(),
                                    },
                                )));
                            }
                        }
                    }
                }
            }
        }
    }
}

fn handle_clipboard_optimize(
    monitor: &mut ClipboardMonitor,
    config: &AppConfig,
    proxy: &EventLoopProxy<AppEvent>,
    force: bool,
    manual_from_ui: bool,
    manual_from_tray: bool,
) {
    if let Some(pending) = monitor.inspect_new_image(force) {
        let _ = proxy.send_event(AppEvent::Worker(Box::new(WorkerOutcome::Optimizing {
            label: "Clipboard".to_string(),
            width: pending.width,
            height: pending.height,
            estimated_size: pending.estimated_size,
            thumbnail_64: pending.thumbnail_64.clone(),
        })));
        match monitor.optimize_pending(pending, config) {
            Ok(Some(summary)) => {
                let _ = proxy.send_event(AppEvent::Worker(Box::new(WorkerOutcome::Optimized {
                    source: HistorySource::Clipboard,
                    revert_source: RevertSource::Clipboard,
                    file_path: None,
                    summary: Box::new(summary),
                    stats: Box::new(monitor.stats.clone()),
                    can_revert_clipboard: monitor.last_backup.is_some(),
                    manual_from_ui,
                })));
            }
            Ok(None) => {
                let _ = proxy.send_event(AppEvent::Worker(Box::new(WorkerOutcome::Skipped {
                    manual_from_ui,
                    manual_from_tray,
                    message: "Clipboard image is already optimal".to_string(),
                })));
            }
            Err(err) => {
                let _ = proxy.send_event(AppEvent::Worker(Box::new(WorkerOutcome::Error {
                    manual_from_ui,
                    error: err.to_string(),
                })));
            }
        }
    } else if manual_from_ui || manual_from_tray {
        let _ = proxy.send_event(AppEvent::Worker(Box::new(WorkerOutcome::Skipped {
            manual_from_ui,
            manual_from_tray,
            message: "No image found on clipboard".to_string(),
        })));
    }
}

enum FileHudEvent {
    Optimizing {
        file_name: String,
        orig_len: usize,
        thumb: Vec<u8>,
    },
    Done {
        last_summary: Option<OptimizationSummary>,
        backups: Vec<DiskBackup>,
    },
}

/// Runs a one-shot file optimization (triggered from the Finder / Explorer Right-Click Menu
/// `Optimize with clop-rs` or `clop-rs file --hud <paths>`) with a smooth, non-blocking Floating HUD Panel!
pub fn run_file_with_hud(
    paths: Vec<PathBuf>,
    output: Option<PathBuf>,
    config: AppConfig,
) -> Result<()> {
    let event_loop = EventLoopBuilder::<FileHudEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    platform::configure_background_app();

    let mut hud = FloatingHud::new(&event_loop, true);
    let mut backups: Vec<DiskBackup> = Vec::new();

    thread::spawn(move || {
        let mut history = OptimizationHistory::load();
        let mut local_backups: Vec<DiskBackup> = Vec::new();
        let mut last_summary: Option<OptimizationSummary> = None;

        for path in &paths {
            let file_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("File")
                .to_string();
            let orig_len = fs::metadata(path).map(|m| m.len() as usize).unwrap_or(0);
            let backup = if output.is_none() {
                DiskBackup::create(path).ok()
            } else {
                None
            };

            let thumb = if pdf::is_pdf_path(path) {
                pdf::procedural_pdf_icon_64()
            } else if video::is_video_path(path) {
                video::extract_video_thumbnail_64(path)
            } else if let Ok(img) = image::open(path) {
                let rgba = img.into_rgba8();
                generate_thumbnail(rgba.width(), rgba.height(), rgba.as_raw(), 64)
            } else {
                vec![0u8; 64 * 64 * 4]
            };

            let _ = proxy.send_event(FileHudEvent::Optimizing {
                file_name: file_name.clone(),
                orig_len,
                thumb: thumb.clone(),
            });

            if let Ok(res) = optimizer::optimize_file(path, output.as_deref(), &config) {
                let target_path = output.clone().unwrap_or_else(|| path.clone());
                if res.was_modified && output.is_none() {
                    if let Some(b) = backup {
                        local_backups.push(b);
                    }
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

        let _ = proxy.send_event(FileHudEvent::Done {
            last_summary,
            backups: local_backups,
        });
    });

    let mut finished_worker = false;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(40));

        if let Event::UserEvent(hud_ev) = event {
            match hud_ev {
                FileHudEvent::Optimizing {
                    file_name,
                    orig_len,
                    thumb,
                } => {
                    hud.show_optimizing(&file_name, 0, 0, orig_len, thumb);
                }
                FileHudEvent::Done {
                    last_summary,
                    backups: worker_backups,
                } => {
                    finished_worker = true;
                    backups = worker_backups;
                    if let Some(summary) = last_summary {
                        hud.show_completed(summary, RevertSource::Downloads);
                    } else {
                        *control_flow = ControlFlow::Exit;
                    }
                }
            }
            return;
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
                        for backup in backups.drain(..) {
                            if let Ok((_, restored_len)) = backup.restore() {
                                total_restored += restored_len;
                            }
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
            if finished_worker && matches!(hud.phase, HudPhase::Hidden) {
                *control_flow = ControlFlow::Exit;
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
