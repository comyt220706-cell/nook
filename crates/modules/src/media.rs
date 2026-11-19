//! Media: now playing from any app that reports to Windows (Spotify,
//! browsers, the Media Player...) through SMTC, with album art, progress and
//! previous / play-pause / next. A note glyph shows in the pill while
//! something plays.
//!
//! The SMTC work runs on `nook_platform::media`'s worker thread; this module
//! only drains its snapshots in `on_tick` and sends commands back.

use egui::text::{LayoutJob, TextFormat, TextWrapping};
use crate::lyrics::{Lyrics, LyricsLoader, Status, TrackKey, current_line};
use egui::{
    Align, Align2, Color32, ColorImage, Layout, Painter, Pos2, Rect, Response, RichText,
    ScrollArea, Sense, TextureHandle, TextureOptions, Ui, UiBuilder, pos2, vec2,
};
use nook_platform::media::{MediaCommand, MediaWatcher, NowPlaying};
use nook_ui::module::{Module, TileCtx, TileSize};
use nook_ui::theme::{SIZE_LABEL, Theme, with_alpha};
use nook_ui::{icons, paint, widgets};
use std::sync::Arc;
use std::time::Duration;

const ART: f32 = 44.0;
/// Album art on the page (the art itself is decoded at 96 px).
const ART_PAGE: f32 = 96.0;
/// Size of the full-panel Music page.
const PAGE: egui::Vec2 = vec2(720.0, 340.0);
/// Width of the details column on the page.
const DETAILS_W: f32 = 250.0;

#[derive(Default)]
pub struct Media {
    watcher: Option<MediaWatcher>,
    now: Option<NowPlaying>,
    /// Texture for `now.art`, keyed by the art's allocation.
    art: Option<(usize, TextureHandle)>,
    lyrics: LyricsLoader,
    /// The tile was clicked: open the Music page.
    open_page: bool,
}

impl Media {
    /// Previous / play-pause / next, centered on (`cx`, `y`).
    fn transport(&mut self, ui: &mut Ui, now: &NowPlaying, cx: f32, y: f32, theme: &Theme) {
        let gap = 34.0;
        if now.can_previous
            && button_at(ui, pos2(cx - gap, y), 24.0, icons::SKIP_BACK, false, theme)
                .on_hover_text("Previous")
                .clicked()
        {
            self.send(MediaCommand::Previous);
        }
        let (glyph, tip) = if now.playing {
            (icons::PAUSE, "Pause")
        } else {
            (icons::PLAY, "Play")
        };
        if now.can_toggle
            && button_at(ui, pos2(cx, y), 28.0, glyph, true, theme)
                .on_hover_text(tip)
                .clicked()
        {
            self.send(MediaCommand::PlayPause);
            // Flip right away; the next snapshot confirms it.
            if let Some(n) = &mut self.now {
                if let Some((pos, dur)) = n.position() {
                    n.timeline = Some((pos, dur));
                    n.captured = std::time::Instant::now();
                }
                n.playing = !n.playing;
            }
        }
        if now.can_next
            && button_at(
                ui,
                pos2(cx + gap, y),
                24.0,
                icons::SKIP_FORWARD,
                false,
                theme,
            )
            .on_hover_text("Next")
            .clicked()
        {
            self.send(MediaCommand::Next);
        }
    }

    /// Album art (or a placeholder) in `rect`.
    fn art_at(&mut self, ui: &Ui, rect: Rect, radius: f32, icon_size: f32, theme: &Theme) {
        match self.art_texture(ui.ctx()) {
            Some(tex) => rounded_image(ui, rect, tex, radius),
            None => {
                widgets::card(ui.painter(), rect, theme);
                paint::icon(
                    ui.painter(),
                    rect.center(),
                    icons::MUSIC_NOTES,
                    icon_size,
                    theme.text_faint,
                    false,
                );
            }
        }
    }

    /// Lyrics, synced or plain, or why there are none.
    fn lyrics_view(&mut self, ui: &mut Ui, rect: Rect, now: &NowPlaying, theme: &Theme) {
        let painter = ui.painter().with_clip_rect(rect);
        let message = |text: &str| {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                text,
                Theme::font(SIZE_LABEL + 1.0),
                theme.text_dim,
            );
        };
        let mut retry = false;
        match self.lyrics.status() {
            Status::Idle | Status::Loading => message("Looking for lyrics…"),
            Status::Failed => {
                message("Couldn't reach the lyrics service");
                let link = Rect::from_center_size(rect.center() + vec2(0.0, 24.0), vec2(120.0, 22.0));
                let resp = ui
                    .interact(link, ui.id().with("lyrics-retry"), Sense::click())
                    .on_hover_cursor(egui::CursorIcon::PointingHand);
                painter.text(
                    link.center(),
                    Align2::CENTER_CENTER,
                    "Try again",
                    Theme::font(SIZE_LABEL),
                    theme.accent_fg(),
                );
                retry = resp.clicked();
            }
            Status::Ready(Lyrics::NotFound) => message("No lyrics found for this track"),
            Status::Ready(Lyrics::Instrumental) => message("Instrumental"),
            Status::Ready(Lyrics::Plain(lines)) => {
                let mut child = ui.new_child(
                    UiBuilder::new()
                        .max_rect(rect)
                        .layout(Layout::top_down(Align::LEFT)),
                );
                ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(&mut child, |ui| {
                        for line in lines {
                            ui.label(
                                RichText::new(line)
                                    .font(Theme::font(SIZE_LABEL + 2.0))
                                    .color(theme.text_dim),
                            );
                        }
                        ui.add_space(20.0);
                    });
            }
            Status::Ready(Lyrics::Synced(lines)) => {
                let position = now.position().map_or(0.0, |(p, _)| p);
                let current = current_line(lines, position);
                // Smooth scroll: the line being sung glides to the middle.
                let target = current.map_or(-1.0, |i| i as f32);
                let offset = ui
                    .ctx()
                    .animate_value_with_time(ui.id().with("lyrics-scroll"), target, 0.3);
                let pitch = 30.0;
                for (i, (_, text)) in lines.iter().enumerate() {
                    let d = i as f32 - offset;
                    let y = rect.center().y + d * pitch;
                    if y < rect.min.y - pitch || y > rect.max.y + pitch || text.is_empty() {
                        continue;
                    }
                    let (font, color) = if Some(i) == current {
                        (Theme::font_semibold(SIZE_LABEL + 6.0), theme.text)
                    } else {
                        let fade = (1.0 - d.abs() * 0.25).clamp(0.15, 1.0);
                        (Theme::font(SIZE_LABEL + 2.0), with_alpha(theme.text_dim, fade))
                    };
                    let h = painter.layout_no_wrap(text.clone(), font.clone(), color).size().y;
                    text_block(
                        &painter,
                        pos2(rect.min.x + 2.0, y - h * 0.5),
                        text,
                        rect.width() - 4.0,
                        1,
                        font,
                        color,
                    );
                }
                if now.playing {
                    ui.ctx().request_repaint_after(Duration::from_millis(250));
                }
            }
        }
        painter.text(
            pos2(rect.max.x, rect.max.y),
            Align2::RIGHT_BOTTOM,
            "Lyrics from LRCLIB",
            Theme::font(SIZE_LABEL - 2.0),
            theme.text_faint,
        );
        if retry {
            self.lyrics.retry();
        }
    }

    fn send(&self, cmd: MediaCommand) {
        if let Some(w) = &self.watcher {
            w.send(cmd);
        }
    }

    fn art_texture(&mut self, ctx: &egui::Context) -> Option<egui::TextureId> {
        let art = self.now.as_ref()?.art.as_ref()?;
        let key = Arc::as_ptr(art) as usize;
        if self.art.as_ref().map(|(k, _)| *k) != Some(key) {
            let image =
                ColorImage::from_rgba_unmultiplied([art.width, art.height], art.rgba.as_slice());
            let tex = ctx.load_texture("nook-media-art", image, TextureOptions::LINEAR);
            self.art = Some((key, tex));
        }
        self.art.as_ref().map(|(_, t)| t.id())
    }
}

/// Text of at most `rows` rows, elided with "…" to `width`. Returns its height.
fn text_block(
    painter: &Painter,
    pos: Pos2,
    text: &str,
    width: f32,
    rows: usize,
    font: egui::FontId,
    color: Color32,
) -> f32 {
    let mut job = LayoutJob::single_section(text.to_owned(), TextFormat::simple(font, color));
    job.wrap = TextWrapping {
        max_width: width,
        max_rows: rows,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    let galley = painter.layout_job(job);
    let height = galley.size().y;
    painter.galley(pos, galley, color);
    height
}

/// Single-line text, elided with "…" to `width`.
fn text_line(ui: &Ui, pos: Pos2, text: &str, width: f32, font: egui::FontId, color: Color32) {
    text_block(ui.painter(), pos, text, width, 1, font, color);
}

fn button_at(
    ui: &mut Ui,
    center: Pos2,
    size: f32,
    glyph: &'static str,
    active: bool,
    theme: &Theme,
) -> Response {
    let rect = Rect::from_center_size(center, vec2(size, size));
    let mut child = ui.new_child(UiBuilder::new().max_rect(rect).id_salt(glyph));
    widgets::icon_button(&mut child, glyph, size, active, theme)
}

fn mmss(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

/// Rounded image: a squircle-ish clip is overkill at 44 px, a rounded rect
/// mesh reads the same.
fn rounded_image(ui: &Ui, rect: Rect, tex: egui::TextureId, radius: f32) {
    ui.painter().add(
        egui::epaint::RectShape::filled(rect, radius, Color32::WHITE)
            .with_texture(tex, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0))),
    );
}

impl Module for Media {
    fn id(&self) -> &'static str {
        "media"
    }

    fn name(&self) -> &str {
        "Media"
    }

    fn icon(&self) -> &'static str {
        icons::MUSIC_NOTES
    }

    fn size_hint(&self) -> TileSize {
        TileSize::Large
    }

    fn is_active(&self) -> bool {
        self.now.as_ref().is_some_and(|n| n.playing)
    }

    fn attach(&mut self, ctx: &egui::Context) {
        let ctx = ctx.clone();
        match MediaWatcher::spawn(move || ctx.request_repaint()) {
            Ok(w) => self.watcher = Some(w),
            Err(e) => tracing::warn!("media: {e}"),
        }
    }

    fn on_tick(&mut self) {
        if let Some(snapshot) = self.watcher.as_ref().and_then(MediaWatcher::poll) {
            self.now = snapshot;
            if self.now.as_ref().and_then(|n| n.art.as_ref()).is_none() {
                self.art = None;
            }
        }
    }

    fn next_repaint(&self, tile_visible: bool) -> Option<Duration> {
        // The progress bar and time advance once a second while visible.
        let n = self.now.as_ref()?;
        (tile_visible && n.playing && n.timeline.is_some()).then(|| Duration::from_secs(1))
    }

    fn ui_tile(&mut self, ui: &mut Ui, ctx: &mut TileCtx<'_>) {
        let theme = ctx.theme;
        let rect = ui.max_rect();

        let Some(now) = self.now.clone() else {
            let c = rect.center();
            paint::icon(
                ui.painter(),
                c - vec2(0.0, 8.0),
                icons::MUSIC_NOTES,
                24.0,
                theme.text_faint,
                false,
            );
            ui.painter().text(
                c + vec2(0.0, 18.0),
                Align2::CENTER_CENTER,
                "Nothing playing",
                Theme::font(SIZE_LABEL),
                theme.text_dim,
            );
            return;
        };

        // Clicking the tile (outside the buttons) opens the Music page.
        let open = ui
            .interact(rect.expand(6.0), ui.id().with("media-open"), Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if open.clicked() {
            self.open_page = true;
        }

        // Album art, or a placeholder.
        let art_rect = Rect::from_min_size(rect.min, vec2(ART, ART));
        self.art_at(ui, art_rect, 10.0, 20.0, theme);

        // Right of the art: source app, time, progress bar.
        let col = rect.min.x + ART + 8.0;
        let col_w = rect.max.x - col;
        text_line(
            ui,
            pos2(col, rect.min.y + 2.0),
            &now.source,
            col_w,
            Theme::font(SIZE_LABEL - 1.0),
            theme.text_faint,
        );
        if let Some((pos, dur)) = now.position() {
            ui.painter().text(
                pos2(col, rect.min.y + 24.0),
                Align2::LEFT_CENTER,
                format!("{} / {}", mmss(pos), mmss(dur)),
                Theme::font(SIZE_LABEL - 1.0),
                theme.text_dim,
            );
            let bar = Rect::from_min_size(pos2(col, rect.min.y + 36.0), vec2(col_w, 3.0));
            ui.painter()
                .rect_filled(bar, 1.5, with_alpha(theme.text_faint, 0.3));
            let frac = (pos / dur.max(1.0)).clamp(0.0, 1.0) as f32;
            let mut fill = bar;
            fill.set_width(bar.width() * frac);
            ui.painter().rect_filled(fill, 1.5, theme.accent_fg());
        } else {
            ui.painter().text(
                pos2(col, rect.min.y + 24.0),
                Align2::LEFT_CENTER,
                if now.playing { "Playing" } else { "Paused" },
                Theme::font(SIZE_LABEL - 1.0),
                theme.text_dim,
            );
        }

        // Title and artist.
        let title = if now.title.is_empty() {
            "Unknown title"
        } else {
            &now.title
        };
        text_line(
            ui,
            pos2(rect.min.x, rect.min.y + ART + 6.0),
            title,
            rect.width(),
            Theme::font_semibold(SIZE_LABEL + 1.0),
            theme.text,
        );
        text_line(
            ui,
            pos2(rect.min.x, rect.min.y + ART + 21.0),
            &now.artist,
            rect.width(),
            Theme::font(SIZE_LABEL),
            theme.text_dim,
        );

        // Transport.
        self.transport(ui, &now, rect.center().x, rect.max.y - 11.0, theme);
    }

    fn take_page_request(&mut self) -> bool {
        std::mem::take(&mut self.open_page)
    }

    fn page_size(&self) -> Option<egui::Vec2> {
        Some(PAGE)
    }

    fn ui_page(&mut self, ui: &mut Ui, _header: &mut Ui, body: Rect, theme: &Theme) -> bool {
        let Some(now) = self.now.clone() else {
            ui.painter().text(
                body.center(),
                Align2::CENTER_CENTER,
                "Nothing playing",
                Theme::font(SIZE_LABEL + 1.0),
                theme.text_dim,
            );
            return false;
        };
        let ctx = ui.ctx().clone();
        let key = (!now.title.is_empty()).then(|| TrackKey {
            title: now.title.clone(),
            artist: now.artist.clone(),
            album: now.album.clone(),
            duration: now.timeline.map_or(0, |(_, d)| d.round() as u32),
        });
        self.lyrics.update(key, &ctx);

        // Left column: art, names, details, progress, buttons.
        let x = body.min.x;
        let col = Rect::from_min_size(body.min, vec2(DETAILS_W, body.height()));
        let art = Rect::from_min_size(col.min, vec2(ART_PAGE, ART_PAGE));
        self.art_at(ui, art, 12.0, 28.0, theme);
        let text_x = x + ART_PAGE + 10.0;
        let text_w = DETAILS_W - ART_PAGE - 10.0;
        let title = if now.title.is_empty() {
            "Unknown title"
        } else {
            &now.title
        };
        let h = text_block(
            ui.painter(),
            pos2(text_x, col.min.y + 2.0),
            title,
            text_w,
            2,
            Theme::font_semibold(SIZE_LABEL + 3.0),
            theme.text,
        );
        let mut y = col.min.y + 2.0 + h + 4.0;
        if !now.artist.is_empty() {
            y += text_block(
                ui.painter(),
                pos2(text_x, y),
                &now.artist,
                text_w,
                1,
                Theme::font(SIZE_LABEL + 1.0),
                theme.text_dim,
            ) + 3.0;
        }
        if !now.source.is_empty() {
            text_line(
                ui,
                pos2(text_x, y),
                &format!("Playing in {}", now.source),
                text_w,
                Theme::font(SIZE_LABEL - 1.0),
                theme.text_faint,
            );
        }

        let mut rows: Vec<(&str, String)> = Vec::new();
        if !now.album.is_empty() {
            rows.push(("Album", now.album.clone()));
        }
        if !now.album_artist.is_empty() && now.album_artist != now.artist {
            rows.push(("Album artist", now.album_artist.clone()));
        }
        if now.track_number > 0 {
            let value = if now.album_track_count > 0 {
                format!("{} of {}", now.track_number, now.album_track_count)
            } else {
                now.track_number.to_string()
            };
            rows.push(("Track", value));
        }
        if !now.genres.is_empty() {
            rows.push(("Genre", now.genres.join(", ")));
        }
        if let Some((_, dur)) = now.position() {
            rows.push(("Length", mmss(dur)));
        }
        let mut ry = col.min.y + ART_PAGE + 16.0;
        let label_w = 84.0;
        for (label, value) in &rows {
            text_line(
                ui,
                pos2(x, ry),
                label,
                label_w,
                Theme::font(SIZE_LABEL - 1.0),
                theme.text_faint,
            );
            text_line(
                ui,
                pos2(x + label_w, ry),
                value,
                DETAILS_W - label_w,
                Theme::font(SIZE_LABEL),
                theme.text_dim,
            );
            ry += 18.0;
        }

        // Progress and buttons at the bottom of the column.
        if let Some((pos, dur)) = now.position() {
            let bar_y = body.max.y - 46.0;
            let bar = Rect::from_min_size(pos2(x, bar_y), vec2(DETAILS_W, 3.0));
            ui.painter()
                .rect_filled(bar, 1.5, with_alpha(theme.text_faint, 0.3));
            let frac = (pos / dur.max(1.0)).clamp(0.0, 1.0) as f32;
            let mut fill = bar;
            fill.set_width(bar.width() * frac);
            ui.painter().rect_filled(fill, 1.5, theme.accent_fg());
            ui.painter().text(
                pos2(x, bar_y - 3.0),
                Align2::LEFT_BOTTOM,
                mmss(pos),
                Theme::font(SIZE_LABEL - 1.0),
                theme.text_dim,
            );
            ui.painter().text(
                pos2(x + DETAILS_W, bar_y - 3.0),
                Align2::RIGHT_BOTTOM,
                mmss(dur),
                Theme::font(SIZE_LABEL - 1.0),
                theme.text_dim,
            );
            if now.playing {
                ctx.request_repaint_after(Duration::from_millis(500));
            }
        }
        self.transport(ui, &now, x + DETAILS_W * 0.5, body.max.y - 14.0, theme);

        // Divider and the lyrics column.
        let split = x + DETAILS_W + 14.0;
        ui.painter().line_segment(
            [pos2(split, body.min.y), pos2(split, body.max.y)],
            egui::Stroke::new(1.0, with_alpha(theme.text_faint, 0.25)),
        );
        let lyrics_rect = Rect::from_min_max(pos2(split + 14.0, body.min.y), body.max);
        self.lyrics_view(ui, lyrics_rect, &now, theme);
        false
    }
}
