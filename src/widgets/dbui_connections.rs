use anyhow::Result;
use cairo::Context;
use drm::control::ClipRect;
use serde::Deserialize;

use crate::action::Action;
use crate::config::Config;
use crate::store::{key, Store};
use crate::widgets::Region;

const CHIP_GAP: f64 = 8.0;
const MIN_CHIP_WIDTH: f64 = 170.0;
const MAX_VISIBLE_DBS: usize = 8;

#[derive(Debug, Deserialize)]
struct DbConnection {
    name: String,
    source: String,
    key_name: String,
    is_connected: bool,
}

pub(crate) struct DbuiConnectionsWidget {
    changed: bool,
}

impl DbuiConnectionsWidget {
    pub(crate) fn new() -> DbuiConnectionsWidget {
        DbuiConnectionsWidget { changed: true }
    }

    pub(crate) fn needs_redraw(&self, store: &Store) -> bool {
        self.changed
            || store.is_dirty(key::NVIM_DBUI_AVAILABLE).unwrap_or(false)
            || store
                .is_dirty(key::NVIM_DBUI_CONNECTIONS_JSON)
                .unwrap_or(false)
    }

    pub(crate) fn on_press(&mut self, x: f64, region: &Region, store: &Store) -> Option<Action> {
        let connections = read_connections(store);
        if connections.is_empty() {
            return None;
        }
        let visible = visible_count(region.width, connections.len());
        for (idx, (left, right)) in self.chip_bounds(region, visible).into_iter().enumerate() {
            if x >= left && x <= right {
                self.changed = true;
                return connections
                    .get(idx)
                    .map(|db| Action::NvimDbConnect(db.key_name.clone()));
            }
        }
        None
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

        if !store.bool(key::NVIM_DBUI_AVAILABLE).unwrap_or(false) {
            draw_centered_text(c, region, "DBUI unavailable", 26.0, (0.45, 0.45, 0.48))?;
            self.changed = false;
            return Ok(damage(region, complete_redraw, top, bot, radius));
        }

        let connections = read_connections(store);
        if connections.is_empty() {
            draw_centered_text(c, region, "No DBUI connections", 26.0, (0.45, 0.45, 0.48))?;
            self.changed = false;
            return Ok(damage(region, complete_redraw, top, bot, radius));
        }

        c.set_font_face(&cfg.font_face);
        c.set_font_size(24.0);
        let visible = visible_count(region.width, connections.len());
        for (db, (left, right)) in connections
            .into_iter()
            .take(visible)
            .zip(self.chip_bounds(region, visible))
        {
            draw_db_chip(c, region, &db, left, right)?;
        }
        self.changed = false;
        Ok(damage(region, complete_redraw, top, bot, radius))
    }

    fn chip_bounds(&self, region: &Region, count: usize) -> Vec<(f64, f64)> {
        if count == 0 {
            return Vec::new();
        }
        let total_gap = CHIP_GAP * (count.saturating_sub(1) as f64);
        let chip_w = ((region.width - total_gap) / count as f64).max(1.0);
        (0..count)
            .map(|idx| {
                let left = region.left + idx as f64 * (chip_w + CHIP_GAP);
                (left, left + chip_w)
            })
            .collect()
    }
}

fn read_connections(store: &Store) -> Vec<DbConnection> {
    let json = store.text(key::NVIM_DBUI_CONNECTIONS_JSON).unwrap_or("[]");
    serde_json::from_str(json).unwrap_or_default()
}

fn visible_count(width: f64, count: usize) -> usize {
    if count == 0 {
        return 0;
    }
    let by_width = (width / MIN_CHIP_WIDTH).floor().max(1.0) as usize;
    count.min(by_width).min(MAX_VISIBLE_DBS)
}

fn draw_db_chip(
    c: &Context,
    region: &Region,
    db: &DbConnection,
    left: f64,
    right: f64,
) -> Result<()> {
    let top = region.height as f64 * 0.09 + region.y_shift;
    let height = region.height as f64 * 0.82;
    let width = (right - left).max(1.0);
    let radius = 10.0;
    if db.is_connected {
        c.set_source_rgb(0.20, 0.34, 0.24);
    } else {
        c.set_source_rgb(0.16, 0.16, 0.18);
    }
    rounded_rect(c, left, top, width, height, radius);
    c.fill()?;

    c.set_source_rgb(0.30, 0.72, 0.62);
    c.set_line_width(1.5);
    rounded_rect(
        c,
        left + 0.75,
        top + 0.75,
        width - 1.5,
        height - 1.5,
        radius,
    );
    c.stroke()?;

    let label = if db.source.trim().is_empty() {
        db.name.clone()
    } else {
        format!("{} · {}", db.name, db.source)
    };
    let label = ellipsize(c, &label, width - 24.0)?;
    let ext = c.text_extents(&label)?;
    c.set_source_rgb(0.88, 0.92, 0.90);
    c.move_to(
        left + (width / 2.0 - ext.width() / 2.0 - ext.x_bearing()).round(),
        region.y_shift
            + (region.height as f64 / 2.0 - ext.height() / 2.0 - ext.y_bearing()).round(),
    );
    c.show_text(&label)?;
    Ok(())
}

fn draw_centered_text(
    c: &Context,
    region: &Region,
    text: &str,
    font_size: f64,
    color: (f64, f64, f64),
) -> Result<()> {
    c.set_font_size(font_size);
    c.set_source_rgb(color.0, color.1, color.2);
    let ext = c.text_extents(text)?;
    c.move_to(
        region.left + (region.width / 2.0 - ext.width() / 2.0 - ext.x_bearing()).round(),
        region.y_shift
            + (region.height as f64 / 2.0 - ext.height() / 2.0 - ext.y_bearing()).round(),
    );
    c.show_text(text)?;
    Ok(())
}

fn ellipsize(c: &Context, text: &str, max_width: f64) -> Result<String> {
    if c.text_extents(text)?.width() <= max_width {
        return Ok(text.to_string());
    }
    let mut out = text.to_string();
    while !out.is_empty() {
        out.pop();
        let candidate = format!("{out}…");
        if c.text_extents(&candidate)?.width() <= max_width {
            return Ok(candidate);
        }
    }
    Ok("…".to_string())
}

fn rounded_rect(c: &Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.0).min(h / 2.0);
    c.new_sub_path();
    c.arc(x + w - r, y + r, r, (-90.0f64).to_radians(), 0.0);
    c.arc(x + w - r, y + h - r, r, 0.0, 90.0f64.to_radians());
    c.arc(
        x + r,
        y + h - r,
        r,
        90.0f64.to_radians(),
        180.0f64.to_radians(),
    );
    c.arc(
        x + r,
        y + r,
        r,
        180.0f64.to_radians(),
        270.0f64.to_radians(),
    );
    c.close_path();
}

fn damage(
    region: &Region,
    complete_redraw: bool,
    top: f64,
    bot: f64,
    radius: f64,
) -> Vec<ClipRect> {
    if complete_redraw {
        vec![]
    } else {
        vec![ClipRect::new(
            region.height as u16 - bot as u16 - radius as u16,
            region.left as u16,
            region.height as u16 - top as u16 + radius as u16,
            region.left as u16 + region.width as u16,
        )]
    }
}
