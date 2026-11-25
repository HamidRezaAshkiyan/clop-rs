use crate::clipboard::{OptimizationSummary, SessionStats};
use crate::config::{AppConfig, OptimizationPreset};
use crate::downloads::is_supported_media_path;
use crate::history::{HistorySource, OptimizationHistory};
use crate::hud::RevertSource;
use crate::optimizer::{
    self, generate_thumbnail, supported_formats_info, DiskBackup, SupportedFormatsInfo,
    SUPPORTED_DOCUMENT_EXTENSIONS, SUPPORTED_IMAGE_EXTENSIONS, SUPPORTED_VIDEO_EXTENSIONS,
};
use crate::updater::UpdateStatus;
use crate::{pdf, platform, video};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use tao::dpi::LogicalSize;
use tao::event_loop::{EventLoopProxy, EventLoopWindowTarget};
use tao::window::{Window, WindowBuilder, WindowId};
use tray_icon::menu::MenuId;
use wry::{DragDropEvent, WebView, WebViewBuilder};

const UI_HTML: &str = include_str!("ui.html");
pub const DASHBOARD_WIDTH: f64 = 820.0;
pub const DASHBOARD_HEIGHT: f64 = 660.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemToggleKind {
    Autostart,
    ContextMenu,
}

#[derive(Debug)]
pub enum WorkerOutcome {
    Optimizing {
        label: String,
        width: u32,
        height: u32,
        estimated_size: usize,
        thumbnail_64: Vec<u8>,
    },
    Optimized {
        source: HistorySource,
        revert_source: RevertSource,
        file_path: Option<PathBuf>,
        summary: Box<OptimizationSummary>,
        stats: Box<SessionStats>,
        can_revert_clipboard: bool,
        manual_from_ui: bool,
    },
    Skipped {
        manual_from_ui: bool,
        manual_from_tray: bool,
        message: String,
    },
    Error {
        manual_from_ui: bool,
        error: String,
    },
    Reverted {
        label: String,
        restored_size: usize,
        can_revert_clipboard: bool,
        from_ui: bool,
    },
}

/// Custom events routed through the `tao` event loop between the WebView, drag-and-drop,
/// tray menu, single-instance listener, and background worker threads.
#[derive(Debug)]
pub enum AppEvent {
    UiIpc(String),
    UiFileHover(bool),
    UiFileDrop(Vec<PathBuf>),
    FileProgress(Box<FileProgressUpdate>),
    TrayMenu(MenuId),
    ActivateExistingInstance,
    Worker(Box<WorkerOutcome>),
    SystemToggleResult {
        kind: SystemToggleKind,
        enabled: bool,
        error: Option<String>,
    },
    UpdateCheckResult {
        status: Box<UpdateStatus>,
        manual: bool,
    },
}

#[derive(Debug, Serialize)]
pub struct FileProgressUpdate {
    pub path: String,
    pub status: String,
    pub original_size: usize,
    pub optimized_size: usize,
    pub saved_bytes: usize,
    pub saved_percent: f64,
    pub exif_stripped: bool,
    pub elapsed_ms: u64,
    pub can_revert: bool,
    pub error: Option<String>,
    #[serde(skip)]
    pub backup: Option<DiskBackup>,
    #[serde(skip)]
    pub summary: Option<OptimizationSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DroppedFileInfo {
    pub path: String,
    pub name: String,
    pub ext: String,
    pub size: u64,
    pub supported: bool,
    pub category: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UiCommand {
    Ready,
    UpdateConfig { config: AppConfig },
    ApplyPreset { preset: OptimizationPreset },
    ApplyQuality { quality: u8 },
    ResetConfig,
    ToggleAutostart { enabled: bool },
    ToggleContextMenu { enabled: bool },
    TogglePause { paused: bool },
    PickDownloadsDir,
    PickFiles,
    PickFolder,
    OptimizeFiles { paths: Vec<String> },
    RevertFile { path: String },
    OptimizeClipboardNow,
    RevertClipboard,
    ClearHistory,
    CheckForUpdates,
    OpenReleaseUrl { url: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct SystemUiState {
    pub version: String,
    pub autostart: bool,
    pub context_menu: bool,
    pub paused: bool,
    pub can_revert_clipboard: bool,
    pub effective_downloads_dir: String,
    pub update: Option<UpdateStatus>,
}

/// Graphical Dashboard window hosting Settings, Clipboard History, and Drag-and-Drop File Optimizer.
pub struct DashboardWindow {
    window: Option<Window>,
    webview: Option<WebView>,
    proxy: EventLoopProxy<AppEvent>,
    /// Cached supported format & external tool availability (avoids scanning PATH on every UI click).
    formats_cache: SupportedFormatsInfo,
    /// Latest known GitHub Releases update status.
    update_status: Option<UpdateStatus>,
    /// Per-file disk-backed backups so the user can revert files optimized in the UI without RAM spikes.
    pub file_backups: HashMap<String, DiskBackup>,
}

impl DashboardWindow {
    pub fn new(proxy: EventLoopProxy<AppEvent>) -> Self {
        Self {
            window: None,
            webview: None,
            proxy,
            formats_cache: supported_formats_info(),
            update_status: None,
            file_backups: HashMap::new(),
        }
    }

    pub fn set_update_status(&mut self, status: UpdateStatus) {
        self.update_status = Some(status);
    }

    pub fn send_update_status(&self, status: &UpdateStatus) {
        self.send_json(&serde_json::json!({
            "type": "update_status",
            "update": status,
        }));
    }

    pub fn window_id(&self) -> Option<WindowId> {
        self.window.as_ref().map(|w| w.id())
    }

    pub fn proxy_clone(&self) -> EventLoopProxy<AppEvent> {
        self.proxy.clone()
    }

    pub fn is_open(&self) -> bool {
        self.window.is_some()
    }

    /// Opens (or brings to front) the Dashboard & Settings window.
    pub fn show(&mut self, event_loop: &EventLoopWindowTarget<AppEvent>) {
        if let Some(ref win) = self.window {
            win.set_visible(true);
            win.set_focus();
            platform::activate_app();
            return;
        }

        let win = match WindowBuilder::new()
            .with_title("clop-rs — Dashboard & Settings")
            .with_inner_size(LogicalSize::new(DASHBOARD_WIDTH, DASHBOARD_HEIGHT))
            .with_min_inner_size(LogicalSize::new(660.0, 500.0))
            .with_resizable(true)
            .with_visible(true)
            .build(event_loop)
        {
            Ok(w) => w,
            Err(err) => {
                log::error!("Failed to create Dashboard window: {}", err);
                return;
            }
        };

        let ipc_proxy = self.proxy.clone();
        let dnd_proxy = self.proxy.clone();

        let webview = match WebViewBuilder::new()
            .with_html(UI_HTML)
            .with_ipc_handler(move |req| {
                let _ = ipc_proxy.send_event(AppEvent::UiIpc(req.body().to_string()));
            })
            .with_drag_drop_handler(move |event| {
                match event {
                    DragDropEvent::Enter { .. } | DragDropEvent::Over { .. } => {
                        let _ = dnd_proxy.send_event(AppEvent::UiFileHover(true));
                    }
                    DragDropEvent::Leave => {
                        let _ = dnd_proxy.send_event(AppEvent::UiFileHover(false));
                    }
                    DragDropEvent::Drop { paths, .. } => {
                        let _ = dnd_proxy.send_event(AppEvent::UiFileHover(false));
                        let _ = dnd_proxy.send_event(AppEvent::UiFileDrop(paths));
                    }
                    _ => {}
                }
                true
            })
            .build(&win)
        {
            Ok(wv) => wv,
            Err(err) => {
                log::error!("Failed to create Dashboard WebView: {}", err);
                return;
            }
        };

        win.set_focus();
        platform::activate_app();
        self.window = Some(win);
        self.webview = Some(webview);
    }

    /// Hides the Dashboard window (keeps tray app alive in the background).
    pub fn hide(&mut self) {
        if let Some(ref win) = self.window {
            win.set_visible(false);
        }
    }

    /// Sends an arbitrary JSON message object to `window.__clopReceive(...)` in the WebView.
    pub fn send_json<T: Serialize>(&self, payload: &T) {
        let Some(ref webview) = self.webview else {
            return;
        };
        if let Ok(json) = serde_json::to_string(payload) {
            let script = format!("window.__clopReceive && window.__clopReceive({});", json);
            let _ = webview.evaluate_script(&script);
        }
    }

    /// Pushes the complete current application state (config, history, stats, formats, system flags) to the UI.
    #[allow(clippy::too_many_arguments)]
    pub fn push_state(
        &self,
        config: &AppConfig,
        history: &OptimizationHistory,
        paused: bool,
        can_revert_clipboard: bool,
        autostart: bool,
        context_menu: bool,
    ) {
        let effective_downloads_dir = config
            .effective_downloads_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "~/Downloads".to_string());

        let payload = serde_json::json!({
            "type": "state",
            "config": config,
            "history": history.entries,
            "stats": history.stats(),
            "formats": &self.formats_cache,
            "system": SystemUiState {
                version: env!("CARGO_PKG_VERSION").to_string(),
                autostart,
                context_menu,
                paused,
                can_revert_clipboard,
                effective_downloads_dir,
                update: self.update_status.clone(),
            }
        });
        self.send_json(&payload);
    }

    /// Pushes only config & system flags (skipping the 200-entry base64 history array)
    /// so toggling settings or tray menu items is instantaneous.
    #[allow(clippy::too_many_arguments)]
    pub fn push_light_state(
        &self,
        config: &AppConfig,
        paused: bool,
        can_revert_clipboard: bool,
        autostart: bool,
        context_menu: bool,
    ) {
        let effective_downloads_dir = config
            .effective_downloads_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "~/Downloads".to_string());

        let payload = serde_json::json!({
            "type": "light_state",
            "config": config,
            "system": SystemUiState {
                version: env!("CARGO_PKG_VERSION").to_string(),
                autostart,
                context_menu,
                paused,
                can_revert_clipboard,
                effective_downloads_dir,
                update: self.update_status.clone(),
            }
        });
        self.send_json(&payload);
    }

    pub fn send_drag_hover(&self, active: bool) {
        self.send_json(&serde_json::json!({
            "type": "drag_hover",
            "active": active,
        }));
    }

    pub fn send_toast(&self, message: &str, kind: &str) {
        self.send_json(&serde_json::json!({
            "type": "toast",
            "message": message,
            "kind": kind,
        }));
    }

    pub fn send_files_added(&self, paths: Vec<PathBuf>) {
        let expanded = expand_paths(&paths);
        let files: Vec<DroppedFileInfo> = expanded.iter().map(|p| inspect_file_info(p)).collect();
        if files.is_empty() {
            return;
        }
        self.send_json(&serde_json::json!({
            "type": "files_added",
            "files": files,
        }));
    }

    pub fn send_file_progress(&self, update: &FileProgressUpdate) {
        let mut val = serde_json::to_value(update).unwrap_or_default();
        if let Some(obj) = val.as_object_mut() {
            obj.insert("type".to_string(), serde_json::json!("file_progress"));
        }
        self.send_json(&val);
    }

    pub fn send_file_reverted(&self, path: &str, restored_size: usize) {
        self.send_json(&serde_json::json!({
            "type": "file_reverted",
            "path": path,
            "restored_size": restored_size,
        }));
    }

    /// Spawns a background worker thread to optimize a batch of files and stream progress updates.
    pub fn spawn_optimize_files(&self, paths: Vec<String>, config: AppConfig) {
        let proxy = self.proxy.clone();
        thread::spawn(move || {
            for path_str in paths {
                let path = PathBuf::from(&path_str);
                let backup = DiskBackup::create(&path).ok();

                match optimizer::optimize_file(&path, None, &config) {
                    Ok(res) => {
                        let file_name = path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("File")
                            .to_string();

                        let thumb = if pdf::is_pdf_path(&path) {
                            pdf::procedural_pdf_icon_64()
                        } else if video::is_video_path(&path) {
                            res.rgba_bytes.clone()
                        } else if res.width > 0 && res.height > 0 {
                            generate_thumbnail(res.width, res.height, &res.rgba_bytes, 64)
                        } else {
                            vec![0u8; 64 * 64 * 4]
                        };

                        let status = if res.was_modified { "done" } else { "optimal" };
                        let can_revert = res.was_modified && backup.is_some();

                        let summary = if res.was_modified {
                            Some(OptimizationSummary {
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
                            })
                        } else {
                            None
                        };

                        let _ = proxy.send_event(AppEvent::FileProgress(Box::new(
                            FileProgressUpdate {
                                path: path_str,
                                status: status.to_string(),
                                original_size: res.original_size,
                                optimized_size: res.optimized_size,
                                saved_bytes: res.saved_bytes(),
                                saved_percent: res.saved_percent(),
                                exif_stripped: res.exif_stripped,
                                elapsed_ms: res.elapsed.as_millis() as u64,
                                can_revert,
                                error: None,
                                backup: if can_revert { backup } else { None },
                                summary,
                            },
                        )));
                    }
                    Err(err) => {
                        let _ = proxy.send_event(AppEvent::FileProgress(Box::new(
                            FileProgressUpdate {
                                path: path_str,
                                status: "error".to_string(),
                                original_size: 0,
                                optimized_size: 0,
                                saved_bytes: 0,
                                saved_percent: 0.0,
                                exif_stripped: false,
                                elapsed_ms: 0,
                                can_revert: false,
                                error: Some(err.to_string()),
                                backup: None,
                                summary: None,
                            },
                        )));
                    }
                }
            }
        });
    }
}

/// Opens a native OS multi-file picker filtered to all supported media extensions.
pub fn pick_supported_files() -> Vec<PathBuf> {
    let mut all_exts: Vec<&str> = Vec::new();
    all_exts.extend_from_slice(SUPPORTED_IMAGE_EXTENSIONS);
    all_exts.extend_from_slice(SUPPORTED_DOCUMENT_EXTENSIONS);
    all_exts.extend_from_slice(SUPPORTED_VIDEO_EXTENSIONS);

    rfd::FileDialog::new()
        .set_title("Select Files to Optimize")
        .add_filter("Supported Media", &all_exts)
        .add_filter("Images", SUPPORTED_IMAGE_EXTENSIONS)
        .add_filter("PDF Documents", SUPPORTED_DOCUMENT_EXTENSIONS)
        .add_filter("Videos", SUPPORTED_VIDEO_EXTENSIONS)
        .pick_files()
        .unwrap_or_default()
}

/// Opens a native OS folder picker.
pub fn pick_folder(title: &str) -> Option<PathBuf> {
    rfd::FileDialog::new().set_title(title).pick_folder()
}

/// Expands dropped/picked paths: files are kept directly; directories are scanned recursively
/// for supported media files (capped at 500 files for safety).
pub fn expand_paths(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut result = Vec::new();
    for p in paths {
        if p.is_dir() {
            collect_supported_in_dir(p, &mut result, 500);
        } else if p.is_file() {
            result.push(p.clone());
        }
    }
    result
}

fn collect_supported_in_dir(dir: &Path, out: &mut Vec<PathBuf>, limit: usize) {
    if out.len() >= limit {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= limit {
            break;
        }
        let path = entry.path();
        if path.is_dir() {
            // Skip hidden directories
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'))
            {
                continue;
            }
            collect_supported_in_dir(&path, out, limit);
        } else if path.is_file() && is_supported_media_path(&path) {
            out.push(path);
        }
    }
}

/// Inspects a file path to classify its extension, size, and support category for the UI.
pub fn inspect_file_info(path: &Path) -> DroppedFileInfo {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("Unknown")
        .to_string();
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);

    let (supported, category) = if SUPPORTED_IMAGE_EXTENSIONS.contains(&ext.as_str()) {
        (true, "image")
    } else if SUPPORTED_DOCUMENT_EXTENSIONS.contains(&ext.as_str()) {
        (true, "document")
    } else if SUPPORTED_VIDEO_EXTENSIONS.contains(&ext.as_str()) {
        (true, "video")
    } else {
        (false, "unsupported")
    };

    DroppedFileInfo {
        path: path.display().to_string(),
        name,
        ext,
        size,
        supported,
        category: category.to_string(),
    }
}
