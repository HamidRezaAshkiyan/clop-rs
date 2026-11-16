use crate::clipboard::OptimizationSummary;
use crate::optimizer::format_bytes;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::time::{Duration, Instant};
use tao::dpi::{LogicalPosition, LogicalSize, PhysicalPosition};
use tao::event::{ElementState, MouseButton, WindowEvent};
use tao::event_loop::EventLoopWindowTarget;
use tao::window::{Window, WindowBuilder, WindowId};

pub const HUD_WIDTH: u32 = 360;
pub const HUD_HEIGHT: u32 = 96;

/// Target that should be reverted if the user clicks the `[REVERT]` button on the HUD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevertSource {
    Clipboard,
    Downloads,
}

/// Current visual state of the Floating HUD Panel.
#[derive(Debug, Clone)]
pub enum HudPhase {
    Hidden,
    Optimizing {
        title: String,
        subtitle: String,
        thumbnail_64: Vec<u8>,
        started_at: Instant,
    },
    Completed {
        summary: OptimizationSummary,
        revert_source: RevertSource,
        completed_at: Instant,
        duration: Duration,
    },
    Reverted {
        source_label: String,
        restored_size: usize,
        thumbnail_64: Vec<u8>,
        reverted_at: Instant,
    },
}

/// User action triggered by clicking inside the Floating HUD Panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HudAction {
    None,
    Revert(RevertSource),
    Dismiss,
}

/// Floating always-on-top HUD panel that displays a progress bar, before/after file sizes,
/// thumbnail preview, EXIF badge, and interactive `[REVERT]` and `[X]` buttons.
pub struct FloatingHud {
    window: Option<Window>,
    pub phase: HudPhase,
    cursor_pos: Option<(f64, f64)>,
    hover_revert: bool,
    hover_close: bool,
    enabled: bool,
}

impl FloatingHud {
    pub fn new<T>(event_loop: &EventLoopWindowTarget<T>, enabled: bool) -> Self {
        let window = if enabled {
            Self::create_window(event_loop)
        } else {
            None
        };
        Self {
            window,
            phase: HudPhase::Hidden,
            cursor_pos: None,
            hover_revert: false,
            hover_close: false,
            enabled,
        }
    }

    fn create_window<T>(event_loop: &EventLoopWindowTarget<T>) -> Option<Window> {
        let mut builder = WindowBuilder::new()
            .with_title("clop-rs")
            .with_inner_size(LogicalSize::new(HUD_WIDTH as f64, HUD_HEIGHT as f64))
            .with_decorations(false)
            .with_always_on_top(true)
            .with_resizable(false)
            .with_focused(false)
            .with_visible(false);

        if let Some(monitor) = event_loop.primary_monitor() {
            let scale = monitor.scale_factor();
            let size = monitor.size();
            let logical_w = size.width as f64 / scale;
            let logical_h = size.height as f64 / scale;
            let x = (logical_w - HUD_WIDTH as f64 - 24.0).max(16.0);
            let y = (logical_h - HUD_HEIGHT as f64 - 64.0).max(16.0);
            builder = builder.with_position(LogicalPosition::new(x, y));
        }

        let win = builder.build(event_loop).ok()?;
        configure_non_activating_panel(&win);
        Some(win)
    }

    pub fn window_id(&self) -> Option<WindowId> {
        self.window.as_ref().map(|w| w.id())
    }

    pub fn set_enabled<T>(&mut self, enabled: bool, event_loop: &EventLoopWindowTarget<T>) {
        self.enabled = enabled;
        if !enabled {
            self.hide();
        } else if self.window.is_none() {
            self.window = Self::create_window(event_loop);
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Displays the HUD immediately with an active progress bar while optimization is running.
    pub fn show_optimizing(
        &mut self,
        source_label: &str,
        width: u32,
        height: u32,
        estimated_size: usize,
        thumbnail_64: Vec<u8>,
    ) {
        if !self.enabled {
            return;
        }
        self.phase = HudPhase::Optimizing {
            title: format!("OPTIMIZING {}...", source_label.to_ascii_uppercase()),
            subtitle: format!("{}x{}  ({})", width, height, format_bytes(estimated_size)),
            thumbnail_64,
            started_at: Instant::now(),
        };
        if let Some(ref win) = self.window {
            win.set_visible(true);
        }
        self.render();
    }

    /// Updates the HUD to show the completed before -> after size, percentage saved,
    /// visual compression ratio bar, and `[REVERT]` button.
    pub fn show_completed(&mut self, summary: OptimizationSummary, revert_source: RevertSource) {
        if !self.enabled {
            return;
        }
        self.phase = HudPhase::Completed {
            summary,
            revert_source,
            completed_at: Instant::now(),
            duration: Duration::from_secs(6),
        };
        if let Some(ref win) = self.window {
            win.set_visible(true);
        }
        self.render();
    }

    /// Updates the HUD to confirm that the last optimization was reverted to the original image.
    pub fn show_reverted(&mut self, source_label: &str, restored_size: usize) {
        if !self.enabled {
            return;
        }
        let thumb = match &self.phase {
            HudPhase::Completed { summary, .. } => summary.thumbnail_64.clone(),
            HudPhase::Optimizing { thumbnail_64, .. } => thumbnail_64.clone(),
            HudPhase::Reverted { thumbnail_64, .. } => thumbnail_64.clone(),
            HudPhase::Hidden => vec![0u8; 64 * 64 * 4],
        };
        self.phase = HudPhase::Reverted {
            source_label: source_label.to_string(),
            restored_size,
            thumbnail_64: thumb,
            reverted_at: Instant::now(),
        };
        if let Some(ref win) = self.window {
            win.set_visible(true);
        }
        self.render();
    }

    pub fn hide(&mut self) {
        self.phase = HudPhase::Hidden;
        self.cursor_pos = None;
        self.hover_revert = false;
        self.hover_close = false;
        if let Some(ref win) = self.window {
            win.set_visible(false);
        }
    }

    /// Periodic tick to animate the countdown bar and auto-dismiss the HUD when expired.
    pub fn tick(&mut self) {
        match &mut self.phase {
            HudPhase::Hidden => {}
            HudPhase::Optimizing { .. } => {
                self.render();
            }
            HudPhase::Completed {
                completed_at,
                duration,
                ..
            } => {
                // Pause auto-hide countdown while cursor is hovering over the HUD panel
                if self.cursor_pos.is_some() {
                    *completed_at = Instant::now();
                } else if completed_at.elapsed() >= *duration {
                    self.hide();
                    return;
                }
                self.render();
            }
            HudPhase::Reverted { reverted_at, .. } => {
                if reverted_at.elapsed() >= Duration::from_millis(2600) {
                    self.hide();
                    return;
                }
                self.render();
            }
        }
    }

    /// Handles mouse movement and clicks inside the Floating HUD window.
    pub fn handle_window_event(&mut self, event: &WindowEvent<'_>) -> HudAction {
        let Some(ref win) = self.window else {
            return HudAction::None;
        };
        let scale = win.scale_factor();

        match event {
            WindowEvent::CursorMoved {
                position: PhysicalPosition { x, y },
                ..
            } => {
                let lx = *x / scale;
                let ly = *y / scale;
                self.cursor_pos = Some((lx, ly));
                let new_hover_revert =
                    is_in_revert_btn(lx, ly) && matches!(self.phase, HudPhase::Completed { .. });
                let new_hover_close = is_in_close_btn(lx, ly);
                if new_hover_revert != self.hover_revert || new_hover_close != self.hover_close {
                    self.hover_revert = new_hover_revert;
                    self.hover_close = new_hover_close;
                    self.render();
                }
                HudAction::None
            }
            WindowEvent::CursorLeft { .. } => {
                self.cursor_pos = None;
                if self.hover_revert || self.hover_close {
                    self.hover_revert = false;
                    self.hover_close = false;
                    self.render();
                }
                HudAction::None
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                if let Some((lx, ly)) = self.cursor_pos {
                    if is_in_close_btn(lx, ly) {
                        self.hide();
                        return HudAction::Dismiss;
                    }
                    if is_in_revert_btn(lx, ly) {
                        if let HudPhase::Completed { revert_source, .. } = self.phase {
                            return HudAction::Revert(revert_source);
                        }
                    }
                }
                HudAction::None
            }
            _ => HudAction::None,
        }
    }

    /// Draws the current HUD state onto an RGBA canvas and blits it to the native window.
    pub fn render(&self) {
        let Some(ref win) = self.window else {
            return;
        };
        if matches!(self.phase, HudPhase::Hidden) {
            return;
        }

        let mut canvas = Canvas::new(HUD_WIDTH, HUD_HEIGHT);

        // 1. Card background & outer border
        canvas.fill_rect(0, 0, HUD_WIDTH, HUD_HEIGHT, [18, 24, 34, 255]);
        canvas.stroke_rect(0, 0, HUD_WIDTH, HUD_HEIGHT, [48, 62, 82, 255]);
        canvas.stroke_rect(1, 1, HUD_WIDTH - 2, HUD_HEIGHT - 2, [30, 41, 59, 255]);

        match &self.phase {
            HudPhase::Hidden => return,
            HudPhase::Optimizing {
                title,
                subtitle,
                thumbnail_64,
                started_at,
            } => {
                canvas.draw_thumbnail(14, 16, thumbnail_64);
                canvas.draw_text(90, 16, title, 1, [56, 189, 248, 255]);
                canvas.draw_text(90, 36, subtitle, 2, [241, 245, 249, 255]);

                // Animated progress bar (x: 90..344, y: 64..76)
                let bar_w = 254u32;
                canvas.fill_rect(90, 64, bar_w, 12, [30, 41, 59, 255]);
                let elapsed_ms = started_at.elapsed().as_millis() as u32;
                let fill_w =
                    ((bar_w * 35) / 100 + (elapsed_ms / 4) % (bar_w * 60 / 100)).min(bar_w);
                canvas.fill_rect(90, 64, fill_w, 12, [16, 185, 129, 255]);
                canvas.stroke_rect(90, 64, bar_w, 12, [51, 65, 85, 255]);
            }
            HudPhase::Completed {
                summary,
                completed_at,
                duration,
                ..
            } => {
                canvas.draw_thumbnail(14, 16, &summary.thumbnail_64);

                // Header: e.g. "OPTIMIZED CLIPBOARD (-76.7%)"
                let src = truncate_str(&summary.source_label.to_ascii_uppercase(), 12);
                let header = if summary.saved_percent >= 0.1 {
                    format!("{} -{:.1}%", src, summary.saved_percent)
                } else {
                    format!("{} OPTIMIZED", src)
                };
                canvas.draw_text(90, 15, &header, 1, [52, 211, 153, 255]);

                // Optional [EXIF] badge if EXIF metadata was stripped
                if summary.exif_stripped {
                    let bx = 90 + (header.len() as u32 * 6) + 6;
                    if bx + 34 < 244 {
                        canvas.fill_rect(bx, 13, 30, 11, [6, 78, 59, 255]);
                        canvas.stroke_rect(bx, 13, 30, 11, [16, 185, 129, 255]);
                        canvas.draw_text(bx + 3, 15, "EXIF", 1, [167, 243, 208, 255]);
                    }
                }

                // Interactive [REVERT] button (x: 248..320, y: 11..29)
                let rev_bg = if self.hover_revert {
                    [37, 99, 235, 255]
                } else {
                    [30, 41, 59, 255]
                };
                let rev_border = if self.hover_revert {
                    [96, 165, 250, 255]
                } else {
                    [71, 85, 105, 255]
                };
                canvas.fill_rect(248, 11, 72, 18, rev_bg);
                canvas.stroke_rect(248, 11, 72, 18, rev_border);
                canvas.draw_text(266, 16, "REVERT", 1, [248, 250, 252, 255]);

                // Interactive [X] close button (x: 326..346, y: 11..29)
                let cls_bg = if self.hover_close {
                    [185, 28, 28, 255]
                } else {
                    [30, 41, 59, 255]
                };
                canvas.fill_rect(326, 11, 20, 18, cls_bg);
                canvas.stroke_rect(326, 11, 20, 18, [71, 85, 105, 255]);
                canvas.draw_text(334, 16, "X", 1, [226, 232, 240, 255]);

                // Main size comparison in large 2x font: "445.7 KB -> 103.7 KB"
                let size_line = format!(
                    "{} -> {}",
                    format_bytes(summary.original_size),
                    format_bytes(summary.optimized_size)
                );
                canvas.draw_text(90, 38, &size_line, 2, [248, 250, 252, 255]);

                // Compression ratio progress bar (x: 90..346, y: 64..76)
                let bar_w = 256u32;
                canvas.fill_rect(90, 64, bar_w, 12, [6, 78, 59, 255]);
                let ratio = if summary.original_size > 0 {
                    (summary.optimized_size as f64 / summary.original_size as f64).clamp(0.05, 1.0)
                } else {
                    1.0
                };
                let new_w = ((bar_w as f64) * ratio).round() as u32;
                canvas.fill_rect(90, 64, new_w, 12, [16, 185, 129, 255]);
                canvas.stroke_rect(90, 64, bar_w, 12, [52, 211, 153, 255]);

                // Bottom countdown bar (y: 92..95)
                let elapsed = completed_at.elapsed().as_secs_f64();
                let total = duration.as_secs_f64().max(0.1);
                let rem = (1.0 - elapsed / total).clamp(0.0, 1.0);
                let rem_w = ((HUD_WIDTH as f64 - 4.0) * rem).round() as u32;
                canvas.fill_rect(2, 92, rem_w, 2, [52, 211, 153, 200]);
            }
            HudPhase::Reverted {
                source_label,
                restored_size,
                thumbnail_64,
                ..
            } => {
                canvas.draw_thumbnail(14, 16, thumbnail_64);
                let header = format!(
                    "REVERTED {}",
                    truncate_str(&source_label.to_ascii_uppercase(), 14)
                );
                canvas.draw_text(90, 18, &header, 1, [251, 191, 36, 255]);
                let msg = format!("Restored {}", format_bytes(*restored_size));
                canvas.draw_text(90, 38, &msg, 2, [248, 250, 252, 255]);
                canvas.fill_rect(90, 64, 256, 12, [217, 119, 6, 255]);
            }
        }

        present_canvas_to_window(win, &canvas);
    }
}

fn is_in_revert_btn(x: f64, y: f64) -> bool {
    (248.0..=320.0).contains(&x) && (11.0..=29.0).contains(&y)
}

fn is_in_close_btn(x: f64, y: f64) -> bool {
    (326.0..=346.0).contains(&x) && (11.0..=29.0).contains(&y)
}

fn truncate_str(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        out.push('~');
        out
    }
}

struct Canvas {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl Canvas {
    fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            rgba: vec![0u8; (width * height * 4) as usize],
        }
    }

    fn set_px(&mut self, x: u32, y: u32, color: [u8; 4]) {
        if x < self.width && y < self.height {
            let idx = ((y * self.width + x) * 4) as usize;
            self.rgba[idx..idx + 4].copy_from_slice(&color);
        }
    }

    fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, color: [u8; 4]) {
        let x_end = (x + w).min(self.width);
        let y_end = (y + h).min(self.height);
        for py in y..y_end {
            for px in x..x_end {
                self.set_px(px, py, color);
            }
        }
    }

    fn stroke_rect(&mut self, x: u32, y: u32, w: u32, h: u32, color: [u8; 4]) {
        if w == 0 || h == 0 {
            return;
        }
        for px in x..(x + w).min(self.width) {
            self.set_px(px, y, color);
            self.set_px(px, (y + h - 1).min(self.height - 1), color);
        }
        for py in y..(y + h).min(self.height) {
            self.set_px(x, py, color);
            self.set_px((x + w - 1).min(self.width - 1), py, color);
        }
    }

    fn draw_thumbnail(&mut self, x: u32, y: u32, thumb_64: &[u8]) {
        self.fill_rect(x, y, 64, 64, [15, 23, 42, 255]);
        if thumb_64.len() >= 64 * 64 * 4 {
            for ty in 0..64u32 {
                for tx in 0..64u32 {
                    let sidx = ((ty * 64 + tx) * 4) as usize;
                    let a = thumb_64[sidx + 3] as u32;
                    if a > 0 {
                        let r = (thumb_64[sidx] as u32 * a + 15 * (255 - a)) / 255;
                        let g = (thumb_64[sidx + 1] as u32 * a + 23 * (255 - a)) / 255;
                        let b = (thumb_64[sidx + 2] as u32 * a + 42 * (255 - a)) / 255;
                        self.set_px(x + tx, y + ty, [r as u8, g as u8, b as u8, 255]);
                    }
                }
            }
        }
        self.stroke_rect(x, y, 64, 64, [71, 85, 105, 255]);
    }

    fn draw_text(&mut self, x: u32, y: u32, text: &str, scale: u32, color: [u8; 4]) {
        let mut cursor_x = x;
        let scale = scale.max(1);
        for ch in text.chars() {
            let glyph = glyph_5x7(ch);
            for (row_idx, &row_bits) in glyph.iter().enumerate() {
                for col_idx in 0..5u32 {
                    if (row_bits >> (4 - col_idx)) & 1 == 1 {
                        let gx = cursor_x + col_idx * scale;
                        let gy = y + (row_idx as u32) * scale;
                        self.fill_rect(gx, gy, scale, scale, color);
                    }
                }
            }
            cursor_x += 6 * scale;
            if cursor_x >= self.width - 4 {
                break;
            }
        }
    }
}

/// 5x7 bitmap font for crisp HUD rendering without external font file dependencies.
fn glyph_5x7(ch: char) -> [u8; 7] {
    match ch.to_ascii_uppercase() {
        'A' => [
            0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'B' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110,
        ],
        'C' => [
            0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110,
        ],
        'D' => [
            0b11100, 0b10010, 0b10001, 0b10001, 0b10001, 0b10010, 0b11100,
        ],
        'E' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
        ],
        'F' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'G' => [
            0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01110,
        ],
        'H' => [
            0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'I' => [
            0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        'J' => [
            0b00111, 0b00010, 0b00010, 0b00010, 0b10010, 0b10010, 0b01100,
        ],
        'K' => [
            0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001,
        ],
        'L' => [
            0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111,
        ],
        'M' => [
            0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001,
        ],
        'N' => [
            0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001,
        ],
        'O' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'P' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'Q' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101,
        ],
        'R' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001,
        ],
        'S' => [
            0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        'T' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'U' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'V' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100,
        ],
        'W' => [
            0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b10101, 0b01010,
        ],
        'X' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001,
        ],
        'Y' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'Z' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111,
        ],
        '0' => [
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        '1' => [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        '2' => [
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
        ],
        '3' => [
            0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        '4' => [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        '5' => [
            0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
        ],
        '6' => [
            0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
        '7' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        '8' => [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        '9' => [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
        ],
        '.' => [
            0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b01100, 0b01100,
        ],
        ',' => [
            0b00000, 0b00000, 0b00000, 0b00000, 0b00110, 0b00100, 0b01000,
        ],
        ':' => [
            0b00000, 0b01100, 0b01100, 0b00000, 0b01100, 0b01100, 0b00000,
        ],
        '-' => [
            0b00000, 0b00000, 0b00000, 0b11111, 0b00000, 0b00000, 0b00000,
        ],
        '>' => [
            0b10000, 0b01000, 0b00100, 0b00010, 0b00100, 0b01000, 0b10000,
        ],
        '<' => [
            0b00001, 0b00010, 0b00100, 0b01000, 0b00100, 0b00010, 0b00001,
        ],
        '(' => [
            0b00010, 0b00100, 0b01000, 0b01000, 0b01000, 0b00100, 0b00010,
        ],
        ')' => [
            0b01000, 0b00100, 0b00010, 0b00010, 0b00010, 0b00100, 0b01000,
        ],
        '%' => [
            0b11001, 0b11010, 0b00010, 0b00100, 0b01000, 0b01011, 0b10011,
        ],
        '/' => [
            0b00001, 0b00010, 0b00010, 0b00100, 0b01000, 0b01000, 0b10000,
        ],
        '_' => [
            0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b11111,
        ],
        '~' => [
            0b00000, 0b00000, 0b01101, 0b10110, 0b00000, 0b00000, 0b00000,
        ],
        _ => [
            0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000,
        ],
    }
}

fn configure_non_activating_panel(window: &Window) {
    #[cfg(target_os = "macos")]
    {
        if let Ok(handle) = window.window_handle() {
            if let RawWindowHandle::AppKit(appkit) = handle.as_raw() {
                unsafe {
                    use objc2::msg_send;
                    use objc2::runtime::AnyObject;
                    let ns_view = appkit.ns_view.as_ptr() as *mut AnyObject;
                    if !ns_view.is_null() {
                        let _: () = msg_send![ns_view, setWantsLayer: true];
                        let ns_win: *mut AnyObject = msg_send![ns_view, window];
                        if !ns_win.is_null() {
                            // NSStatusWindowLevel (25) so it floats above normal windows
                            let _: () = msg_send![ns_win, setLevel: 25isize];
                            let _: () = msg_send![ns_win, setHasShadow: true];
                        }
                    }
                }
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = window;
    }
}

fn present_canvas_to_window(window: &Window, canvas: &Canvas) {
    let Ok(handle) = window.window_handle() else {
        return;
    };

    match handle.as_raw() {
        #[cfg(target_os = "macos")]
        RawWindowHandle::AppKit(appkit) => unsafe {
            use core_graphics::base::{kCGBitmapByteOrderDefault, kCGImageAlphaLast};
            use core_graphics::color_space::CGColorSpace;
            use core_graphics::data_provider::CGDataProvider;
            use core_graphics::image::CGImage;
            use objc2::msg_send;
            use objc2::runtime::AnyObject;
            use std::sync::Arc;

            let ns_view = appkit.ns_view.as_ptr() as *mut AnyObject;
            if ns_view.is_null() {
                return;
            }
            let _: () = msg_send![ns_view, setWantsLayer: true];
            let layer: *mut AnyObject = msg_send![ns_view, layer];
            if layer.is_null() {
                return;
            }

            let cs = CGColorSpace::create_device_rgb();
            let buf_arc = Arc::new(canvas.rgba.clone());
            let provider = CGDataProvider::from_buffer(buf_arc);
            let cg_img = CGImage::new(
                canvas.width as usize,
                canvas.height as usize,
                8,
                32,
                (canvas.width * 4) as usize,
                &cs,
                kCGBitmapByteOrderDefault | kCGImageAlphaLast,
                &provider,
                false,
                0,
            );
            use foreign_types::ForeignType;
            let cg_img_ptr = cg_img.as_ptr() as *mut AnyObject;
            let _: () = msg_send![layer, setContents: cg_img_ptr];
        },
        #[cfg(target_os = "windows")]
        RawWindowHandle::Win32(win32) => unsafe {
            use std::mem::size_of;
            use windows_sys::Win32::Foundation::HWND;
            use windows_sys::Win32::Graphics::Gdi::{
                GetDC, ReleaseDC, StretchDIBits, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
                DIB_RGB_COLORS, RGBQUAD, SRCCOPY,
            };

            let hwnd = win32.hwnd.get() as HWND;
            let hdc = GetDC(hwnd);
            if hdc.is_null() {
                return;
            }

            // Convert RGBA to BGRA top-down DIB
            let mut bgra = Vec::with_capacity(canvas.rgba.len());
            #[allow(clippy::chunks_exact_to_as_chunks)]
            for px in canvas.rgba.chunks_exact(4) {
                bgra.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
            }

            let phys_size = window.inner_size();
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: canvas.width as i32,
                    biHeight: -(canvas.height as i32), // top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    biSizeImage: 0,
                    biXPelsPerMeter: 0,
                    biYPelsPerMeter: 0,
                    biClrUsed: 0,
                    biClrImportant: 0,
                },
                bmiColors: [RGBQUAD {
                    rgbBlue: 0,
                    rgbGreen: 0,
                    rgbRed: 0,
                    rgbReserved: 0,
                }],
            };

            StretchDIBits(
                hdc,
                0,
                0,
                phys_size.width as i32,
                phys_size.height as i32,
                0,
                0,
                canvas.width as i32,
                canvas.height as i32,
                bgra.as_ptr() as *const _,
                &bmi,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
            ReleaseDC(hwnd, hdc);
        },
        _ => {}
    }
}
