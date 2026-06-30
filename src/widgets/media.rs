use anyhow::Result;
use cairo::{Context, ImageSurface};
use drm::control::ClipRect;
use std::{fs::File, path::Path};

use crate::action::Action;
use crate::config::Config;
use crate::store::{key, Store};
use crate::widgets::Region;

const ART_FRAC: f64 = 0.84;
const CONTROL_FRAC: f64 = 0.34;
const GAP: f64 = 14.0;
const CONTROL_GAP: f64 = 2.0;

#[derive(Clone, Copy)]
enum MediaPress {
    Previous,
    PlayPause,
    Next,
    Scrubber,
}

pub(crate) struct MediaWidget {
    pub(crate) changed: bool,
    active: Option<MediaPress>,
}

impl MediaWidget {
    pub(crate) fn new() -> MediaWidget {
        MediaWidget {
            changed: true,
            active: None,
        }
    }

    pub(crate) fn needs_redraw(&self, store: &Store) -> bool {
        self.changed
            || store.is_dirty(key::MEDIA_ACTIVE_TITLE).unwrap_or(false)
            || store.is_dirty(key::MEDIA_ACTIVE_ART_URL).unwrap_or(false)
            || store.is_dirty(key::MEDIA_ACTIVE_POSITION).unwrap_or(false)
            || store.is_dirty(key::MEDIA_ACTIVE_LENGTH).unwrap_or(false)
            || store.is_dirty(key::MEDIA_ACTIVE_STATUS).unwrap_or(false)
    }

    pub(crate) fn on_press(&mut self, x: f64, region: &Region, store: &Store) -> Option<Action> {
        if self.is_in_scrub_well(x, region, store) {
            if let Some(seek) = self.seek_from_x(x, region, store) {
                self.active = Some(MediaPress::Scrubber);
                self.changed = true;
                return Some(Action::MediaSeek(seek));
            }
        }
        let press = self.hit_control(x, region)?;
        self.active = Some(press);
        self.changed = true;
        Some(match press {
            MediaPress::Previous => Action::MediaPrevious,
            MediaPress::PlayPause => Action::MediaPlayPause,
            MediaPress::Next => Action::MediaNext,
            MediaPress::Scrubber => unreachable!(),
        })
    }

    pub(crate) fn on_drag(&mut self, x: f64, region: &Region, store: &Store) -> Option<Action> {
        if matches!(self.active, Some(MediaPress::Scrubber)) {
            self.changed = true;
            self.seek_from_x(x, region, store).map(Action::MediaSeek)
        } else {
            None
        }
    }

    pub(crate) fn on_release(&mut self) -> Option<Action> {
        if self.active.take().is_some() {
            self.changed = true;
        }
        None
    }

    fn hit_control(&self, x: f64, region: &Region) -> Option<MediaPress> {
        let (_, _, controls) = self.layout(region);
        for (idx, (left, right)) in controls.into_iter().enumerate() {
            if x >= left && x <= right {
                return Some(match idx {
                    0 => MediaPress::Previous,
                    1 => MediaPress::PlayPause,
                    _ => MediaPress::Next,
                });
            }
        }
        None
    }

    fn layout(&self, region: &Region) -> (f64, (f64, f64), [(f64, f64); 3]) {
        let art_size = (region.height as f64 * ART_FRAC).min(region.width * 0.22);
        let control_w = (region.height as f64 * CONTROL_FRAC).max(116.0);
        let controls_total = control_w * 3.0 + CONTROL_GAP * 2.0;
        let controls_start = region.left + region.width - controls_total;
        let art_left = region.left;
        let scrub_left = art_left + art_size + GAP;
        let scrub_right = (controls_start - GAP).max(scrub_left + 1.0);
        let controls = [
            (controls_start, controls_start + control_w),
            (
                controls_start + control_w + CONTROL_GAP,
                controls_start + control_w * 2.0 + CONTROL_GAP,
            ),
            (
                controls_start + control_w * 2.0 + CONTROL_GAP * 2.0,
                controls_start + control_w * 3.0 + CONTROL_GAP * 2.0,
            ),
        ];
        (art_size, (scrub_left, scrub_right), controls)
    }

    fn scrub_well_bounds(&self, region: &Region, store: &Store) -> Option<(f64, f64)> {
        let (_, (left, right), _) = self.layout(region);
        let length = store.number(key::MEDIA_ACTIVE_LENGTH).unwrap_or(0.0);
        let pos = store.number(key::MEDIA_ACTIVE_POSITION).unwrap_or(0.0);
        let label_w = time_label_width(pos, length);
        let bar_left = left + label_w + 14.0;
        (right > bar_left).then_some((bar_left, right))
    }

    fn is_in_scrub_well(&self, x: f64, region: &Region, store: &Store) -> bool {
        self.scrub_well_bounds(region, store)
            .is_some_and(|(left, right)| x >= left && x <= right)
    }

    fn seek_from_x(&self, x: f64, region: &Region, store: &Store) -> Option<f64> {
        let length = store.number(key::MEDIA_ACTIVE_LENGTH).ok()?;
        if length <= 0.0 {
            return None;
        }
        let (left, right) = self.scrub_well_bounds(region, store)?;
        // Absolute finger position maps directly to media position. Values outside
        // the well clamp to the nearest edge.
        let pct = ((x - left) / (right - left)).clamp(0.0, 1.0);
        Some(length * pct)
    }

    pub(crate) fn draw(
        &mut self,
        c: &Context,
        cfg: &Config,
        region: &Region,
        store: &Store,
        complete_redraw: bool,
    ) -> Result<Vec<ClipRect>> {
        let radius = 8.0f64;
        let top = region.height as f64 * 0.15;
        let bot = region.height as f64 * 0.85;
        if !complete_redraw {
            c.set_source_rgb(0.0, 0.0, 0.0);
            c.rectangle(
                region.left,
                top - radius,
                region.width,
                bot - top + radius * 2.0,
            );
            c.fill()?;
        }

        let (art_size, scrub, controls) = self.layout(region);
        self.draw_art(c, region, art_size, store)?;
        self.draw_progress(c, region, scrub, store)?;
        self.draw_controls(c, cfg, region, controls, store)?;
        self.changed = false;

        if complete_redraw {
            Ok(vec![])
        } else {
            Ok(vec![ClipRect::new(
                region.height as u16 - bot as u16 - radius as u16,
                region.left as u16,
                region.height as u16 - top as u16 + radius as u16,
                (region.left + region.width) as u16,
            )])
        }
    }

    fn draw_art(&self, c: &Context, region: &Region, art_size: f64, store: &Store) -> Result<()> {
        let x = region.left;
        let y = (region.height as f64 - art_size) / 2.0 + region.y_shift;
        let path = store
            .text(key::MEDIA_ACTIVE_ART_URL)
            .ok()
            .and_then(file_url_to_path);
        if let Some(path) = path {
            if let Ok(surface) = load_png(&path) {
                c.save()?;
                rounded_rect(c, x, y, art_size, art_size, 10.0);
                c.clip();
                c.translate(x, y);
                c.scale(
                    art_size / surface.width() as f64,
                    art_size / surface.height() as f64,
                );
                c.set_source_surface(surface, 0.0, 0.0)?;
                c.paint()?;
                c.restore()?;
                return Ok(());
            }
        }
        c.set_source_rgb(0.16, 0.16, 0.18);
        rounded_rect(c, x, y, art_size, art_size, 10.0);
        c.fill()?;
        Ok(())
    }

    fn draw_progress(
        &self,
        c: &Context,
        region: &Region,
        (left, right): (f64, f64),
        store: &Store,
    ) -> Result<()> {
        let length = store.number(key::MEDIA_ACTIVE_LENGTH).unwrap_or(0.0);
        let pos = store.number(key::MEDIA_ACTIVE_POSITION).unwrap_or(0.0);
        let pct = if length > 0.0 {
            (pos / length).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let label = format_time_pair(pos, length);
        c.set_source_rgb(1.0, 1.0, 1.0);
        c.set_font_size(24.0);
        let ext = c.text_extents(&label)?;
        let label_w = time_label_width(pos, length);
        let label_x = left + (label_w - ext.width()).max(0.0);
        let label_y = region.y_shift + (region.height as f64 / 2.0 + ext.height() / 2.0).round();
        c.move_to(label_x, label_y);
        c.show_text(&label)?;

        let bar_left = left + label_w + 14.0;
        let w = (right - bar_left).max(1.0);
        let radius = 8.0;
        let bot = region.height as f64 * 0.15;
        let top = region.height as f64 * 0.85;
        let y = bot - radius + region.y_shift;
        let h = top - bot + radius * 2.0;

        c.set_source_rgb(0.0, 0.0, 0.0);
        rounded_rect(c, bar_left, y, w, h, radius);
        c.fill()?;
        c.set_source_rgb(0.55, 0.55, 0.58);
        c.set_line_width(1.5);
        rounded_rect(c, bar_left + 0.75, y + 0.75, w - 1.5, h - 1.5, radius);
        c.stroke()?;

        let title = store.text(key::MEDIA_ACTIVE_TITLE).unwrap_or("");
        if !title.is_empty() {
            c.save()?;
            c.rectangle(bar_left + 10.0, y, (w - 20.0).max(1.0), h);
            c.clip();
            c.set_source_rgb(0.28, 0.28, 0.30);
            c.set_font_size(26.0);
            let title_ext = c.text_extents(title)?;
            c.move_to(
                bar_left + ((w - title_ext.width()) / 2.0).max(12.0).round(),
                y + (h / 2.0 + title_ext.height() / 2.0).round(),
            );
            c.show_text(title)?;
            c.restore()?;
        }

        let thumb_w = 7.0;
        let thumb_h = h * 0.82;
        let thumb_x = (bar_left + w * pct - thumb_w / 2.0).clamp(bar_left, bar_left + w - thumb_w);
        let thumb_y = y + (h - thumb_h) / 2.0;
        c.set_source_rgb(0.95, 0.95, 0.96);
        rounded_rect(c, thumb_x, thumb_y, thumb_w, thumb_h, thumb_w / 2.0);
        c.fill()?;
        Ok(())
    }

    fn draw_controls(
        &self,
        c: &Context,
        cfg: &Config,
        region: &Region,
        controls: [(f64, f64); 3],
        store: &Store,
    ) -> Result<()> {
        for (idx, (left, right)) in controls.into_iter().enumerate() {
            let active = matches!(
                (self.active, idx),
                (Some(MediaPress::Previous), 0)
                    | (Some(MediaPress::PlayPause), 1)
                    | (Some(MediaPress::Next), 2)
            );
            let color = if active {
                0.40
            } else if cfg.show_button_outlines {
                0.20
            } else {
                0.0
            };
            let radius = 8.0;
            let bot = region.height as f64 * 0.15;
            let top = region.height as f64 * 0.85;
            let y = bot - radius + region.y_shift;
            let h = top - bot + radius * 2.0;
            c.set_source_rgb(color, color, color);
            rounded_rect(c, left, y, right - left, h, radius);
            c.fill()?;
            c.set_source_rgb(1.0, 1.0, 1.0);
            draw_media_icon(
                c,
                idx,
                store.text(key::MEDIA_ACTIVE_STATUS).unwrap_or("") == "Playing",
                left,
                y,
                right - left,
                h,
            )?;
        }
        Ok(())
    }
}

fn draw_media_icon(
    c: &Context,
    idx: usize,
    playing: bool,
    left: f64,
    top: f64,
    width: f64,
    height: f64,
) -> Result<()> {
    let cx = left + width / 2.0;
    let cy = top + height / 2.0;
    let size = height * 0.48;
    match idx {
        0 => {
            let tri = size * 0.58;
            let gap = size * 0.08;
            draw_triangle(c, cx + gap / 2.0, cy, -tri, size)?;
            draw_triangle(c, cx + gap / 2.0 + tri * 0.72, cy, -tri, size)?;
        }
        1 if playing => {
            let bar_w = size * 0.22;
            let gap = size * 0.16;
            let total = bar_w * 2.0 + gap;
            c.rectangle(cx - total / 2.0, cy - size * 0.5, bar_w, size);
            c.rectangle(cx - total / 2.0 + bar_w + gap, cy - size * 0.5, bar_w, size);
            c.fill()?;
        }
        1 => draw_triangle(c, cx - size * 0.42, cy, size * 0.84, size)?,
        _ => {
            let tri = size * 0.58;
            let gap = size * 0.08;
            draw_triangle(c, cx - gap / 2.0 - tri * 0.72, cy, tri, size)?;
            draw_triangle(c, cx - gap / 2.0, cy, tri, size)?;
        }
    }
    Ok(())
}

fn draw_triangle(c: &Context, x: f64, y: f64, width: f64, height: f64) -> Result<()> {
    c.move_to(x, y - height / 2.0);
    c.line_to(x, y + height / 2.0);
    c.line_to(x + width, y);
    c.close_path();
    c.fill()?;
    Ok(())
}

fn time_label_width(position_us: f64, length_us: f64) -> f64 {
    let position = (position_us / 1_000_000.0).max(0.0).round() as u64;
    let length = (length_us / 1_000_000.0).max(0.0).round() as u64;
    if position >= 3600 || length >= 3600 {
        245.0
    } else {
        160.0
    }
}

fn format_time_pair(position_us: f64, length_us: f64) -> String {
    let position = (position_us / 1_000_000.0).max(0.0).round() as u64;
    let length = (length_us / 1_000_000.0).max(0.0).round() as u64;
    let with_hours = position >= 3600 || length >= 3600;
    format!(
        "{}/{}",
        format_time(position, with_hours),
        format_time(length, with_hours)
    )
}

fn format_time(seconds: u64, with_hours: bool) -> String {
    let h = seconds / 3600;
    let m = (seconds % 3600) / 60;
    let s = seconds % 60;
    if with_hours {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

fn file_url_to_path(url: &str) -> Option<String> {
    url.strip_prefix("file://").map(|path| path.to_string())
}

fn load_png(path: &str) -> Result<ImageSurface> {
    let mut file = File::open(Path::new(path))?;
    Ok(ImageSurface::create_from_png(&mut file)?)
}

fn rounded_rect(c: &Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    c.new_sub_path();
    c.arc(
        x + w - r,
        y + r,
        r,
        (-90f64).to_radians(),
        0f64.to_radians(),
    );
    c.arc(
        x + w - r,
        y + h - r,
        r,
        0f64.to_radians(),
        90f64.to_radians(),
    );
    c.arc(x + r, y + h - r, r, 90f64.to_radians(), 180f64.to_radians());
    c.arc(x + r, y + r, r, 180f64.to_radians(), 270f64.to_radians());
    c.close_path();
}
