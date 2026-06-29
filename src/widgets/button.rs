use std::{
    fs::File,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Result};
use cairo::{Antialias, Context, Format, ImageSurface};
use freedesktop_icons::lookup;
use input_linux::{uinput::UInputHandle, Key};
use librsvg_rebind::{prelude::HandleExt, Handle, Rectangle};

use crate::battery::find_battery_device;
use crate::config::ButtonConfig;
use crate::input::toggle_keys;
use crate::widgets::indicator::Indicator;
use crate::{dbg_ts, DEFAULT_ICON_SIZE};

pub(crate) enum ButtonImage {
    Text(String),
    Svg(Handle),
    Bitmap(ImageSurface),
    Indicator(Indicator),
    Spacer,
}

pub(crate) struct Button {
    pub(crate) image: ButtonImage,
    pub(crate) changed: bool,
    pub(crate) active: bool,
    action: Vec<Key>,
    icon_width: f64,
    icon_height: f64,
    /// If set, touching this button opens the named layer as a momentary modal
    /// (e.g. a slider) instead of sending its key.
    pub(crate) open_layer: Option<String>,
}

fn try_load_svg(path: &str) -> Result<ButtonImage> {
    Ok(ButtonImage::Svg(
        Handle::from_file(path).map_err(|_| anyhow!("failed to load image"))?,
    ))
}

fn try_load_png(path: impl AsRef<Path>, icon_width: i32, icon_height: i32) -> Result<ButtonImage> {
    let mut file = File::open(path)?;
    let surf = ImageSurface::create_from_png(&mut file)?;
    if surf.height() == icon_height && surf.width() == icon_width {
        return Ok(ButtonImage::Bitmap(surf));
    }
    let resized = ImageSurface::create(Format::ARgb32, icon_width, icon_height).unwrap();
    let c = Context::new(&resized).unwrap();
    c.scale(
        icon_width as f64 / surf.width() as f64,
        icon_height as f64 / surf.height() as f64,
    );
    c.set_source_surface(surf, 0.0, 0.0).unwrap();
    c.set_antialias(Antialias::Best);
    c.paint().unwrap();
    Ok(ButtonImage::Bitmap(resized))
}

pub(crate) fn try_load_image(
    name: impl AsRef<str>,
    theme: Option<impl AsRef<str>>,
    icon_width: i32,
    icon_height: i32,
) -> Result<ButtonImage> {
    let name = name.as_ref();
    let locations;

    // Load list of candidate locations
    if let Some(theme) = theme {
        // Freedesktop icons
        let theme = theme.as_ref();
        let candidates = vec![
            lookup(name)
                .with_cache()
                .with_theme(theme)
                .with_size(icon_height as u16)
                .force_svg()
                .find(),
            lookup(name)
                .with_cache()
                .with_theme(theme)
                .force_svg()
                .find(),
        ];

        // .flatten() removes `None` and unwraps `Some` values
        locations = candidates.into_iter().flatten().collect();
    } else {
        // Standard file icons
        locations = vec![
            PathBuf::from(format!("/etc/tiny-dfr/{name}.svg")),
            PathBuf::from(format!("/etc/tiny-dfr/{name}.png")),
            PathBuf::from(format!("/usr/share/tiny-dfr/{name}.svg")),
            PathBuf::from(format!("/usr/share/tiny-dfr/{name}.png")),
        ];
    };

    // Try to load each candidate
    let mut last_err = anyhow!("no suitable icon path was found"); // in case locations is empty

    for location in locations {
        let result = match location.extension().and_then(|s| s.to_str()) {
            Some("png") => try_load_png(&location, icon_width, icon_height),
            Some("svg") => try_load_svg(
                location
                    .to_str()
                    .ok_or(anyhow!("image path is not unicode"))?,
            ),
            _ => Err(anyhow!("invalid file extension")),
        };

        match result {
            Ok(image) => return Ok(image),
            Err(err) => {
                last_err = err.context(format!("while loading path {}", location.display()));
            }
        };
    }

    // if function hasn't returned by now, all sources have been exhausted
    Err(last_err.context(format!("failed loading all possible paths for icon {name}")))
}

impl Button {
    pub(crate) fn with_config(cfg: ButtonConfig) -> Button {
        let open_layer = cfg.open_layer.clone();
        let mut button = if let Some(text) = cfg.text {
            Button::new_text(text, cfg.action)
        } else if let Some(icon) = cfg.icon {
            Button::new_icon(
                &icon,
                cfg.theme,
                cfg.action,
                cfg.icon_width.unwrap_or(DEFAULT_ICON_SIZE),
                cfg.icon_height.unwrap_or(DEFAULT_ICON_SIZE),
            )
        } else if let Some(time) = cfg.time {
            Button::new_indicator(cfg.action, Indicator::new_clock(&time, cfg.locale.as_deref()))
        } else if let Some(battery_mode) = cfg.battery {
            if let Some(battery) = find_battery_device() {
                Button::new_indicator(
                    cfg.action,
                    Indicator::new_battery(battery, battery_mode, cfg.theme),
                )
            } else {
                Button::new_text("Battery N/A".to_string(), cfg.action)
            }
        } else {
            Button::new_spacer()
        };
        button.open_layer = open_layer;
        button
    }
    fn new_spacer() -> Button {
        Button {
            action: vec![],
            active: false,
            changed: false,
            image: ButtonImage::Spacer,
            icon_width: 0.0,
            icon_height: 0.0,
            open_layer: None,
        }
    }
    fn new_text(text: String, action: Vec<Key>) -> Button {
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::Text(text),
            icon_width: 0.0,
            icon_height: 0.0,
            open_layer: None,
        }
    }
    fn new_indicator(action: Vec<Key>, ind: Indicator) -> Button {
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::Indicator(ind),
            icon_width: 0.0,
            icon_height: 0.0,
            open_layer: None,
        }
    }
    fn new_icon(
        path: impl AsRef<str>,
        theme: Option<impl AsRef<str>>,
        action: Vec<Key>,
        icon_width: i32,
        icon_height: i32,
    ) -> Button {
        let image =
            try_load_image(path, theme, icon_width, icon_height).expect("failed to load icon");
        Button {
            action,
            image,
            icon_width: icon_width as f64,
            icon_height: icon_height as f64,
            active: false,
            changed: false,
            open_layer: None,
        }
    }
    pub(crate) fn needs_faster_refresh(&self) -> bool {
        match &self.image {
            ButtonImage::Indicator(ind) => ind.needs_faster_refresh(),
            _ => false,
        }
    }
    pub(crate) fn render(
        &self,
        c: &Context,
        height: i32,
        button_left_edge: f64,
        button_width: u64,
        y_shift: f64,
    ) {
        match &self.image {
            ButtonImage::Text(text) => {
                let extents = c.text_extents(text).unwrap();
                c.move_to(
                    button_left_edge + (button_width as f64 / 2.0 - extents.width() / 2.0).round(),
                    y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
                );
                c.show_text(text).unwrap();
            }
            ButtonImage::Svg(svg) => {
                let x =
                    button_left_edge + (button_width as f64 / 2.0 - self.icon_width / 2.0).round();
                let y = y_shift + ((height as f64 - self.icon_height) / 2.0).round();

                svg.render_document(c, &Rectangle::new(x, y, self.icon_width, self.icon_height))
                    .unwrap();
            }
            ButtonImage::Bitmap(surf) => {
                let x =
                    button_left_edge + (button_width as f64 / 2.0 - self.icon_width / 2.0).round();
                let y = y_shift + ((height as f64 - self.icon_height) / 2.0).round();
                c.set_source_surface(surf, x, y).unwrap();
                c.rectangle(x, y, self.icon_width, self.icon_height);
                c.fill().unwrap();
            }
            ButtonImage::Indicator(ind) => {
                ind.render(c, height, button_left_edge, button_width, y_shift)
            }
            ButtonImage::Spacer => (),
        }
    }
    pub(crate) fn set_active<F>(&mut self, uinput: &mut UInputHandle<F>, active: bool)
    where
        F: AsRawFd,
    {
        if self.active != active {
            self.active = active;
            self.changed = true;
            eprintln!(
                "[dbg {:.6}] set_active={} key={:?}",
                dbg_ts(),
                active,
                self.action.first()
            );
            toggle_keys(uinput, &self.action, active as i32);
        }
    }
    pub(crate) fn set_background_color(&self, c: &Context, color: f64) {
        if let ButtonImage::Indicator(ind) = &self.image {
            ind.background_color(c, color)
        } else {
            c.set_source_rgb(color, color, color);
        }
    }
}
