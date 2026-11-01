//! The notch shell: a squircle that springs between the collapsed pill and
//! the expanded panel, painted as glass, hosting the pill badges and the
//! panel content.
//!
//! The OS window spans the whole width of the monitor's work area, so where
//! the notch sits (left corner, center, right corner) only moves the shape
//! inside the window, and switching slots springs it across; the window
//! itself never moves. A notch in a corner grows away from its edge.

use crate::anim::{self, ease_out_cubic};
use crate::geometry::{CORNER_EXPONENT, Squircle, lerp};
use crate::paint;
use crate::shell_state::{ShellState, ShellTimings};
use crate::spring::SpringParams;
use crate::theme::Theme;
use egui::emath::TSTransform;
use egui::{Id, Pos2, Rect, Sense, Ui, Vec2, pos2, vec2};
use serde::{Deserialize, Serialize};

/// Where the notch rests along the top edge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Placement {
    /// Top-left corner; the panel opens to the right.
    Left,
    /// Top center; the panel opens both ways.
    #[default]
    Center,
    /// Top-right corner; the panel opens to the left.
    Right,
}

impl Placement {
    pub const ALL: [Placement; 3] = [Placement::Left, Placement::Center, Placement::Right];

    /// Horizontal anchor: 0 = against the left edge, 1 = against the right.
    pub fn frac(self) -> f32 {
        match self {
            Placement::Left => 0.0,
            Placement::Center => 0.5,
            Placement::Right => 1.0,
        }
    }
}

/// Which screen edge the notch tucks itself into while idle ("hide to
/// side"). It shows only a thin tab there and slides out when opened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dock {
    #[default]
    Off,
    Left,
    Right,
}

impl Dock {
    pub fn is_on(self) -> bool {
        self != Dock::Off
    }
}

/// What opens the panel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OpenOn {
    /// Resting the pointer on the pill (the original behavior).
    #[default]
    Hover,
    /// Clicking the pill.
    Click,
}

/// Size of the tab a docked notch shrinks to while idle.
pub const DOCK_TAB: Vec2 = vec2(16.0, 64.0);
/// How much of the docked tab sticks out of the screen edge.
const DOCK_PEEK: f32 = 12.0;

/// Where the shape sits this frame, as fractions of the room it has.
#[derive(Clone, Copy, Debug)]
struct Pose {
    /// Horizontal anchor: 0 = left edge, 1 = right edge.
    fx: f32,
    /// Vertical anchor: 0 = top edge, 1 = bottom edge.
    fy: f32,
    /// 0 = fully on screen, 1 = tucked into the dock edge.
    hide: f32,
    /// Edge the shape tucks into.
    side: Dock,
    /// Height of the closed shape (the room above the bottom edge).
    rest_h: f32,
}

impl Pose {
    fn top(fx: f32, rest_h: f32) -> Self {
        Self {
            fx,
            fy: 0.0,
            hide: 0.0,
            side: Dock::Off,
            rest_h,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ShellMetrics {
    pub collapsed: Vec2,
    /// Extra size while the pointer rests on the closed pill.
    pub hover_swell: Vec2,
    pub expanded: Vec2,
    pub collapsed_radius: f32,
    pub expanded_radius: f32,
    /// Gap between the top of the monitor and the shell.
    pub top_margin: f32,
    /// Gap between the side of the monitor and a notch in a corner.
    pub side_margin: f32,
}

impl Default for ShellMetrics {
    fn default() -> Self {
        Self {
            collapsed: vec2(140.0, 34.0),
            hover_swell: vec2(12.0, 2.0),
            expanded: vec2(568.0, 236.0),
            collapsed_radius: 17.0,
            expanded_radius: 32.0,
            top_margin: 6.0,
            side_margin: 6.0,
        }
    }
}

impl ShellMetrics {
    /// Smallest OS window that fits the expanded panel plus spring
    /// overshoot. The app widens it to the monitor's work area; the height
    /// is what matters.
    pub fn window_size(&self) -> Vec2 {
        let overshoot = 1.08;
        let w = self.collapsed.x + (self.expanded.x - self.collapsed.x) * overshoot;
        let h = self.collapsed.y + (self.expanded.y - self.collapsed.y) * overshoot;
        vec2((w + 16.0).ceil(), (h + self.top_margin + 16.0).ceil())
    }
}

/// What the content callback gets each frame.
pub struct ContentCtx {
    /// The panel rect at its final expanded size (layout uses this so tiles
    /// do not reflow while the shell is still springing).
    pub panel: Rect,
    /// Global content opacity (fade in/out).
    pub alpha: f32,
    /// Seconds since the stagger reveal began (`None` before it starts).
    pub reveal: Option<f32>,
    /// True while content is going away (no stagger, just fade).
    pub closing: bool,
    /// Content scale transform (0.96 -> 1.0 about the panel center).
    pub transform: TSTransform,
    /// The shell's current rect: content outside it is not on glass yet
    /// (a tall page still springing open).
    pub shape: Rect,
}

impl ContentCtx {
    /// Stagger alpha for the `index`-th element: 20 ms apart, 180 ms each.
    pub fn stagger(&self, index: usize) -> f32 {
        if self.closing {
            return 1.0;
        }
        match self.reveal {
            Some(t) => ease_out_cubic((t - index as f32 * 0.020) / 0.180),
            None => 0.0,
        }
    }
}

pub struct ShellOutput {
    /// Current shape in window coordinates (points). Drive hit-testing with it.
    pub shape: Squircle,
    /// 0 = pill, 1 = panel (may overshoot slightly).
    pub progress: f32,
    /// False when fully hidden (fullscreen app in front).
    pub visible: bool,
    /// Whole-shell opacity (fullscreen fade).
    pub opacity: f32,
    /// The user just dropped the notch after dragging it: `Shell::free` (or
    /// `Shell::dock_y` while docked) holds the new position to save.
    pub moved: bool,
}

pub struct Shell {
    pub metrics: ShellMetrics,
    pub timings: ShellTimings,
    pub state: ShellState,
    /// Resting slot along the top edge.
    pub placement: Placement,
    /// Tuck into a side edge while idle.
    pub dock: Dock,
    /// Vertical position along the dock edge, 0 = top, 1 = bottom.
    pub dock_y: f32,
    /// Where the user dragged the notch to (`x`, `y` as fractions of the
    /// room), overriding `placement`. `None` keeps it on the top edge.
    pub free: Option<(f32, f32)>,
    /// Last edge the notch was docked to, so it slides back out the way it
    /// went in while the dock is being switched off.
    dock_side: Dock,
    /// The notch is still springing back to the top edge, so the OS window
    /// must stay tall until it gets there.
    settling: bool,
    /// Treat the shell as hovered whatever the pointer does (typing in it,
    /// files being dragged over it).
    pub hold_open: bool,
    /// Expanded size wanted by the page on show (phone link), instead of
    /// `metrics.expanded`.
    pub expanded_override: Option<Vec2>,
    /// Height the OS window must keep: it grows as soon as a taller page
    /// opens, and shrinks only once the shell has settled back down.
    tall: f32,
    id: Id,
    outline: Vec<Pos2>,
    last_shape: Option<Squircle>,
    reveal_start: Option<f64>,
}

impl Shell {
    pub fn new(metrics: ShellMetrics, timings: ShellTimings) -> Self {
        Self {
            metrics,
            timings,
            state: ShellState::default(),
            placement: Placement::default(),
            dock: Dock::Off,
            dock_y: 0.35,
            free: None,
            dock_side: Dock::Left,
            settling: false,
            hold_open: false,
            expanded_override: None,
            tall: metrics.expanded.y,
            id: Id::new("nook-shell"),
            outline: Vec::new(),
            last_shape: None,
            reveal_start: None,
        }
    }

    /// The expanded size currently wanted.
    pub fn expanded(&self) -> Vec2 {
        self.expanded_override.unwrap_or(self.metrics.expanded)
    }

    /// OS window size that fits the shell now, including a taller page that
    /// is still springing open or closed.
    pub fn window_size(&self) -> Vec2 {
        let m = ShellMetrics {
            expanded: vec2(self.expanded().x, self.tall.max(self.expanded().y)),
            ..self.metrics
        };
        m.window_size()
    }

    /// Whether the OS window must cover the whole work area. At the top edge
    /// a strip is enough; anywhere else the shape needs room to move.
    pub fn needs_full_window(&self) -> bool {
        self.dock.is_on() || self.free.is_some() || self.settling
    }

    /// Size of the closed shape: the pill, or the tab of a docked notch.
    fn base_size(&self) -> Vec2 {
        if self.dock.is_on() {
            DOCK_TAB
        } else {
            self.metrics.collapsed
        }
    }

    /// Where the pose is headed: horizontal and vertical anchor fractions.
    fn target_anchor(&self) -> (f32, f32) {
        match self.dock {
            Dock::Left => (0.0, self.dock_y),
            Dock::Right => (1.0, self.dock_y),
            Dock::Off => self.free.unwrap_or((self.placement.frac(), 0.0)),
        }
    }

    /// Range of left edges for a shell of `width`.
    fn x_range(&self, window: Rect, width: f32) -> (f32, f32) {
        let lo = window.min.x + self.metrics.side_margin;
        let hi = (window.max.x - self.metrics.side_margin - width).max(lo);
        (lo, hi)
    }

    /// Range of top edges for a closed shell of height `rest_h`.
    fn y_range(&self, window: Rect, rest_h: f32) -> (f32, f32) {
        let lo = window.min.y + self.metrics.top_margin;
        let hi = (window.max.y - self.metrics.top_margin - rest_h).max(lo);
        (lo, hi)
    }

    #[cfg(test)]
    fn shape_for(&self, window: Rect, size: Vec2, progress: f32, frac: f32) -> Squircle {
        self.shape_at(window, size, progress, Pose::top(frac, self.metrics.collapsed.y))
    }

    fn shape_at(&self, window: Rect, size: Vec2, progress: f32, pose: Pose) -> Squircle {
        let m = &self.metrics;
        let (lo_x, hi_x) = self.x_range(window, size.x);
        let mut left = lerp(lo_x, hi_x, pose.fx);
        if pose.hide > 0.0 {
            // Tucked: only DOCK_PEEK of the shape stays on screen.
            let tucked = match pose.side {
                Dock::Left => window.min.x - size.x + DOCK_PEEK,
                Dock::Right => window.max.x - DOCK_PEEK,
                Dock::Off => left,
            };
            left = lerp(left, tucked, pose.hide);
        }
        let (lo_y, hi_y) = self.y_range(window, pose.rest_h);
        let mut top = lerp(lo_y, hi_y, pose.fy);
        if pose.fy > 0.0 {
            // A panel opened low on the screen grows upward instead of off it.
            top = top.min(window.max.y - m.top_margin - size.y);
        }
        let top = top.max(window.min.y);
        let radius = lerp(
            m.collapsed_radius,
            m.expanded_radius,
            progress.clamp(0.0, 1.0),
        );
        // Corners morph from a circular capsule (pill) to a squircle (panel).
        let exponent = lerp(2.0, CORNER_EXPONENT, progress.clamp(0.0, 1.0));
        Squircle::new(
            Rect::from_min_size(pos2(left, top), size),
            radius.min(size.x.min(size.y) * 0.5),
        )
        .with_exponent(exponent)
    }

    /// Runs one frame. `hidden` fades the whole shell out (fullscreen apps).
    /// `composited` means the compositor paints the glass tint, so only
    /// grain, rim and highlight are drawn here.
    pub fn show(
        &mut self,
        ui: &mut Ui,
        theme: &Theme,
        hidden: bool,
        composited: bool,
        pill: impl FnOnce(&mut Ui, Rect, f32),
        content: impl FnOnce(&mut Ui, &ContentCtx),
    ) -> ShellOutput {
        let ctx = ui.ctx().clone();
        let window = ctx.content_rect();
        let now = ctx.input(|i| i.time);
        let m = self.metrics;

        let hovered = !hidden
            && (self.hold_open
                || match (self.last_shape, ctx.input(|i| i.pointer.hover_pos())) {
                    (Some(s), Some(p)) => s.contains(p),
                    _ => false,
                });

        let base = self.base_size();
        if self.dock.is_on() {
            self.dock_side = self.dock;
        }

        // Click on the glass (not on a tile) pins / unpins; dragging it
        // moves the notch.
        let mut dragging = false;
        let mut dropped = false;
        let mut drag = Vec2::ZERO;
        if let Some(shape) = self.last_shape
            && !hidden
        {
            let resp = ui.interact(shape.rect, self.id.with("bg"), Sense::click_and_drag());
            if resp.clicked() {
                self.state.toggle_pin(now);
            }
            dragging = resp.dragged();
            dropped = resp.drag_stopped();
            drag = resp.drag_delta();
        }
        if dragging {
            ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
            if self.free.is_none() && !self.dock.is_on() && !self.settling {
                // First move of a notch still on the top edge: the window
                // grows to the whole screen first, then follows the pointer.
                self.free = Some((self.placement.frac(), 0.0));
            } else if let Some(cur) = self.last_shape.map(|s| s.rect) {
                let (lo_x, hi_x) = self.x_range(window, cur.width());
                let (lo_y, hi_y) = self.y_range(window, base.y);
                let fx = if hi_x > lo_x {
                    ((cur.min.x + drag.x - lo_x) / (hi_x - lo_x)).clamp(0.0, 1.0)
                } else {
                    0.5
                };
                let fy = if hi_y > lo_y {
                    ((cur.min.y + drag.y - lo_y) / (hi_y - lo_y)).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                if self.dock.is_on() {
                    // Docked: slide along the edge only.
                    self.dock_y = fy;
                } else {
                    self.free = Some((fx, fy));
                }
            }
        }

        let targets = self.state.update(now, hovered, &self.timings);
        if let Some(at) = targets.wake_at {
            ctx.request_repaint_after_secs(((at - now).max(0.0)) as f32 + 0.001);
        }

        let open = targets.shell_open && !hidden;
        let expanded = self.expanded();
        let rest = if self.state.is_hovering_closed() && !self.dock.is_on() {
            base + m.hover_swell
        } else {
            base
        };
        let shell_params = SpringParams::shell();
        let w = anim::spring_from(
            &ctx,
            self.id.with("w"),
            base.x,
            if open { expanded.x } else { rest.x },
            shell_params,
        );
        let h = anim::spring_from(
            &ctx,
            self.id.with("h"),
            base.y,
            if open { expanded.y } else { rest.y },
            SpringParams::with_overshoot(220.0, 0.04),
        );
        let opacity = anim::spring_from(
            &ctx,
            self.id.with("opacity"),
            0.0,
            if hidden { 0.0 } else { 1.0 },
            SpringParams::critical(180.0),
        )
        .clamp(0.0, 1.0);

        // Progress is measured against the regular panel, so a taller page
        // reads as fully open (1.0) instead of re-running the morph.
        let mut progress = ((h - base.y) / (m.expanded.y - base.y)).max(0.0);
        if expanded.y > m.expanded.y {
            progress = progress.min(1.0);
        }
        // Grow the window at once; shrink it once the shape has come down.
        if expanded.y >= self.tall || h <= expanded.y.max(m.expanded.y) + 0.5 {
            self.tall = expanded.y.max(m.expanded.y);
        }

        // Anchor fractions; a new placement springs across the screen, and
        // while dragging the shape sticks to the pointer.
        let (tx, ty) = self.target_anchor();
        let pos = anim::spring_vec2(&ctx, self.id.with("pos"), vec2(tx, ty), dragging);
        let settling = self.free.is_none() && !self.dock.is_on() && pos.y.abs() > 0.002;
        if self.settling && !settling {
            // One more frame so the window can shrink back to its strip.
            ctx.request_repaint();
        }
        self.settling = settling;
        // Slide into the dock edge while idle, out again when opened.
        let hide = anim::spring(
            &ctx,
            self.id.with("hide"),
            if self.dock.is_on() && !open { 1.0 } else { 0.0 },
            SpringParams::critical(240.0),
        )
        .clamp(0.0, 1.0);
        let pose = Pose {
            fx: pos.x,
            fy: pos.y.max(0.0),
            hide,
            side: self.dock_side,
            rest_h: base.y,
        };
        let frac = pos.x;
        let shape = self.shape_at(window, vec2(w, h), progress, pose);
        shape.outline_into(&mut self.outline);
        self.last_shape = Some(shape);

        let visible = opacity > 0.002;
        if !visible {
            return ShellOutput {
                shape,
                progress,
                visible,
                opacity,
                moved: dropped,
            };
        }

        let painter = ui.painter().clone();
        paint::glass(
            &painter,
            &shape,
            &self.outline,
            theme,
            opacity,
            progress,
            !composited,
        );

        // Collapsed badges fade out as the shell grows.
        let pill_alpha = if self.dock.is_on() {
            0.0
        } else {
            (1.0 - progress / 0.35).clamp(0.0, 1.0) * opacity
        };
        if pill_alpha > 0.01 {
            let mut pill_ui = ui.new_child(egui::UiBuilder::new().max_rect(shape.rect));
            pill_ui.set_clip_rect(shape.rect);
            pill_ui.multiply_opacity(pill_alpha);
            pill(&mut pill_ui, shape.rect, pill_alpha);
        }

        // A docked notch shows a small grip on its tab.
        if self.dock.is_on() {
            let fade = (1.0 - progress / 0.25).clamp(0.0, 1.0) * opacity;
            if fade > 0.01 {
                let visible_part = shape.rect.intersect(window);
                painter.rect_filled(
                    Rect::from_center_size(
                        pos2(visible_part.center().x, shape.rect.center().y),
                        vec2(3.0, 22.0),
                    ),
                    1.5,
                    crate::theme::with_alpha(theme.text_faint, fade),
                );
            }
        }

        // Content: fade (100 ms) + stagger once the shell is ~60% open.
        if targets.content_open && progress >= 0.6 && self.reveal_start.is_none() {
            self.reveal_start = Some(now);
        }
        if !targets.content_open {
            self.reveal_start = None;
        }
        let fade = anim::tween(&ctx, self.id.with("content"), targets.content_open, 0.10);
        let reveal = self.reveal_start.map(|t| (now - t) as f32);
        if let Some(t) = reveal
            && t < 0.6
        {
            ctx.request_repaint();
        }
        let content_alpha = fade * opacity;
        if content_alpha > 0.001 && (reveal.is_some() || !targets.content_open) {
            let open_pose = Pose { hide: 0.0, ..pose };
            let panel = self.shape_at(window, expanded, 1.0, open_pose).rect;
            let scale = lerp(0.96, 1.0, ease_out_cubic(progress.clamp(0.0, 1.0)));
            // Grow out of the anchored side: the top center for a centered
            // notch, the top outer corner for one in a corner.
            let c = vec2(lerp(panel.min.x, panel.max.x, frac), panel.min.y);
            let transform = TSTransform::new(c * (1.0 - scale), scale);
            let cctx = ContentCtx {
                panel,
                alpha: content_alpha,
                reveal,
                closing: !targets.content_open,
                transform,
                shape: shape.rect,
            };
            content(ui, &cctx);
        }

        if self.state.is_pinned() && progress > 0.85 {
            paint::icon(
                &painter,
                pos2(shape.rect.center().x, shape.rect.min.y + 8.0),
                egui_phosphor::fill::PUSH_PIN,
                9.0,
                crate::theme::with_alpha(theme.text_faint, content_alpha),
                true,
            );
        }

        ShellOutput {
            shape,
            progress,
            visible,
            opacity,
            moved: dropped,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corner_shells_hug_their_edge() {
        let shell = Shell::new(ShellMetrics::default(), ShellTimings::default());
        let window = Rect::from_min_size(Pos2::ZERO, vec2(1536.0, 300.0));
        let m = shell.metrics;
        let left = shell.shape_for(window, m.expanded, 1.0, 0.0).rect;
        let right = shell.shape_for(window, m.expanded, 1.0, 1.0).rect;
        let center = shell.shape_for(window, m.expanded, 1.0, 0.5).rect;
        assert_eq!(left.min.x, m.side_margin);
        assert_eq!(right.max.x, window.max.x - m.side_margin);
        assert!((center.center().x - window.center().x).abs() < 1e-3);
        // A pill in the left corner keeps its left edge as it opens.
        let pill = shell.shape_for(window, m.collapsed, 0.0, 0.0).rect;
        assert_eq!(pill.min.x, left.min.x);
        // ...and one in the right corner keeps its right edge.
        let pill = shell.shape_for(window, m.collapsed, 0.0, 1.0).rect;
        assert_eq!(pill.max.x, right.max.x);
    }
}
