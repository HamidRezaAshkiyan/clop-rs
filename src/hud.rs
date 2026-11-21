use crate::clipboard::OptimizationSummary;
use crate::optimizer::format_bytes;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::time::{Duration, Instant};
use tao::dpi::{LogicalPosition, LogicalSize, PhysicalPosition};
use tao::event::{ElementState, MouseButton, WindowEvent};
use tao::event_loop::EventLoopWindowTarget;
use tao::window::{Window, WindowBuilder, WindowId};

/// Logical toast dimensions (rendered internally at 2x supersampled HiDPI resolution).
pub const HUD_WIDTH: u32 = 392;
pub const HUD_HEIGHT: u32 = 104;
const HUD_SCALE: u32 = 2;
const FADE_IN_MS: f64 = 160.0;

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

/// Floating always-on-top HUD toast that displays a HiDPI glassmorphic card with
/// rounded corners, animated shimmer progress bar, before/after sizes, savings pill,
/// elapsed time, thumbnail preview, EXIF badge, hover-to-pause indicator, and
/// interactive `[REVERT]` and `[X]` buttons.
pub struct FloatingHud {
    window: Option<Window>,
    pub phase: HudPhase,
    shown_at: Option<Instant>,
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
            shown_at: None,
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
            .with_transparent(true)
            .with_always_on_top(true)
            .with_resizable(false)
            .with_focused(false)
            .with_visible(false);

        if let Some(monitor) = event_loop.primary_monitor() {
            let scale = monitor.scale_factor();
            let size = monitor.size();
            let logical_w = size.width as f64 / scale;
            let logical_h = size.height as f64 / scale;
            let x = (logical_w - HUD_WIDTH as f64 - 20.0).max(16.0);
            let y = (logical_h - HUD_HEIGHT as f64 - 56.0).max(16.0);
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

    /// Displays the HUD immediately with an active shimmer progress bar while optimization runs.
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
        let was_hidden = matches!(self.phase, HudPhase::Hidden);
        self.phase = HudPhase::Optimizing {
            title: format!(
                "OPTIMIZING {}",
                truncate_str(&source_label.to_ascii_uppercase(), 14)
            ),
            subtitle: if width > 0 && height > 0 {
                format!("{}x{}  {}", width, height, format_bytes(estimated_size))
            } else {
                format_bytes(estimated_size)
            },
            thumbnail_64,
            started_at: Instant::now(),
        };
        if was_hidden {
            self.shown_at = Some(Instant::now());
        }
        if let Some(ref win) = self.window {
            set_window_alpha(win, self.current_alpha());
            win.set_visible(true);
        }
        self.render();
    }

    /// Updates the HUD to show the completed before -> after size, percentage saved,
    /// visual compression ratio bar, elapsed time, and `[REVERT]` button.
    pub fn show_completed(&mut self, summary: OptimizationSummary, revert_source: RevertSource) {
        if !self.enabled {
            return;
        }
        let was_hidden = matches!(self.phase, HudPhase::Hidden);
        self.phase = HudPhase::Completed {
            summary,
            revert_source,
            completed_at: Instant::now(),
            duration: Duration::from_secs(6),
        };
        if was_hidden {
            self.shown_at = Some(Instant::now());
        }
        if let Some(ref win) = self.window {
            set_window_alpha(win, self.current_alpha());
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
        if matches!(self.phase, HudPhase::Hidden) {
            self.shown_at = Some(Instant::now());
        }
        self.phase = HudPhase::Reverted {
            source_label: source_label.to_string(),
            restored_size,
            thumbnail_64: thumb,
            reverted_at: Instant::now(),
        };
        if let Some(ref win) = self.window {
            set_window_alpha(win, self.current_alpha());
            win.set_visible(true);
        }
        self.render();
    }

    pub fn hide(&mut self) {
        self.phase = HudPhase::Hidden;
        self.shown_at = None;
        self.cursor_pos = None;
        self.hover_revert = false;
        self.hover_close = false;
        if let Some(ref win) = self.window {
            win.set_visible(false);
        }
    }

    fn current_alpha(&self) -> f64 {
        let Some(shown) = self.shown_at else {
            return 1.0;
        };
        ease_out_cubic((shown.elapsed().as_secs_f64() * 1000.0) / FADE_IN_MS)
    }

    /// Periodic tick to animate fade-in, shimmer bar, countdown bar, and auto-dismiss when expired.
    pub fn tick(&mut self) {
        if let Some(ref win) = self.window {
            if !matches!(self.phase, HudPhase::Hidden) {
                set_window_alpha(win, self.current_alpha());
            }
        }
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
                if self.cursor_pos.is_some() {
                    *reverted_at = Instant::now();
                } else if reverted_at.elapsed() >= Duration::from_millis(2600) {
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
                let was_hovering = self.cursor_pos.is_some();
                self.cursor_pos = Some((lx, ly));
                let new_hover_revert =
                    is_in_revert_btn(lx, ly) && matches!(self.phase, HudPhase::Completed { .. });
                let new_hover_close = is_in_close_btn(lx, ly);
                if new_hover_revert != self.hover_revert
                    || new_hover_close != self.hover_close
                    || !was_hovering
                {
                    self.hover_revert = new_hover_revert;
                    self.hover_close = new_hover_close;
                    self.render();
                }
                HudAction::None
            }
            WindowEvent::CursorLeft { .. } => {
                self.cursor_pos = None;
                self.hover_revert = false;
                self.hover_close = false;
                self.render();
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

    /// Draws the current HUD state onto a 2x HiDPI RGBA canvas and blits it to the native window.
    pub fn render(&self) {
        let Some(ref win) = self.window else {
            return;
        };
        if matches!(self.phase, HudPhase::Hidden) {
            return;
        }

        let canvas = self.render_to_canvas();
        present_canvas_to_window(win, &canvas);
    }

    fn render_to_canvas(&self) -> Canvas {
        let mut canvas = Canvas::new(HUD_WIDTH, HUD_HEIGHT, HUD_SCALE);
        let hovering = self.cursor_pos.is_some();

        // 1. Glassmorphic vertical gradient card with rounded corners
        canvas.fill_rounded_gradient(
            0,
            0,
            HUD_WIDTH,
            HUD_HEIGHT,
            14,
            [20, 27, 40, 246],
            [13, 18, 28, 250],
        );

        // Outer border (brightens slightly on hover to indicate paused auto-dismiss)
        let border_col = if hovering {
            [74, 96, 128, 255]
        } else {
            [45, 58, 79, 255]
        };
        canvas.stroke_rounded_rect(0, 0, HUD_WIDTH, HUD_HEIGHT, 14, border_col);
        // Subtle inner top specular highlight
        canvas.fill_rect(14, 1, HUD_WIDTH - 28, 1, [255, 255, 255, 22]);

        // Left status accent pill
        let accent_col = match &self.phase {
            HudPhase::Optimizing { .. } => [56, 189, 248, 255], // Sky blue
            HudPhase::Completed { .. } => [16, 185, 129, 255],  // Emerald
            HudPhase::Reverted { .. } => [245, 158, 11, 255],   // Amber
            HudPhase::Hidden => [71, 85, 105, 255],
        };
        canvas.fill_rounded_rect(5, 14, 3, HUD_HEIGHT - 28, 2, accent_col);

        match &self.phase {
            HudPhase::Hidden => {}
            HudPhase::Optimizing {
                title,
                subtitle,
                thumbnail_64,
                started_at,
            } => {
                canvas.draw_rounded_thumbnail(16, 18, thumbnail_64, 8);
                canvas.draw_text(92, 16, title, 1, [56, 189, 248, 255]);
                canvas.draw_text(92, 37, subtitle, 2, [241, 245, 249, 255]);

                // Close button
                draw_close_button(&mut canvas, self.hover_close);

                // Animated shimmer progress bar (x: 92..374, y: 69..81)
                let bar_x = 92u32;
                let bar_y = 69u32;
                let bar_w = 282u32;
                let bar_h = 10u32;
                canvas.fill_rounded_rect(bar_x, bar_y, bar_w, bar_h, 5, [30, 41, 59, 255]);

                let elapsed_ms = started_at.elapsed().as_millis() as u32;
                let shimmer_w = 88u32;
                let cycle = bar_w + shimmer_w;
                let pos = (elapsed_ms / 4) % cycle;
                let seg_start = pos.saturating_sub(shimmer_w);
                let seg_end = pos.min(bar_w);
                if seg_end > seg_start {
                    canvas.fill_rounded_rect(
                        bar_x + seg_start,
                        bar_y,
                        seg_end - seg_start,
                        bar_h,
                        5,
                        [56, 189, 248, 255],
                    );
                }
                canvas.stroke_rounded_rect(bar_x, bar_y, bar_w, bar_h, 5, [51, 65, 85, 255]);
            }
            HudPhase::Completed {
                summary,
                completed_at,
                duration,
                ..
            } => {
                canvas.draw_rounded_thumbnail(16, 18, &summary.thumbnail_64, 8);

                // Top metadata row: Source label + Savings pill + optional [EXIF] pill
                let src = truncate_str(&summary.source_label.to_ascii_uppercase(), 10);
                canvas.draw_text(92, 16, &src, 1, [148, 163, 184, 255]);

                let mut badge_x = 92 + (src.len() as u32 * 6) + 7;
                if summary.saved_percent >= 0.1 {
                    let pct = format!("-{:.1}%", summary.saved_percent);
                    let pill_w = (pct.len() as u32 * 6) + 10;
                    if badge_x + pill_w < 276 {
                        canvas.fill_rounded_rect(badge_x, 12, pill_w, 15, 7, [6, 78, 59, 255]);
                        canvas.stroke_rounded_rect(badge_x, 12, pill_w, 15, 7, [16, 185, 129, 255]);
                        canvas.draw_text(badge_x + 5, 16, &pct, 1, [167, 243, 208, 255]);
                        badge_x += pill_w + 5;
                    }
                }

                if summary.exif_stripped {
                    let exif_w = 34u32;
                    if badge_x + exif_w < 276 {
                        canvas.fill_rounded_rect(badge_x, 12, exif_w, 15, 7, [30, 58, 138, 255]);
                        canvas.stroke_rounded_rect(badge_x, 12, exif_w, 15, 7, [96, 165, 250, 255]);
                        canvas.draw_text(badge_x + 5, 16, "EXIF", 1, [191, 219, 254, 255]);
                    }
                }

                // Interactive rounded [REVERT] button (x: 280..350, y: 11..30)
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
                canvas.fill_rounded_rect(280, 11, 70, 19, 6, rev_bg);
                canvas.stroke_rounded_rect(280, 11, 70, 19, 6, rev_border);
                canvas.draw_text(297, 17, "REVERT", 1, [248, 250, 252, 255]);

                // Interactive rounded [X] close button (x: 356..376, y: 11..30)
                draw_close_button(&mut canvas, self.hover_close);

                // Main size comparison in crisp 2x font: "445.7 KB -> 103.7 KB"
                let size_line = format!(
                    "{} -> {}",
                    format_bytes(summary.original_size),
                    format_bytes(summary.optimized_size)
                );
                canvas.draw_text(92, 39, &size_line, 2, [248, 250, 252, 255]);

                // Compression ratio progress bar (x: 92..376, y: 66..76)
                let bar_w = 284u32;
                canvas.fill_rounded_rect(92, 66, bar_w, 10, 5, [6, 78, 59, 180]);
                let ratio = if summary.original_size > 0 {
                    (summary.optimized_size as f64 / summary.original_size as f64).clamp(0.06, 1.0)
                } else {
                    1.0
                };
                let new_w = ((bar_w as f64) * ratio).round() as u32;
                canvas.fill_rounded_rect(92, 66, new_w.max(10), 10, 5, [16, 185, 129, 255]);
                canvas.stroke_rounded_rect(92, 66, bar_w, 10, 5, [52, 211, 153, 180]);

                // Footer micro-info: saved bytes + elapsed time (or PAUSED on hover)
                let footer_left = format!(
                    "SAVED {}  ({})",
                    format_bytes(summary.saved_bytes),
                    format_elapsed(summary.elapsed)
                );
                canvas.draw_text(92, 82, &footer_left, 1, [148, 163, 184, 255]);

                if hovering {
                    canvas.draw_text(336, 82, "PAUSED", 1, [251, 191, 36, 255]);
                }

                // Smooth bottom countdown bar (y: 98..101)
                let elapsed = completed_at.elapsed().as_secs_f64();
                let total = duration.as_secs_f64().max(0.1);
                let rem = if hovering {
                    1.0
                } else {
                    (1.0 - elapsed / total).clamp(0.0, 1.0)
                };
                let rem_w = ((HUD_WIDTH as f64 - 28.0) * rem).round() as u32;
                if rem_w > 2 {
                    let bar_col = if hovering {
                        [251, 191, 36, 180]
                    } else {
                        [52, 211, 153, 210]
                    };
                    canvas.fill_rounded_rect(14, 98, rem_w, 3, 2, bar_col);
                }
            }
            HudPhase::Reverted {
                source_label,
                restored_size,
                thumbnail_64,
                reverted_at,
            } => {
                canvas.draw_rounded_thumbnail(16, 18, thumbnail_64, 8);
                let header = format!(
                    "REVERTED {}",
                    truncate_str(&source_label.to_ascii_uppercase(), 14)
                );
                canvas.draw_text(92, 16, &header, 1, [251, 191, 36, 255]);
                draw_close_button(&mut canvas, self.hover_close);

                let msg = format!("RESTORED {}", format_bytes(*restored_size));
                canvas.draw_text(92, 39, &msg, 2, [248, 250, 252, 255]);
                canvas.fill_rounded_rect(92, 66, 284, 10, 5, [217, 119, 6, 220]);

                let rem = if hovering {
                    1.0
                } else {
                    (1.0 - reverted_at.elapsed().as_secs_f64() / 2.6).clamp(0.0, 1.0)
                };
                let rem_w = ((HUD_WIDTH as f64 - 28.0) * rem).round() as u32;
                if rem_w > 2 {
                    canvas.fill_rounded_rect(14, 98, rem_w, 3, 2, [251, 191, 36, 200]);
                }
            }
        }

        canvas
    }
}

fn draw_close_button(canvas: &mut Canvas, hover_close: bool) {
    let cls_bg = if hover_close {
        [185, 28, 28, 255]
    } else {
        [30, 41, 59, 255]
    };
    let cls_border = if hover_close {
        [248, 113, 113, 255]
    } else {
        [71, 85, 105, 255]
    };
    canvas.fill_rounded_rect(356, 11, 20, 19, 6, cls_bg);
    canvas.stroke_rounded_rect(356, 11, 20, 19, 6, cls_border);
    canvas.draw_text(364, 17, "X", 1, [226, 232, 240, 255]);
}

fn is_in_revert_btn(x: f64, y: f64) -> bool {
    (280.0..=350.0).contains(&x) && (11.0..=30.0).contains(&y)
}

fn is_in_close_btn(x: f64, y: f64) -> bool {
    (356.0..=376.0).contains(&x) && (11.0..=30.0).contains(&y)
}

fn ease_out_cubic(t: f64) -> f64 {
    let c = t.clamp(0.0, 1.0);
    1.0 - (1.0 - c).powi(3)
}

fn format_elapsed(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{}MS", ms)
    } else {
        format!("{:.1}S", d.as_secs_f64())
    }
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

/// Supersampled RGBA software canvas supporting alpha blending, gradients,
/// and anti-aliased rounded rectangles.
struct Canvas {
    width: u32,
    height: u32,
    scale: u32,
    rgba: Vec<u8>,
}

impl Canvas {
    fn new(logical_width: u32, logical_height: u32, scale: u32) -> Self {
        let scale = scale.max(1);
        let width = logical_width * scale;
        let height = logical_height * scale;
        Self {
            width,
            height,
            scale,
            rgba: vec![0u8; (width * height * 4) as usize],
        }
    }

    fn blend_px(&mut self, px: u32, py: u32, color: [u8; 4]) {
        if px >= self.width || py >= self.height || color[3] == 0 {
            return;
        }
        let idx = ((py * self.width + px) * 4) as usize;
        let sa = color[3] as f32 / 255.0;
        let da = self.rgba[idx + 3] as f32 / 255.0;
        let out_a = sa + da * (1.0 - sa);
        if out_a <= 0.0 {
            return;
        }
        let blend = |sc: u8, dc: u8| -> u8 {
            (((sc as f32 * sa) + (dc as f32 * da * (1.0 - sa))) / out_a)
                .round()
                .clamp(0.0, 255.0) as u8
        };
        self.rgba[idx] = blend(color[0], self.rgba[idx]);
        self.rgba[idx + 1] = blend(color[1], self.rgba[idx + 1]);
        self.rgba[idx + 2] = blend(color[2], self.rgba[idx + 2]);
        self.rgba[idx + 3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
    }

    fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, color: [u8; 4]) {
        let sx = x * self.scale;
        let sy = y * self.scale;
        let x_end = (sx + w * self.scale).min(self.width);
        let y_end = (sy + h * self.scale).min(self.height);
        for py in sy..y_end {
            for px in sx..x_end {
                self.blend_px(px, py, color);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn fill_rounded_gradient(
        &mut self,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        radius: u32,
        top: [u8; 4],
        bottom: [u8; 4],
    ) {
        let sx = x * self.scale;
        let sy = y * self.scale;
        let sw = w * self.scale;
        let sh = h * self.scale;
        let r = (radius * self.scale).min(sw / 2).min(sh / 2) as f32;
        let x_end = (sx + sw).min(self.width);
        let y_end = (sy + sh).min(self.height);

        for py in sy..y_end {
            let t = if sh > 1 {
                (py - sy) as f32 / (sh - 1) as f32
            } else {
                0.0
            };
            let base = lerp_color(top, bottom, t);
            for px in sx..x_end {
                let cov = rounded_rect_coverage(
                    (px - sx) as f32 + 0.5,
                    (py - sy) as f32 + 0.5,
                    sw as f32,
                    sh as f32,
                    r,
                );
                if cov > 0.0 {
                    let mut c = base;
                    c[3] = ((c[3] as f32) * cov).round() as u8;
                    self.blend_px(px, py, c);
                }
            }
        }
    }

    fn fill_rounded_rect(&mut self, x: u32, y: u32, w: u32, h: u32, radius: u32, color: [u8; 4]) {
        self.fill_rounded_gradient(x, y, w, h, radius, color, color);
    }

    fn stroke_rounded_rect(&mut self, x: u32, y: u32, w: u32, h: u32, radius: u32, color: [u8; 4]) {
        let sx = x * self.scale;
        let sy = y * self.scale;
        let sw = w * self.scale;
        let sh = h * self.scale;
        let r = (radius * self.scale).min(sw / 2).min(sh / 2) as f32;
        let thickness = self.scale as f32;
        let x_end = (sx + sw).min(self.width);
        let y_end = (sy + sh).min(self.height);

        for py in sy..y_end {
            for px in sx..x_end {
                let fx = (px - sx) as f32 + 0.5;
                let fy = (py - sy) as f32 + 0.5;
                let outer = rounded_rect_coverage(fx, fy, sw as f32, sh as f32, r);
                if outer <= 0.0 {
                    continue;
                }
                let inner = rounded_rect_coverage(
                    fx - thickness,
                    fy - thickness,
                    (sw as f32 - 2.0 * thickness).max(0.0),
                    (sh as f32 - 2.0 * thickness).max(0.0),
                    (r - thickness).max(0.0),
                );
                let ring = (outer - inner).clamp(0.0, 1.0);
                if ring > 0.0 {
                    let mut c = color;
                    c[3] = ((c[3] as f32) * ring).round() as u8;
                    self.blend_px(px, py, c);
                }
            }
        }
    }

    fn draw_rounded_thumbnail(&mut self, x: u32, y: u32, thumb_64: &[u8], radius: u32) {
        self.fill_rounded_rect(x, y, 64, 64, radius, [15, 23, 42, 255]);
        if thumb_64.len() >= 64 * 64 * 4 {
            let sx = x * self.scale;
            let sy = y * self.scale;
            let sw = 64 * self.scale;
            let sh = 64 * self.scale;
            let r = (radius * self.scale) as f32;
            for py in 0..sh {
                let ty = (py / self.scale).min(63);
                for px in 0..sw {
                    let cov = rounded_rect_coverage(
                        px as f32 + 0.5,
                        py as f32 + 0.5,
                        sw as f32,
                        sh as f32,
                        r,
                    );
                    if cov <= 0.0 {
                        continue;
                    }
                    let tx = (px / self.scale).min(63);
                    let sidx = ((ty * 64 + tx) * 4) as usize;
                    let a = ((thumb_64[sidx + 3] as f32) * cov).round() as u8;
                    if a > 0 {
                        self.blend_px(
                            sx + px,
                            sy + py,
                            [thumb_64[sidx], thumb_64[sidx + 1], thumb_64[sidx + 2], a],
                        );
                    }
                }
            }
        }
        self.stroke_rounded_rect(x, y, 64, 64, radius, [71, 85, 105, 220]);
    }

    fn draw_text(&mut self, x: u32, y: u32, text: &str, text_scale: u32, color: [u8; 4]) {
        let mut cursor_x = x;
        let ts = text_scale.max(1);
        let logical_w = self.width / self.scale;
        for ch in text.chars() {
            let glyph = glyph_5x7(ch);
            for (row_idx, &row_bits) in glyph.iter().enumerate() {
                for col_idx in 0..5u32 {
                    if (row_bits >> (4 - col_idx)) & 1 == 1 {
                        let gx = cursor_x + col_idx * ts;
                        let gy = y + (row_idx as u32) * ts;
                        self.fill_rect(gx, gy, ts, ts, color);
                    }
                }
            }
            cursor_x += 6 * ts;
            if cursor_x >= logical_w.saturating_sub(6) {
                break;
            }
        }
    }
}

fn lerp_color(a: [u8; 4], b: [u8; 4], t: f32) -> [u8; 4] {
    let t = t.clamp(0.0, 1.0);
    [
        (a[0] as f32 + (b[0] as f32 - a[0] as f32) * t).round() as u8,
        (a[1] as f32 + (b[1] as f32 - a[1] as f32) * t).round() as u8,
        (a[2] as f32 + (b[2] as f32 - a[2] as f32) * t).round() as u8,
        (a[3] as f32 + (b[3] as f32 - a[3] as f32) * t).round() as u8,
    ]
}

/// Signed-distance anti-aliased coverage (0.0..=1.0) for a rounded rectangle.
fn rounded_rect_coverage(px: f32, py: f32, w: f32, h: f32, r: f32) -> f32 {
    if w <= 0.0 || h <= 0.0 {
        return 0.0;
    }
    let hx = w * 0.5;
    let hy = h * 0.5;
    let cx = (px - hx).abs() - (hx - r);
    let cy = (py - hy).abs() - (hy - r);
    let dist = (cx.max(0.0).powi(2) + cy.max(0.0).powi(2)).sqrt() + cx.max(cy).min(0.0) - r;
    (0.5 - dist).clamp(0.0, 1.0)
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
                        let layer: *mut AnyObject = msg_send![ns_view, layer];
                        if !layer.is_null() {
                            let _: () = msg_send![layer, setCornerRadius: 14.0f64];
                            let _: () = msg_send![layer, setMasksToBounds: true];
                        }
                        let ns_win: *mut AnyObject = msg_send![ns_view, window];
                        if !ns_win.is_null() {
                            // NSStatusWindowLevel (25) so it floats above normal windows
                            let _: () = msg_send![ns_win, setLevel: 25isize];
                            let _: () = msg_send![ns_win, setOpaque: false];
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

fn set_window_alpha(window: &Window, alpha: f64) {
    #[cfg(target_os = "macos")]
    {
        if let Ok(handle) = window.window_handle() {
            if let RawWindowHandle::AppKit(appkit) = handle.as_raw() {
                unsafe {
                    use objc2::msg_send;
                    use objc2::runtime::AnyObject;
                    let ns_view = appkit.ns_view.as_ptr() as *mut AnyObject;
                    if !ns_view.is_null() {
                        let ns_win: *mut AnyObject = msg_send![ns_view, window];
                        if !ns_win.is_null() {
                            let _: () = msg_send![ns_win, setAlphaValue: alpha.clamp(0.0, 1.0)];
                        }
                    }
                }
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (window, alpha);
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

            // Convert RGBA to BGRA top-down DIB (composite transparent corners onto dark slate)
            let mut bgra = Vec::with_capacity(canvas.rgba.len());
            #[allow(clippy::chunks_exact_to_as_chunks)]
            for px in canvas.rgba.chunks_exact(4) {
                let a = px[3] as u32;
                let r = (px[0] as u32 * a + 13 * (255 - a)) / 255;
                let g = (px[1] as u32 * a + 18 * (255 - a)) / 255;
                let b = (px[2] as u32 * a + 28 * (255 - a)) / 255;
                bgra.extend_from_slice(&[b as u8, g as u8, r as u8, 255]);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hud_hit_testing_and_easing() {
        assert!(is_in_revert_btn(300.0, 20.0));
        assert!(!is_in_revert_btn(200.0, 20.0));
        assert!(is_in_close_btn(366.0, 20.0));
        assert!(!is_in_close_btn(300.0, 20.0));

        assert_eq!(ease_out_cubic(0.0), 0.0);
        assert_eq!(ease_out_cubic(1.0), 1.0);
        assert!(ease_out_cubic(0.5) > 0.8);

        assert_eq!(format_elapsed(Duration::from_millis(42)), "42MS");
        assert_eq!(format_elapsed(Duration::from_millis(1500)), "1.5S");
    }

    #[test]
    fn test_hud_supersampled_canvas_rounded_rect() {
        let mut canvas = Canvas::new(HUD_WIDTH, HUD_HEIGHT, HUD_SCALE);
        assert_eq!(canvas.width, HUD_WIDTH * HUD_SCALE);
        assert_eq!(canvas.height, HUD_HEIGHT * HUD_SCALE);

        canvas.fill_rounded_gradient(
            0,
            0,
            HUD_WIDTH,
            HUD_HEIGHT,
            14,
            [20, 27, 40, 246],
            [13, 18, 28, 250],
        );

        // Top-left corner pixel (0, 0) should be fully transparent outside the 14px radius
        assert_eq!(canvas.rgba[3], 0);
        // Center pixel should be opaque card fill
        let center_idx = (((canvas.height / 2) * canvas.width + (canvas.width / 2)) * 4) as usize;
        assert!(canvas.rgba[center_idx + 3] >= 245);
    }
}
