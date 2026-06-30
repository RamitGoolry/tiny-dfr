use anyhow::Result;
use cairo::{Context, ImageSurface};
use drm::control::ClipRect;
use std::{fs::File, path::Path};

use crate::action::Action;
use crate::config::Config;
use crate::store::{key, Store};
use crate::widgets::Region;

const ART_FRAC: f64 = 0.72;
const CONTROL_FRAC: f64 = 0.30;
const GAP: f64 = 14.0;
const CONTROL_GAP: f64 = 2.0;

#[derive(Clone, Copy)]
enum MediaPress {
    Previous,
    PlayPause,
    Next,
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
            || store.is_dirty(key::MEDIA_ACTIVE_ART_URL).unwrap_or(false)
            || store.is_dirty(key::MEDIA_ACTIVE_POSITION).unwrap_or(false)
            || store.is_dirty(key::MEDIA_ACTIVE_LENGTH).unwrap_or(false)
            || store.is_dirty(key::MEDIA_ACTIVE_STATUS).unwrap_or(false)
    }

    pub(crate) fn on_press(&mut self, x: f64, region: &Region) -> Option<Action> {
        let press = self.hit_control(x, region)?;
        self.active = Some(press);
        self.changed = true;
        Some(match press {
            MediaPress::Previous => Action::MediaPrevious,
            MediaPress::PlayPause => Action::MediaPlayPause,
            MediaPress::Next => Action::MediaNext,
        })
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
        let control_w = (region.height as f64 * CONTROL_FRAC).max(104.0);
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
        let w = (right - left).max(1.0);
        let h = region.height as f64 * 0.18;
        let y = (region.height as f64 - h) / 2.0 + region.y_shift;
        c.set_source_rgb(0.20, 0.20, 0.22);
        rounded_rect(c, left, y, w, h, h / 2.0);
        c.fill()?;
        c.set_source_rgb(0.20, 0.55, 1.0);
        rounded_rect(c, left, y, w * pct, h, h / 2.0);
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
