use crate::clipboard::{ClipboardMonitor, OptimizationSummary};
use crate::config::{AppConfig, OptimizationPreset};
use crate::downloads::{is_supported_media_path, DownloadsWatcher};
use crate::history::{HistorySource, OptimizationHistory};
use crate::optimizer::{self, format_bytes, supported_formats_info};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, List, ListItem, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const LEADER_TIMEOUT_MS: u64 = 2000;

/// Visual color theme inspired by OpenCode's TUI themes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiTheme {
    OpenCodeDark,
    TokyoNight,
    CatppuccinMocha,
    GruvboxDark,
}

impl TuiTheme {
    pub fn name(self) -> &'static str {
        match self {
            TuiTheme::OpenCodeDark => "opencode-dark",
            TuiTheme::TokyoNight => "tokyo-night",
            TuiTheme::CatppuccinMocha => "catppuccin",
            TuiTheme::GruvboxDark => "gruvbox",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "opencode" | "opencode-dark" | "dark" => Some(TuiTheme::OpenCodeDark),
            "tokyo" | "tokyo-night" | "tokyonight" => Some(TuiTheme::TokyoNight),
            "catppuccin" | "mocha" => Some(TuiTheme::CatppuccinMocha),
            "gruvbox" | "gruvbox-dark" => Some(TuiTheme::GruvboxDark),
            _ => None,
        }
    }

    pub fn next(self) -> Self {
        match self {
            TuiTheme::OpenCodeDark => TuiTheme::TokyoNight,
            TuiTheme::TokyoNight => TuiTheme::CatppuccinMocha,
            TuiTheme::CatppuccinMocha => TuiTheme::GruvboxDark,
            TuiTheme::GruvboxDark => TuiTheme::OpenCodeDark,
        }
    }

    fn palette(self) -> ThemePalette {
        match self {
            TuiTheme::OpenCodeDark => ThemePalette {
                bg: Color::Rgb(15, 18, 25),
                surface: Color::Rgb(22, 27, 38),
                border: Color::Rgb(46, 56, 74),
                border_focus: Color::Rgb(56, 189, 248),
                accent: Color::Rgb(56, 189, 248),
                success: Color::Rgb(52, 211, 153),
                warn: Color::Rgb(251, 191, 36),
                error: Color::Rgb(248, 113, 113),
                fg: Color::Rgb(226, 232, 240),
                muted: Color::Rgb(130, 146, 168),
            },
            TuiTheme::TokyoNight => ThemePalette {
                bg: Color::Rgb(26, 27, 38),
                surface: Color::Rgb(36, 40, 59),
                border: Color::Rgb(65, 72, 104),
                border_focus: Color::Rgb(122, 162, 247),
                accent: Color::Rgb(122, 162, 247),
                success: Color::Rgb(158, 206, 106),
                warn: Color::Rgb(224, 175, 104),
                error: Color::Rgb(247, 118, 142),
                fg: Color::Rgb(192, 202, 245),
                muted: Color::Rgb(120, 128, 168),
            },
            TuiTheme::CatppuccinMocha => ThemePalette {
                bg: Color::Rgb(30, 30, 46),
                surface: Color::Rgb(49, 50, 68),
                border: Color::Rgb(88, 91, 112),
                border_focus: Color::Rgb(137, 180, 250),
                accent: Color::Rgb(137, 180, 250),
                success: Color::Rgb(166, 227, 161),
                warn: Color::Rgb(249, 226, 175),
                error: Color::Rgb(243, 139, 168),
                fg: Color::Rgb(205, 214, 244),
                muted: Color::Rgb(147, 153, 178),
            },
            TuiTheme::GruvboxDark => ThemePalette {
                bg: Color::Rgb(40, 40, 40),
                surface: Color::Rgb(50, 48, 47),
                border: Color::Rgb(102, 92, 84),
                border_focus: Color::Rgb(250, 189, 47),
                accent: Color::Rgb(131, 165, 152),
                success: Color::Rgb(184, 187, 38),
                warn: Color::Rgb(250, 189, 47),
                error: Color::Rgb(251, 73, 52),
                fg: Color::Rgb(235, 219, 178),
                muted: Color::Rgb(168, 153, 132),
            },
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ThemePalette {
    bg: Color,
    surface: Color,
    border: Color,
    border_focus: Color,
    accent: Color,
    success: Color,
    warn: Color,
    error: Color,
    fg: Color,
    muted: Color,
}

/// Startup options for the interactive OpenCode-style TUI.
#[derive(Debug, Clone)]
pub struct TuiLaunchOptions {
    pub work_dir: PathBuf,
    pub watch_clipboard: bool,
    pub watch_downloads: bool,
    pub theme: TuiTheme,
}

impl Default for TuiLaunchOptions {
    fn default() -> Self {
        Self {
            work_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            watch_clipboard: false,
            watch_downloads: false,
            theme: TuiTheme::OpenCodeDark,
        }
    }
}

/// Slash command definition for autocomplete & `/help`.
#[derive(Debug, Clone, Copy)]
pub struct SlashCommandDef {
    pub name: &'static str,
    pub usage: &'static str,
    pub description: &'static str,
    pub shortcut: &'static str,
}

pub const SLASH_COMMANDS: &[SlashCommandDef] = &[
    SlashCommandDef {
        name: "/optimize",
        usage: "/optimize <path>",
        description: "Optimize a file or directory on disk",
        shortcut: "@file",
    },
    SlashCommandDef {
        name: "/clipboard",
        usage: "/clipboard",
        description: "Optimize the current clipboard image now",
        shortcut: "Ctrl+X c",
    },
    SlashCommandDef {
        name: "/watch",
        usage: "/watch",
        description: "Toggle live clipboard auto-optimization",
        shortcut: "Ctrl+X w",
    },
    SlashCommandDef {
        name: "/downloads",
        usage: "/downloads",
        description: "Toggle Downloads folder auto-optimization",
        shortcut: "",
    },
    SlashCommandDef {
        name: "/preset",
        usage: "/preset [lossless|high-quality|balanced|smallest-size]",
        description: "Switch or cycle quality-vs-size preset",
        shortcut: "Ctrl+X p",
    },
    SlashCommandDef {
        name: "/quality",
        usage: "/quality <10-100>",
        description: "Set custom quality target (10 = smallest, 100 = highest)",
        shortcut: "",
    },
    SlashCommandDef {
        name: "/exif",
        usage: "/exif",
        description: "Toggle EXIF & metadata stripping",
        shortcut: "",
    },
    SlashCommandDef {
        name: "/undo",
        usage: "/undo",
        description: "Revert last clipboard or file optimization",
        shortcut: "Ctrl+X u",
    },
    SlashCommandDef {
        name: "/history",
        usage: "/history [n]",
        description: "Load recent persistent optimization history into feed",
        shortcut: "",
    },
    SlashCommandDef {
        name: "/details",
        usage: "/details",
        description: "Toggle right sidebar details & diagnostics",
        shortcut: "Ctrl+X d",
    },
    SlashCommandDef {
        name: "/theme",
        usage: "/theme [dark|tokyo|catppuccin|gruvbox]",
        description: "Switch or cycle TUI color theme",
        shortcut: "Ctrl+X t",
    },
    SlashCommandDef {
        name: "/clear",
        usage: "/clear",
        description: "Clear the activity feed",
        shortcut: "",
    },
    SlashCommandDef {
        name: "/help",
        usage: "/help",
        description: "Toggle commands & keybindings overlay",
        shortcut: "Ctrl+X h",
    },
    SlashCommandDef {
        name: "/exit",
        usage: "/exit",
        description: "Exit the clop-rs TUI",
        shortcut: "Ctrl+X q",
    },
];

#[derive(Debug, Clone)]
enum FeedItem {
    Info(String),
    Warning(String),
    Error(String),
    Shell {
        command: String,
        output: String,
        success: bool,
    },
    Optimized {
        source: String,
        label: String,
        original_size: usize,
        optimized_size: usize,
        saved_bytes: usize,
        saved_percent: f64,
        exif_stripped: bool,
        elapsed_ms: u64,
    },
}

#[derive(Debug, Clone)]
enum LastUndoTarget {
    Clipboard,
    File { path: PathBuf, original: Vec<u8> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PopupMode {
    None,
    SlashCommands,
    FilePicker,
}

/// Interactive OpenCode-inspired TUI state.
pub struct TuiApp {
    pub config: AppConfig,
    pub work_dir: PathBuf,
    pub theme: TuiTheme,
    pub show_sidebar: bool,
    pub show_help: bool,
    pub watch_clipboard: bool,
    pub watch_downloads: bool,
    pub input: String,
    pub should_quit: bool,

    feed: Vec<FeedItem>,
    scroll_from_bottom: usize,
    history: OptimizationHistory,
    session_items: usize,
    session_saved_bytes: u64,
    session_orig_bytes: u64,

    monitor: ClipboardMonitor,
    downloads: DownloadsWatcher,
    last_undo: Option<LastUndoTarget>,
    last_poll: Instant,

    leader_armed_at: Option<Instant>,
    popup_selected: usize,
    workspace_files: Vec<String>,
}

impl TuiApp {
    pub fn new(mut config: AppConfig, opts: TuiLaunchOptions) -> Self {
        if opts.watch_downloads {
            config.watch_downloads = true;
        }
        let history = OptimizationHistory::load();
        let downloads = DownloadsWatcher::new(config.effective_downloads_dir());
        let workspace_files = scan_workspace_media_files(&opts.work_dir, 200);

        let mut app = Self {
            config,
            work_dir: opts.work_dir,
            theme: opts.theme,
            show_sidebar: true,
            show_help: false,
            watch_clipboard: opts.watch_clipboard,
            watch_downloads: opts.watch_downloads,
            input: String::new(),
            should_quit: false,
            feed: Vec::new(),
            scroll_from_bottom: 0,
            history,
            session_items: 0,
            session_saved_bytes: 0,
            session_orig_bytes: 0,
            monitor: ClipboardMonitor::new(),
            downloads,
            last_undo: None,
            last_poll: Instant::now(),
            leader_armed_at: None,
            popup_selected: 0,
            workspace_files,
        };

        app.push_info(
            "Welcome to clop-rs TUI ⚡  Type `/` for commands, `@` to pick a file, `!cmd` for shell, or `Ctrl+X` leader shortcuts.",
        );
        app
    }

    fn push_info(&mut self, msg: impl Into<String>) {
        self.feed.push(FeedItem::Info(msg.into()));
        self.scroll_from_bottom = 0;
    }

    fn push_warn(&mut self, msg: impl Into<String>) {
        self.feed.push(FeedItem::Warning(msg.into()));
        self.scroll_from_bottom = 0;
    }

    fn push_err(&mut self, msg: impl Into<String>) {
        self.feed.push(FeedItem::Error(msg.into()));
        self.scroll_from_bottom = 0;
    }

    fn record_optimization(
        &mut self,
        source: HistorySource,
        summary: &OptimizationSummary,
        file_path: Option<&Path>,
    ) {
        self.session_items += 1;
        self.session_orig_bytes += summary.original_size as u64;
        self.session_saved_bytes += summary.saved_bytes as u64;

        self.history
            .record_summary(source, summary, file_path, &self.config);
        let _ = self.history.save();

        self.feed.push(FeedItem::Optimized {
            source: source.as_str().to_ascii_uppercase(),
            label: summary.source_label.clone(),
            original_size: summary.original_size,
            optimized_size: summary.optimized_size,
            saved_bytes: summary.saved_bytes,
            saved_percent: summary.saved_percent,
            exif_stripped: summary.exif_stripped,
            elapsed_ms: summary.elapsed.as_millis() as u64,
        });
        self.scroll_from_bottom = 0;
    }

    fn popup_mode(&self) -> PopupMode {
        if self.input.starts_with('/') && !self.input.contains(' ') {
            PopupMode::SlashCommands
        } else if extract_at_query(&self.input).is_some() {
            PopupMode::FilePicker
        } else {
            PopupMode::None
        }
    }

    fn filtered_slash_commands(&self) -> Vec<&'static SlashCommandDef> {
        filter_slash_commands(&self.input)
    }

    fn filtered_files(&self) -> Vec<&String> {
        let Some(query) = extract_at_query(&self.input) else {
            return Vec::new();
        };
        filter_workspace_files(&self.workspace_files, query)
    }

    fn is_leader_armed(&self) -> bool {
        self.leader_armed_at
            .is_some_and(|t| t.elapsed() <= Duration::from_millis(LEADER_TIMEOUT_MS))
    }

    /// Periodic background tick for leader timeout and live clipboard/Downloads watchers.
    pub fn tick(&mut self) {
        if let Some(armed) = self.leader_armed_at {
            if armed.elapsed() > Duration::from_millis(LEADER_TIMEOUT_MS) {
                self.leader_armed_at = None;
            }
        }

        let interval = Duration::from_millis(self.config.poll_interval_ms.max(150));
        if self.last_poll.elapsed() < interval {
            return;
        }
        self.last_poll = Instant::now();

        if self.watch_clipboard {
            if let Some(pending) = self.monitor.inspect_new_image(false) {
                if let Ok(Some(summary)) = self.monitor.optimize_pending(pending, &self.config) {
                    self.last_undo = Some(LastUndoTarget::Clipboard);
                    self.record_optimization(HistorySource::Clipboard, &summary, None);
                }
            }
        }

        if self.watch_downloads && self.config.watch_downloads {
            if let Some(pending) = self.downloads.inspect_next_file(&self.config) {
                let path = pending.path.clone();
                if let Ok(Some(summary)) =
                    self.downloads.optimize_pending_file(pending, &self.config)
                {
                    if let Some(ref b) = self.downloads.last_backup {
                        self.last_undo = Some(LastUndoTarget::File {
                            path: b.path.clone(),
                            original: b.original_bytes.clone(),
                        });
                    }
                    self.record_optimization(HistorySource::Downloads, &summary, Some(&path));
                }
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }

        // Global Ctrl+C exit
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }

        // Help overlay dismissal
        if self.show_help {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Enter
            ) {
                self.show_help = false;
            }
            return;
        }

        // OpenCode-style Ctrl+X leader sequence
        if self.is_leader_armed() {
            self.leader_armed_at = None;
            match key.code {
                KeyCode::Char('c') => self.execute_command("/clipboard"),
                KeyCode::Char('w') => self.execute_command("/watch"),
                KeyCode::Char('p') => self.execute_command("/preset"),
                KeyCode::Char('u') => self.execute_command("/undo"),
                KeyCode::Char('t') => self.execute_command("/theme"),
                KeyCode::Char('d') => self.execute_command("/details"),
                KeyCode::Char('h') => self.execute_command("/help"),
                KeyCode::Char('q') => self.should_quit = true,
                KeyCode::Esc => {}
                _ => self.push_warn("Unknown leader shortcut. Press Ctrl+X h for help."),
            }
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('x') {
            self.leader_armed_at = Some(Instant::now());
            return;
        }

        let popup = self.popup_mode();

        match key.code {
            KeyCode::Esc => {
                if !self.input.is_empty() {
                    self.input.clear();
                    self.popup_selected = 0;
                }
            }
            KeyCode::Up => {
                if popup != PopupMode::None {
                    self.popup_selected = self.popup_selected.saturating_sub(1);
                } else {
                    self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(1);
                }
            }
            KeyCode::Down => {
                if popup == PopupMode::SlashCommands {
                    let count = self.filtered_slash_commands().len();
                    if count > 0 {
                        self.popup_selected = (self.popup_selected + 1).min(count - 1);
                    }
                } else if popup == PopupMode::FilePicker {
                    let count = self.filtered_files().len();
                    if count > 0 {
                        self.popup_selected = (self.popup_selected + 1).min(count - 1);
                    }
                } else {
                    self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(1);
                }
            }
            KeyCode::PageUp => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(6);
            }
            KeyCode::PageDown => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(6);
            }
            KeyCode::Tab => {
                self.apply_popup_completion();
            }
            KeyCode::Enter => {
                if popup == PopupMode::SlashCommands {
                    let matches = self.filtered_slash_commands();
                    if let Some(cmd) = matches.get(self.popup_selected) {
                        if matches!(cmd.name, "/optimize" | "/quality") {
                            self.input = format!("{} ", cmd.name);
                            self.popup_selected = 0;
                            return;
                        }
                        let name = cmd.name.to_string();
                        self.input.clear();
                        self.popup_selected = 0;
                        self.execute_command(&name);
                        return;
                    }
                } else if popup == PopupMode::FilePicker && self.apply_popup_completion() {
                    let line = self.input.trim().to_string();
                    if line.starts_with('@') && !line.contains(' ') {
                        self.input.clear();
                        self.popup_selected = 0;
                        self.execute_command(&line);
                    }
                    return;
                }

                let submitted = self.input.trim().to_string();
                self.input.clear();
                self.popup_selected = 0;
                if !submitted.is_empty() {
                    self.execute_command(&submitted);
                }
            }
            KeyCode::Backspace => {
                self.input.pop();
                self.popup_selected = 0;
            }
            KeyCode::Char('?') if self.input.is_empty() => {
                self.show_help = true;
            }
            KeyCode::Char(c) => {
                self.input.push(c);
                self.popup_selected = 0;
            }
            _ => {}
        }
    }

    fn apply_popup_completion(&mut self) -> bool {
        match self.popup_mode() {
            PopupMode::SlashCommands => {
                let matches = self.filtered_slash_commands();
                if let Some(cmd) = matches.get(self.popup_selected) {
                    let suffix = if matches!(cmd.name, "/optimize" | "/quality" | "/preset") {
                        " "
                    } else {
                        ""
                    };
                    self.input = format!("{}{}", cmd.name, suffix);
                    self.popup_selected = 0;
                    return true;
                }
            }
            PopupMode::FilePicker => {
                let chosen = self
                    .filtered_files()
                    .get(self.popup_selected)
                    .map(|s| (*s).clone());
                if let Some(file) = chosen {
                    if let Some(at_pos) = self.input.rfind('@') {
                        self.input.truncate(at_pos);
                        self.input.push('@');
                        self.input.push_str(&file);
                        self.popup_selected = 0;
                        return true;
                    }
                }
            }
            PopupMode::None => {}
        }
        false
    }

    /// Executes a slash command (`/cmd`), `@file` reference, `!shell` command, or direct path.
    pub fn execute_command(&mut self, raw: &str) {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return;
        }

        // 1. OpenCode-style `!shell` command execution
        if let Some(cmd_str) = trimmed.strip_prefix('!') {
            let cmd_str = cmd_str.trim();
            if cmd_str.is_empty() {
                self.push_warn("Usage: !<shell command> (e.g. !ls -lh)");
                return;
            }
            self.run_shell_command(cmd_str);
            return;
        }

        // 2. OpenCode-style `@file` reference(s) -> optimize referenced file(s)
        if trimmed.starts_with('@') {
            let targets: Vec<PathBuf> = trimmed
                .split_whitespace()
                .filter_map(|tok| tok.strip_prefix('@'))
                .filter(|s| !s.is_empty())
                .map(|rel| self.resolve_path(rel))
                .collect();
            if targets.is_empty() {
                self.push_warn("Type @ followed by a filename to pick and optimize a file.");
                return;
            }
            for path in targets {
                self.optimize_target_path(&path);
            }
            return;
        }

        // 3. Slash commands
        if trimmed.starts_with('/') {
            let mut parts = trimmed.splitn(2, char::is_whitespace);
            let cmd = parts.next().unwrap_or("").to_ascii_lowercase();
            let arg = parts.next().map(str::trim).unwrap_or("");

            match cmd.as_str() {
                "/exit" | "/quit" | "/q" => {
                    self.should_quit = true;
                }
                "/help" => {
                    self.show_help = !self.show_help;
                }
                "/clear" => {
                    self.feed.clear();
                    self.scroll_from_bottom = 0;
                }
                "/details" => {
                    self.show_sidebar = !self.show_sidebar;
                }
                "/clipboard" | "/cb" => {
                    self.optimize_clipboard_now();
                }
                "/watch" => {
                    self.watch_clipboard = !self.watch_clipboard;
                    self.push_info(format!(
                        "Live clipboard watcher: {}",
                        if self.watch_clipboard { "ON" } else { "OFF" }
                    ));
                }
                "/downloads" => {
                    self.watch_downloads = !self.watch_downloads;
                    self.config.watch_downloads = self.watch_downloads;
                    let _ = self.config.save();
                    self.downloads
                        .set_directory(self.config.effective_downloads_dir());
                    self.push_info(format!(
                        "Downloads folder watcher: {}",
                        if self.watch_downloads { "ON" } else { "OFF" }
                    ));
                }
                "/exif" => {
                    self.config.strip_metadata = !self.config.strip_metadata;
                    let _ = self.config.save();
                    self.push_info(format!(
                        "Strip EXIF & metadata: {}",
                        if self.config.strip_metadata {
                            "ON"
                        } else {
                            "OFF"
                        }
                    ));
                }
                "/preset" => {
                    let next_preset = if arg.is_empty() {
                        match self.config.preset {
                            OptimizationPreset::Balanced => OptimizationPreset::SmallestSize,
                            OptimizationPreset::SmallestSize => OptimizationPreset::HighQuality,
                            OptimizationPreset::HighQuality => OptimizationPreset::Lossless,
                            OptimizationPreset::Lossless | OptimizationPreset::Custom => {
                                OptimizationPreset::Balanced
                            }
                        }
                    } else {
                        match arg.to_ascii_lowercase().as_str() {
                            "lossless" => OptimizationPreset::Lossless,
                            "high-quality" | "high" | "hq" => OptimizationPreset::HighQuality,
                            "balanced" | "default" => OptimizationPreset::Balanced,
                            "smallest-size" | "smallest" | "small" => {
                                OptimizationPreset::SmallestSize
                            }
                            _ => {
                                self.push_warn(
                                    "Unknown preset. Choose: lossless | high-quality | balanced | smallest-size",
                                );
                                return;
                            }
                        }
                    };
                    self.config.apply_preset(next_preset);
                    let _ = self.config.save();
                    self.push_info(format!(
                        "Preset switched to {}",
                        self.config.preset_summary_label()
                    ));
                }
                "/quality" => {
                    if let Ok(q) = arg.parse::<u8>() {
                        self.config.apply_quality_level(q);
                        let _ = self.config.save();
                        self.push_info(format!(
                            "Quality set to Q{} ({})",
                            self.config.quality_level,
                            self.config.preset_summary_label()
                        ));
                    } else {
                        self.push_warn("Usage: /quality <10-100> (e.g. /quality 80)");
                    }
                }
                "/theme" => {
                    if arg.is_empty() {
                        self.theme = self.theme.next();
                    } else if let Some(t) = TuiTheme::from_name(arg) {
                        self.theme = t;
                    } else {
                        self.push_warn(
                            "Unknown theme. Choose: dark | tokyo | catppuccin | gruvbox",
                        );
                        return;
                    }
                    self.push_info(format!("Theme switched to `{}`", self.theme.name()));
                }
                "/undo" | "/revert" => {
                    self.undo_last();
                }
                "/history" => {
                    let limit = arg.parse::<usize>().unwrap_or(10).clamp(1, 50);
                    let entries: Vec<_> =
                        self.history.entries.iter().take(limit).cloned().collect();
                    if entries.is_empty() {
                        self.push_info("No optimization history recorded yet.");
                    } else {
                        self.push_info(format!("Loaded {} recent history entries:", entries.len()));
                        for e in entries.into_iter().rev() {
                            self.feed.push(FeedItem::Optimized {
                                source: e.source.as_str().to_ascii_uppercase(),
                                label: e.label,
                                original_size: e.original_size,
                                optimized_size: e.optimized_size,
                                saved_bytes: e.saved_bytes,
                                saved_percent: e.saved_percent,
                                exif_stripped: e.exif_stripped,
                                elapsed_ms: e.elapsed_ms,
                            });
                        }
                    }
                }
                "/optimize" | "/opt" => {
                    if arg.is_empty() {
                        self.push_warn(
                            "Usage: /optimize <file-or-dir>  (or type @ to fuzzy-pick a file)",
                        );
                        return;
                    }
                    let cleaned = arg.trim_start_matches('@');
                    let path = self.resolve_path(cleaned);
                    self.optimize_target_path(&path);
                }
                _ => {
                    self.push_warn(format!(
                        "Unknown command `{}`. Type /help for commands.",
                        cmd
                    ));
                }
            }
            return;
        }

        // 4. Plain path fallback: if the user typed an existing file/dir path directly, optimize it
        let direct = self.resolve_path(trimmed);
        if direct.exists() {
            self.optimize_target_path(&direct);
        } else {
            self.push_warn(format!(
                "Unrecognized input `{}`. Use `/help`, `@file`, or `!shell`.",
                trimmed
            ));
        }
    }

    fn resolve_path(&self, raw: &str) -> PathBuf {
        let p = PathBuf::from(raw);
        if p.is_absolute() {
            p
        } else {
            self.work_dir.join(p)
        }
    }

    fn optimize_clipboard_now(&mut self) {
        let Some(pending) = self.monitor.inspect_new_image(true) else {
            self.push_info("Clipboard does not currently contain an image.");
            return;
        };
        match self.monitor.optimize_pending(pending, &self.config) {
            Ok(Some(summary)) => {
                self.last_undo = Some(LastUndoTarget::Clipboard);
                self.record_optimization(HistorySource::Clipboard, &summary, None);
            }
            Ok(None) => {
                self.push_info("Clipboard image is already optimal.");
            }
            Err(e) => {
                self.push_err(format!("Clipboard optimization failed: {}", e));
            }
        }
    }

    fn optimize_target_path(&mut self, path: &Path) {
        if !path.exists() {
            self.push_err(format!("Path not found: {}", path.display()));
            return;
        }

        let files: Vec<PathBuf> = if path.is_file() {
            vec![path.to_path_buf()]
        } else {
            let mut found = Vec::new();
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_file() && is_supported_media_path(&p) {
                        found.push(p);
                    }
                }
            }
            found.sort();
            found
        };

        if files.is_empty() {
            self.push_warn(format!("No supported media files in {}", path.display()));
            return;
        }

        for file in files {
            let backup_bytes = fs::read(&file).ok();
            match optimizer::optimize_file(&file, None, &self.config) {
                Ok(res) => {
                    let name = file
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("File")
                        .to_string();
                    if res.was_modified {
                        if let Some(orig) = backup_bytes {
                            self.last_undo = Some(LastUndoTarget::File {
                                path: file.clone(),
                                original: orig,
                            });
                        }
                        let summary = OptimizationSummary {
                            source_label: name,
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
                        self.record_optimization(HistorySource::Cli, &summary, Some(&file));
                    } else {
                        self.push_info(format!(
                            "• {} is already optimal ({})",
                            name,
                            format_bytes(res.original_size)
                        ));
                    }
                }
                Err(e) => {
                    self.push_err(format!("Failed to optimize {}: {}", file.display(), e));
                }
            }
        }
        self.workspace_files = scan_workspace_media_files(&self.work_dir, 200);
    }

    fn undo_last(&mut self) {
        let Some(target) = self.last_undo.take() else {
            self.push_warn("Nothing to undo in this session.");
            return;
        };
        match target {
            LastUndoTarget::Clipboard => match self.monitor.revert_last() {
                Ok(Some(bytes)) => {
                    self.push_info(format!(
                        "Reverted clipboard image ({})",
                        format_bytes(bytes)
                    ));
                }
                Ok(None) => self.push_warn("No clipboard backup available to revert."),
                Err(e) => self.push_err(format!("Clipboard revert failed: {}", e)),
            },
            LastUndoTarget::File { path, original } => match fs::write(&path, &original) {
                Ok(()) => {
                    self.push_info(format!(
                        "Reverted {} ({})",
                        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
                        format_bytes(original.len())
                    ));
                }
                Err(e) => self.push_err(format!("File revert failed: {}", e)),
            },
        }
    }

    fn run_shell_command(&mut self, cmd_str: &str) {
        let output = if cfg!(target_os = "windows") {
            Command::new("cmd")
                .args(["/C", cmd_str])
                .current_dir(&self.work_dir)
                .output()
        } else {
            Command::new("sh")
                .args(["-c", cmd_str])
                .current_dir(&self.work_dir)
                .output()
        };

        match output {
            Ok(out) => {
                let mut combined = String::from_utf8_lossy(&out.stdout).to_string();
                let stderr = String::from_utf8_lossy(&out.stderr);
                if !stderr.trim().is_empty() {
                    if !combined.is_empty() && !combined.ends_with('\n') {
                        combined.push('\n');
                    }
                    combined.push_str(&stderr);
                }
                let lines: Vec<&str> = combined.lines().take(12).collect();
                let truncated = if combined.lines().count() > 12 {
                    format!("{}\n… (truncated)", lines.join("\n"))
                } else if lines.is_empty() {
                    "(no output)".to_string()
                } else {
                    lines.join("\n")
                };
                self.feed.push(FeedItem::Shell {
                    command: cmd_str.to_string(),
                    output: truncated,
                    success: out.status.success(),
                });
                self.scroll_from_bottom = 0;
            }
            Err(e) => {
                self.push_err(format!("Failed to execute `{}`: {}", cmd_str, e));
            }
        }
    }

    fn draw(&self, frame: &mut Frame) {
        let p = self.theme.palette();
        let area = frame.area();

        // Fill background
        frame.render_widget(Block::default().style(Style::default().bg(p.bg)), area);

        let root = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Top header bar
                Constraint::Min(8),    // Main content (feed + sidebar)
                Constraint::Length(3), // Bottom composer bar
                Constraint::Length(1), // Status/hints footer
            ])
            .split(area);

        self.draw_header(frame, root[0], p);

        if self.show_sidebar && area.width >= 84 {
            let body = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(68), Constraint::Percentage(32)])
                .split(root[1]);
            self.draw_feed(frame, body[0], p);
            self.draw_sidebar(frame, body[1], p);
        } else {
            self.draw_feed(frame, root[1], p);
        }

        self.draw_composer(frame, root[2], p);
        self.draw_footer(frame, root[3], p);

        // Autocomplete popup above composer
        let popup_mode = self.popup_mode();
        if popup_mode != PopupMode::None {
            self.draw_autocomplete_popup(frame, root[1], popup_mode, p);
        }

        // Centered help modal
        if self.show_help {
            self.draw_help_modal(frame, area, p);
        }
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect, p: ThemePalette) {
        let cb_span = if self.watch_clipboard {
            Span::styled(
                " CB:ON ",
                Style::default()
                    .fg(p.bg)
                    .bg(p.success)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(" CB:OFF ", Style::default().fg(p.muted).bg(p.surface))
        };

        let dl_span = if self.watch_downloads {
            Span::styled(
                " DL:ON ",
                Style::default()
                    .fg(p.bg)
                    .bg(p.accent)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(" DL:OFF ", Style::default().fg(p.muted).bg(p.surface))
        };

        let mut spans = vec![
            Span::styled(
                " clop-rs ⚡ ",
                Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled("│ ", Style::default().fg(p.border)),
            Span::styled(
                self.config.preset_summary_label(),
                Style::default().fg(p.fg).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            cb_span,
            Span::raw(" "),
            dl_span,
            Span::raw("  "),
            Span::styled(
                self.work_dir.display().to_string(),
                Style::default().fg(p.muted),
            ),
        ];

        if self.is_leader_armed() {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                " ^X LEADER ",
                Style::default()
                    .fg(p.bg)
                    .bg(p.warn)
                    .add_modifier(Modifier::BOLD),
            ));
        }

        let header = Paragraph::new(Line::from(spans)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(p.border))
                .style(Style::default().bg(p.surface)),
        );
        frame.render_widget(header, area);
    }

    fn draw_feed(&self, frame: &mut Frame, area: Rect, p: ThemePalette) {
        let mut lines: Vec<Line> = Vec::new();
        for item in &self.feed {
            match item {
                FeedItem::Info(msg) => {
                    lines.push(Line::from(vec![
                        Span::styled("● ", Style::default().fg(p.accent)),
                        Span::styled(msg.clone(), Style::default().fg(p.fg)),
                    ]));
                    lines.push(Line::raw(""));
                }
                FeedItem::Warning(msg) => {
                    lines.push(Line::from(vec![
                        Span::styled("▲ ", Style::default().fg(p.warn)),
                        Span::styled(msg.clone(), Style::default().fg(p.warn)),
                    ]));
                    lines.push(Line::raw(""));
                }
                FeedItem::Error(msg) => {
                    lines.push(Line::from(vec![
                        Span::styled("✖ ", Style::default().fg(p.error)),
                        Span::styled(msg.clone(), Style::default().fg(p.error)),
                    ]));
                    lines.push(Line::raw(""));
                }
                FeedItem::Shell {
                    command,
                    output,
                    success,
                } => {
                    let badge_col = if *success { p.success } else { p.error };
                    lines.push(Line::from(vec![
                        Span::styled(
                            " ! ",
                            Style::default()
                                .fg(p.bg)
                                .bg(badge_col)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(" "),
                        Span::styled(
                            command.clone(),
                            Style::default().fg(p.fg).add_modifier(Modifier::BOLD),
                        ),
                    ]));
                    for out_line in output.lines() {
                        lines.push(Line::from(vec![
                            Span::styled("  │ ", Style::default().fg(p.border)),
                            Span::styled(out_line.to_string(), Style::default().fg(p.muted)),
                        ]));
                    }
                    lines.push(Line::raw(""));
                }
                FeedItem::Optimized {
                    source,
                    label,
                    original_size,
                    optimized_size,
                    saved_bytes,
                    saved_percent,
                    exif_stripped,
                    elapsed_ms,
                } => {
                    let ratio = if *original_size > 0 {
                        (*optimized_size as f64 / *original_size as f64).clamp(0.05, 1.0)
                    } else {
                        1.0
                    };
                    let bar_len = 16usize;
                    let filled = ((bar_len as f64) * ratio).round() as usize;
                    let bar = format!(
                        "{}{}",
                        "█".repeat(filled.min(bar_len)),
                        "░".repeat(bar_len.saturating_sub(filled))
                    );

                    let mut top = vec![
                        Span::styled(
                            format!(" {} ", source),
                            Style::default()
                                .fg(p.bg)
                                .bg(p.accent)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(" "),
                        Span::styled(
                            label.clone(),
                            Style::default().fg(p.fg).add_modifier(Modifier::BOLD),
                        ),
                        Span::raw("  "),
                        Span::styled(
                            format!("-{:.1}%", saved_percent),
                            Style::default().fg(p.success).add_modifier(Modifier::BOLD),
                        ),
                    ];
                    if *exif_stripped {
                        top.push(Span::raw(" "));
                        top.push(Span::styled(
                            "[EXIF]",
                            Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
                        ));
                    }
                    lines.push(Line::from(top));

                    lines.push(Line::from(vec![
                        Span::styled("  │ ", Style::default().fg(p.border)),
                        Span::styled(
                            format!(
                                "{} -> {}",
                                format_bytes(*original_size),
                                format_bytes(*optimized_size)
                            ),
                            Style::default().fg(p.fg),
                        ),
                        Span::raw("  "),
                        Span::styled(bar, Style::default().fg(p.success)),
                        Span::raw("  "),
                        Span::styled(
                            format!("saved {} in {}ms", format_bytes(*saved_bytes), elapsed_ms),
                            Style::default().fg(p.muted),
                        ),
                    ]));
                    lines.push(Line::raw(""));
                }
            }
        }

        let inner_h = area.height.saturating_sub(2) as usize;
        let max_scroll = lines.len().saturating_sub(inner_h);
        let effective_scroll = max_scroll.saturating_sub(self.scroll_from_bottom.min(max_scroll));

        let feed_widget = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((effective_scroll as u16, 0))
            .block(
                Block::default()
                    .title(Span::styled(
                        " Activity Feed ",
                        Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
                    ))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(p.border))
                    .style(Style::default().bg(p.bg)),
            );
        frame.render_widget(feed_widget, area);
    }

    fn draw_sidebar(&self, frame: &mut Frame, area: Rect, p: ThemePalette) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(7), // Savings & gauge
                Constraint::Length(9), // Engine & settings
                Constraint::Min(6),    // Leader shortcuts
            ])
            .split(area);

        // 1. Savings card
        let stats = self.history.stats();
        let pct = if self.session_orig_bytes > 0 {
            (self.session_saved_bytes as f64 / self.session_orig_bytes as f64) * 100.0
        } else {
            stats.overall_saved_percent
        };
        let savings_block = Block::default()
            .title(Span::styled(
                " Savings ",
                Style::default().fg(p.success).add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.border))
            .style(Style::default().bg(p.surface));
        let inner = savings_block.inner(chunks[0]);
        frame.render_widget(savings_block, chunks[0]);

        let s_layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Length(2)])
            .split(inner);

        let stats_text = Paragraph::new(vec![
            Line::from(vec![
                Span::styled("Session: ", Style::default().fg(p.muted)),
                Span::styled(
                    format!(
                        "{} items · {}",
                        self.session_items,
                        format_bytes(self.session_saved_bytes as usize)
                    ),
                    Style::default().fg(p.fg).add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("All-time: ", Style::default().fg(p.muted)),
                Span::styled(
                    format!(
                        "{} items · {}",
                        stats.total_items,
                        format_bytes(stats.total_saved_bytes as usize)
                    ),
                    Style::default().fg(p.success),
                ),
            ]),
        ]);
        frame.render_widget(stats_text, s_layout[0]);

        let gauge = Gauge::default()
            .gauge_style(Style::default().fg(p.success).bg(p.bg))
            .ratio((pct / 100.0).clamp(0.0, 1.0))
            .label(format!("{:.1}% avg saved", pct));
        frame.render_widget(gauge, s_layout[1]);

        // 2. Engine & Config card
        let formats = supported_formats_info();
        let ok = |b: bool| {
            if b {
                Span::styled("✓", Style::default().fg(p.success))
            } else {
                Span::styled("✗", Style::default().fg(p.muted))
            }
        };
        let config_lines = vec![
            Line::from(vec![
                Span::styled("Preset:  ", Style::default().fg(p.muted)),
                Span::styled(
                    self.config.preset_summary_label(),
                    Style::default().fg(p.fg),
                ),
            ]),
            Line::from(vec![
                Span::styled("EXIF:    ", Style::default().fg(p.muted)),
                Span::styled(
                    if self.config.strip_metadata {
                        "Strip"
                    } else {
                        "Keep"
                    },
                    Style::default().fg(p.fg),
                ),
            ]),
            Line::from(vec![
                Span::styled("Theme:   ", Style::default().fg(p.muted)),
                Span::styled(self.theme.name(), Style::default().fg(p.accent)),
            ]),
            Line::from(vec![
                Span::styled("Codecs:  ", Style::default().fg(p.muted)),
                ok(true),
                Span::styled(" MozJPEG ", Style::default().fg(p.fg)),
                ok(true),
                Span::styled(" Oxipng", Style::default().fg(p.fg)),
            ]),
            Line::from(vec![
                Span::styled("Tools:   ", Style::default().fg(p.muted)),
                ok(formats.has_ffmpeg),
                Span::styled(" FFmpeg  ", Style::default().fg(p.fg)),
                ok(formats.has_ghostscript),
                Span::styled(" GS", Style::default().fg(p.fg)),
            ]),
        ];
        let config_card = Paragraph::new(config_lines).block(
            Block::default()
                .title(Span::styled(
                    " Engine ",
                    Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(p.border))
                .style(Style::default().bg(p.surface)),
        );
        frame.render_widget(config_card, chunks[1]);

        // 3. Keybinds card
        let keys = vec![
            Line::from(vec![
                Span::styled("Ctrl+X c ", Style::default().fg(p.warn)),
                Span::styled("Optimize clipboard", Style::default().fg(p.fg)),
            ]),
            Line::from(vec![
                Span::styled("Ctrl+X w ", Style::default().fg(p.warn)),
                Span::styled("Toggle CB watch", Style::default().fg(p.fg)),
            ]),
            Line::from(vec![
                Span::styled("Ctrl+X p ", Style::default().fg(p.warn)),
                Span::styled("Cycle preset", Style::default().fg(p.fg)),
            ]),
            Line::from(vec![
                Span::styled("Ctrl+X u ", Style::default().fg(p.warn)),
                Span::styled("Undo last", Style::default().fg(p.fg)),
            ]),
            Line::from(vec![
                Span::styled("Ctrl+X t ", Style::default().fg(p.warn)),
                Span::styled("Cycle theme", Style::default().fg(p.fg)),
            ]),
            Line::from(vec![
                Span::styled("Ctrl+X d ", Style::default().fg(p.warn)),
                Span::styled("Toggle sidebar", Style::default().fg(p.fg)),
            ]),
            Line::from(vec![
                Span::styled("Ctrl+X q ", Style::default().fg(p.warn)),
                Span::styled("Quit", Style::default().fg(p.fg)),
            ]),
        ];
        let keys_card = Paragraph::new(keys).block(
            Block::default()
                .title(Span::styled(
                    " Shortcuts ",
                    Style::default().fg(p.muted).add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(p.border))
                .style(Style::default().bg(p.surface)),
        );
        frame.render_widget(keys_card, chunks[2]);
    }

    fn draw_composer(&self, frame: &mut Frame, area: Rect, p: ThemePalette) {
        let mode_badge = if self.input.starts_with('/') {
            (" CMD ", p.accent)
        } else if self.input.starts_with('!') {
            (" SH ", p.warn)
        } else if self.input.starts_with('@') {
            (" FILE ", p.success)
        } else {
            (" ❯ ", p.accent)
        };

        let line = if self.input.is_empty() {
            Line::from(vec![
                Span::styled(
                    mode_badge.0,
                    Style::default()
                        .fg(p.bg)
                        .bg(mode_badge.1)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(
                    "Type /command, @file to optimize, or !shell...",
                    Style::default().fg(p.muted),
                ),
            ])
        } else {
            Line::from(vec![
                Span::styled(
                    mode_badge.0,
                    Style::default()
                        .fg(p.bg)
                        .bg(mode_badge.1)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(self.input.clone(), Style::default().fg(p.fg)),
                Span::styled("█", Style::default().fg(p.accent)),
            ])
        };

        let composer = Paragraph::new(line).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(p.border_focus))
                .style(Style::default().bg(p.surface)),
        );
        frame.render_widget(composer, area);
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect, p: ThemePalette) {
        let footer = Paragraph::new(Line::from(vec![
            Span::styled(" / ", Style::default().fg(p.accent)),
            Span::styled("commands  ", Style::default().fg(p.muted)),
            Span::styled("@ ", Style::default().fg(p.success)),
            Span::styled("files  ", Style::default().fg(p.muted)),
            Span::styled("! ", Style::default().fg(p.warn)),
            Span::styled("shell  ", Style::default().fg(p.muted)),
            Span::styled("Tab ", Style::default().fg(p.fg)),
            Span::styled("complete  ", Style::default().fg(p.muted)),
            Span::styled("↑/↓ ", Style::default().fg(p.fg)),
            Span::styled("scroll  ", Style::default().fg(p.muted)),
            Span::styled("? ", Style::default().fg(p.fg)),
            Span::styled("help  ", Style::default().fg(p.muted)),
            Span::styled("Ctrl+X q ", Style::default().fg(p.fg)),
            Span::styled("quit", Style::default().fg(p.muted)),
        ]));
        frame.render_widget(footer, area);
    }

    fn draw_autocomplete_popup(
        &self,
        frame: &mut Frame,
        feed_area: Rect,
        mode: PopupMode,
        p: ThemePalette,
    ) {
        let items: Vec<ListItem> = match mode {
            PopupMode::SlashCommands => self
                .filtered_slash_commands()
                .into_iter()
                .enumerate()
                .map(|(i, cmd)| {
                    let selected = i == self.popup_selected;
                    let style = if selected {
                        Style::default().bg(p.accent).fg(p.bg)
                    } else {
                        Style::default().fg(p.fg)
                    };
                    let desc_style = if selected {
                        Style::default().bg(p.accent).fg(p.bg)
                    } else {
                        Style::default().fg(p.muted)
                    };
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            format!(" {:<13} ", cmd.name),
                            style.add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(cmd.description, desc_style),
                    ]))
                })
                .collect(),
            PopupMode::FilePicker => self
                .filtered_files()
                .into_iter()
                .take(8)
                .enumerate()
                .map(|(i, file)| {
                    let selected = i == self.popup_selected;
                    let style = if selected {
                        Style::default()
                            .bg(p.success)
                            .fg(p.bg)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(p.fg)
                    };
                    ListItem::new(Line::from(Span::styled(format!(" @{} ", file), style)))
                })
                .collect(),
            PopupMode::None => return,
        };

        if items.is_empty() {
            return;
        }

        let height = (items.len() as u16 + 2).min(10).min(feed_area.height);
        let width = feed_area.width.min(68);
        let popup_rect = Rect {
            x: feed_area.x + 1,
            y: feed_area.y + feed_area.height.saturating_sub(height),
            width: width.saturating_sub(2),
            height,
        };

        let title = if mode == PopupMode::SlashCommands {
            " Commands (Tab/Enter) "
        } else {
            " Media Files (Tab/Enter) "
        };

        frame.render_widget(Clear, popup_rect);
        let list = List::new(items).block(
            Block::default()
                .title(Span::styled(
                    title,
                    Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(p.border_focus))
                .style(Style::default().bg(p.surface)),
        );
        frame.render_widget(list, popup_rect);
    }

    fn draw_help_modal(&self, frame: &mut Frame, area: Rect, p: ThemePalette) {
        let w = area.width.saturating_sub(8).min(72);
        let h = area.height.saturating_sub(6).min(24);
        let rect = Rect {
            x: area.x + (area.width.saturating_sub(w)) / 2,
            y: area.y + (area.height.saturating_sub(h)) / 2,
            width: w,
            height: h,
        };

        let mut lines = vec![
            Line::from(Span::styled(
                "OpenCode-Style Interactive TUI for clop-rs",
                Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
            )),
            Line::raw(""),
        ];
        for cmd in SLASH_COMMANDS {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{:<12} ", cmd.name),
                    Style::default().fg(p.success).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{:<44}", cmd.description),
                    Style::default().fg(p.fg),
                ),
                Span::styled(cmd.shortcut, Style::default().fg(p.warn)),
            ]));
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "Press Esc or Enter to close",
            Style::default().fg(p.muted),
        )));

        frame.render_widget(Clear, rect);
        let modal = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .title(Span::styled(
                    " Help & Keybindings ",
                    Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(p.border_focus))
                .style(Style::default().bg(p.surface)),
        );
        frame.render_widget(modal, rect);
    }
}

/// Launches the interactive OpenCode-style TUI in the user's terminal.
pub fn run_tui(config: AppConfig, opts: TuiLaunchOptions) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = TuiApp::new(config, opts);
    let run_result = run_event_loop(&mut terminal, &mut app);

    // Always restore terminal state cleanly
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();

    run_result
}

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut TuiApp,
) -> Result<()> {
    while !app.should_quit {
        app.tick();
        terminal.draw(|f| app.draw(f))?;

        if event::poll(Duration::from_millis(80))? {
            if let Event::Key(key) = event::read()? {
                app.handle_key(key);
            }
        }
    }
    Ok(())
}

/// Extracts the active `@query` token at the end of the composer input, if any.
pub fn extract_at_query(input: &str) -> Option<&str> {
    let last_token = input.split_whitespace().last()?;
    last_token.strip_prefix('@')
}

/// Filters `SLASH_COMMANDS` by prefix/substring match against `input`.
pub fn filter_slash_commands(input: &str) -> Vec<&'static SlashCommandDef> {
    let q = input.trim().to_ascii_lowercase();
    SLASH_COMMANDS
        .iter()
        .filter(|cmd| cmd.name.starts_with(&q) || cmd.name.contains(q.trim_start_matches('/')))
        .collect()
}

/// Fuzzy-filters workspace media file paths against a query string.
pub fn filter_workspace_files<'a>(files: &'a [String], query: &str) -> Vec<&'a String> {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return files.iter().collect();
    }
    files
        .iter()
        .filter(|f| f.to_ascii_lowercase().contains(&q))
        .collect()
}

fn scan_workspace_media_files(root: &Path, limit: usize) -> Vec<String> {
    let mut results = Vec::new();
    scan_dir_limited(root, root, 2, limit, &mut results);
    results.sort();
    results
}

fn scan_dir_limited(root: &Path, dir: &Path, depth: usize, limit: usize, out: &mut Vec<String>) {
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
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') || name_str == "target" || name_str == "node_modules" {
            continue;
        }
        if path.is_file() && is_supported_media_path(&path) {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.display().to_string());
            }
        } else if path.is_dir() && depth > 0 {
            scan_dir_limited(root, &path, depth - 1, limit, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slash_command_filtering_and_at_query() {
        let matches = filter_slash_commands("/cl");
        assert!(matches.iter().any(|c| c.name == "/clipboard"));
        assert!(matches.iter().any(|c| c.name == "/clear"));

        assert_eq!(extract_at_query("@photo.png"), Some("photo.png"));
        assert_eq!(extract_at_query("/opt @hero"), Some("hero"));
        assert_eq!(extract_at_query("/clipboard"), None);
    }

    #[test]
    fn test_tui_commands_and_theme_cycling() {
        let mut app = TuiApp::new(AppConfig::default(), TuiLaunchOptions::default());
        assert_eq!(app.theme, TuiTheme::OpenCodeDark);
        assert!(app.show_sidebar);

        app.execute_command("/theme tokyo");
        assert_eq!(app.theme, TuiTheme::TokyoNight);

        app.execute_command("/details");
        assert!(!app.show_sidebar);

        app.execute_command("/preset smallest-size");
        assert_eq!(app.config.preset, OptimizationPreset::SmallestSize);

        app.execute_command("/watch");
        assert!(app.watch_clipboard);

        app.execute_command("/exit");
        assert!(app.should_quit);
    }
}
