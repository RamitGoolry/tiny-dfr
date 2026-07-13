use anyhow::{Context as AnyhowContext, Result};
use cairo::{Context, Format, ImageSurface};
use drm::control::ClipRect;
use image::{imageops::FilterType, ImageReader};
use serde::Deserialize;
use std::{
    collections::HashMap,
    io::{Cursor, Read},
};

use crate::action::Action;
use crate::config::Config;
use crate::store::{key, Store};
use crate::widgets::Region;

const CHIP_GAP: f64 = 8.0;
const MIN_CHIP_WIDTH: f64 = 170.0;
const MAX_VISIBLE_TABS: usize = 8;

#[derive(Debug, Deserialize)]
struct BrowserTab {
    id: String,
    title: String,
    url: String,
    favicon_url: String,
    active: bool,
}

pub(crate) struct BrowserTabsWidget {
    changed: bool,
    favicons: HashMap<String, Option<ImageSurface>>,
}

impl BrowserTabsWidget {
    pub(crate) fn new() -> BrowserTabsWidget {
        BrowserTabsWidget {
            changed: true,
            favicons: HashMap::new(),
        }
    }

    pub(crate) fn needs_redraw(&self, store: &Store) -> bool {
        self.changed
            || store.is_dirty(key::BROWSER_TABS_AVAILABLE).unwrap_or(false)
            || store.is_dirty(key::BROWSER_TABS_JSON).unwrap_or(false)
            || store
                .is_dirty(key::BROWSER_TABS_ACTIVE_INDEX)
                .unwrap_or(false)
    }

    pub(crate) fn on_press(&mut self, x: f64, region: &Region, store: &Store) -> Option<Action> {
        let tabs = read_tabs(store);
        if tabs.is_empty() {
            return None;
        }
        let indices = visible_indices(&tabs, visible_count(region.width, tabs.len()));
        for (visible_idx, (left, right)) in self
            .chip_bounds(region, indices.len())
            .into_iter()
            .enumerate()
        {
            if x >= left && x <= right {
                self.changed = true;
                return indices
                    .get(visible_idx)
                    .and_then(|idx| tabs.get(*idx))
                    .map(|tab| Action::BrowserActivateTab(tab.id.clone()));
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

        if !store.bool(key::BROWSER_TABS_AVAILABLE).unwrap_or(false) {
            draw_centered_text(
                c,
                region,
                "Browser tabs unavailable",
                26.0,
                (0.45, 0.45, 0.48),
            )?;
            self.changed = false;
            return Ok(damage(region, complete_redraw, top, bot, radius));
        }

        let tabs = read_tabs(store);
        if tabs.is_empty() {
            draw_centered_text(c, region, "No browser tabs", 26.0, (0.45, 0.45, 0.48))?;
            self.changed = false;
            return Ok(damage(region, complete_redraw, top, bot, radius));
        }

        c.set_font_face(&cfg.font_face);
        c.set_font_size(24.0);
        let indices = visible_indices(&tabs, visible_count(region.width, tabs.len()));
        let bounds = self.chip_bounds(region, indices.len());
        for (idx, (left, right)) in indices.into_iter().zip(bounds) {
            if let Some(tab) = tabs.get(idx) {
                draw_tab_chip(&mut self.favicons, c, region, tab, left, right)?;
            }
        }
        self.changed = false;
        Ok(damage(region, complete_redraw, top, bot, radius))
    }

    fn chip_bounds(&self, region: &Region, tab_count: usize) -> Vec<(f64, f64)> {
        let visible = visible_count(region.width, tab_count);
        if visible == 0 {
            return Vec::new();
        }
        let total_gap = CHIP_GAP * (visible.saturating_sub(1) as f64);
        let chip_w = ((region.width - total_gap) / visible as f64).max(1.0);
        (0..visible)
            .map(|idx| {
                let left = region.left + idx as f64 * (chip_w + CHIP_GAP);
                (left, left + chip_w)
            })
            .collect()
    }
}

fn read_tabs(store: &Store) -> Vec<BrowserTab> {
    let json = store.text(key::BROWSER_TABS_JSON).unwrap_or("[]");
    serde_json::from_str(json).unwrap_or_default()
}

fn visible_indices(tabs: &[BrowserTab], visible_count: usize) -> Vec<usize> {
    if visible_count >= tabs.len() {
        return (0..tabs.len()).collect();
    }
    let active_idx = tabs.iter().position(|tab| tab.active);
    let mut indices: Vec<usize> = (0..visible_count).collect();
    if let Some(active_idx) = active_idx {
        if active_idx >= visible_count && visible_count > 0 {
            indices[visible_count - 1] = active_idx;
        }
    }
    indices
}

fn visible_count(width: f64, tab_count: usize) -> usize {
    if tab_count == 0 {
        return 0;
    }
    let by_width = (width / MIN_CHIP_WIDTH).floor().max(1.0) as usize;
    tab_count.min(by_width).min(MAX_VISIBLE_TABS)
}

fn draw_tab_chip(
    favicons: &mut HashMap<String, Option<ImageSurface>>,
    c: &Context,
    region: &Region,
    tab: &BrowserTab,
    left: f64,
    right: f64,
) -> Result<()> {
    let top = region.height as f64 * 0.09 + region.y_shift;
    let height = region.height as f64 * 0.82;
    let width = (right - left).max(1.0);
    let radius = 10.0;
    if tab.active {
        c.set_source_rgb(0.36, 0.36, 0.40);
    } else {
        c.set_source_rgb(0.16, 0.16, 0.18);
    }
    rounded_rect(c, left, top, width, height, radius);
    c.fill()?;

    c.set_source_rgb(0.72, 0.72, 0.76);
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

    let title = if tab.title.trim().is_empty() {
        tab.url.as_str()
    } else {
        tab.title.as_str()
    };
    let icon_size = 22.0;
    let icon_gap = 9.0;
    let label = ellipsize(c, title, width - icon_size - icon_gap - 32.0)?;
    let ext = c.text_extents(&label)?;
    let group_w = icon_size + icon_gap + ext.width();
    let group_left = left + ((width - group_w) / 2.0).max(12.0);
    let icon_y = region.y_shift + (region.height as f64 / 2.0 - icon_size / 2.0).round();
    if let Some(surface) = favicon_surface(favicons, &tab.favicon_url) {
        draw_favicon(c, surface, group_left, icon_y, icon_size)?;
    } else {
        draw_tab_icon(c, group_left, icon_y, icon_size, tab.active)?;
    }
    c.set_source_rgb(0.88, 0.88, 0.90);
    c.move_to(
        group_left + icon_size + icon_gap - ext.x_bearing(),
        region.y_shift
            + (region.height as f64 / 2.0 - ext.height() / 2.0 - ext.y_bearing()).round(),
    );
    c.show_text(&label)?;
    Ok(())
}

fn favicon_surface<'a>(
    favicons: &'a mut HashMap<String, Option<ImageSurface>>,
    url: &str,
) -> Option<&'a ImageSurface> {
    if url.trim().is_empty() {
        return None;
    }
    if !favicons.contains_key(url) {
        let surface = fetch_favicon(url)
            .inspect_err(|err| eprintln!("browser favicon: {url}: {err:#}"))
            .ok();
        favicons.insert(url.to_string(), surface);
    }
    favicons.get(url).and_then(Option::as_ref)
}

fn fetch_favicon(url: &str) -> Result<ImageSurface> {
    if url.starts_with("data:") {
        return Err(anyhow::anyhow!("data URI favicons are not supported yet"));
    }
    let mut response = ureq::get(url)
        .call()
        .with_context(|| format!("fetching favicon from {url}"))?;
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .read_to_end(&mut bytes)
        .context("reading favicon response")?;
    let image = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .context("guessing favicon format")?
        .decode()
        .context("decoding favicon")?
        .resize_exact(32, 32, FilterType::Lanczos3)
        .into_rgba8();
    rgba_to_cairo_surface(image.as_raw(), image.width(), image.height())
}

fn rgba_to_cairo_surface(rgba: &[u8], width: u32, height: u32) -> Result<ImageSurface> {
    let mut surface = ImageSurface::create(Format::ARgb32, width as i32, height as i32)
        .context("creating favicon surface")?;
    let stride = surface.stride() as usize;
    {
        let mut data = surface.data().context("mapping favicon surface")?;
        for y in 0..height as usize {
            for x in 0..width as usize {
                let src = (y * width as usize + x) * 4;
                let dst = y * stride + x * 4;
                let r = rgba[src] as u16;
                let g = rgba[src + 1] as u16;
                let b = rgba[src + 2] as u16;
                let a = rgba[src + 3] as u16;
                data[dst] = ((b * a + 127) / 255) as u8;
                data[dst + 1] = ((g * a + 127) / 255) as u8;
                data[dst + 2] = ((r * a + 127) / 255) as u8;
                data[dst + 3] = a as u8;
            }
        }
    }
    Ok(surface)
}

fn draw_favicon(c: &Context, surface: &ImageSurface, x: f64, y: f64, size: f64) -> Result<()> {
    c.save()?;
    c.translate(x, y);
    c.scale(
        size / surface.width() as f64,
        size / surface.height() as f64,
    );
    c.set_source_surface(surface, 0.0, 0.0)?;
    c.paint()?;
    c.restore()?;
    Ok(())
}

fn draw_tab_icon(c: &Context, x: f64, y: f64, size: f64, active: bool) -> Result<()> {
    c.save()?;
    let radius = 4.0;
    if active {
        c.set_source_rgb(0.86, 0.86, 0.90);
    } else {
        c.set_source_rgb(0.66, 0.66, 0.70);
    }
    c.set_line_width(2.0);
    rounded_rect(c, x, y + size * 0.18, size, size * 0.66, radius);
    c.stroke()?;
    c.move_to(x + size * 0.22, y + size * 0.40);
    c.line_to(x + size * 0.78, y + size * 0.40);
    c.stroke()?;
    c.restore()?;
    Ok(())
}

fn draw_centered_text(
    c: &Context,
    region: &Region,
    text: &str,
    size: f64,
    color: (f64, f64, f64),
) -> Result<()> {
    c.save()?;
    c.set_font_size(size);
    c.set_source_rgb(color.0, color.1, color.2);
    let ext = c.text_extents(text)?;
    c.move_to(
        region.left + (region.width / 2.0 - ext.width() / 2.0 - ext.x_bearing()).round(),
        region.y_shift
            + (region.height as f64 / 2.0 - ext.height() / 2.0 - ext.y_bearing()).round(),
    );
    c.show_text(text)?;
    c.restore()?;
    Ok(())
}

fn ellipsize(c: &Context, text: &str, max_width: f64) -> Result<String> {
    if c.text_extents(text)?.width() <= max_width {
        return Ok(text.to_string());
    }
    let ellipsis = "…";
    let mut chars: Vec<char> = text.chars().collect();
    while !chars.is_empty() {
        chars.pop();
        let candidate = format!("{}{}", chars.iter().collect::<String>(), ellipsis);
        if c.text_extents(&candidate)?.width() <= max_width {
            return Ok(candidate);
        }
    }
    Ok(ellipsis.to_string())
}

fn rounded_rect(c: &Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.0).min(h / 2.0);
    c.new_sub_path();
    c.arc(
        x + w - r,
        y + r,
        r,
        (-90.0f64).to_radians(),
        0.0f64.to_radians(),
    );
    c.arc(
        x + w - r,
        y + h - r,
        r,
        0.0f64.to_radians(),
        90.0f64.to_radians(),
    );
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
            (region.left + region.width) as u16,
        )]
    }
}
