use std::{
    fs::File,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Result};
use cairo::{Antialias, Context, Format, ImageSurface};
use freedesktop_icons::lookup;
use input_linux::Key;
use librsvg_rebind::{prelude::HandleExt, Handle, Rectangle};

use drm::control::ClipRect;

use crate::action::{Action, Edge};
use crate::config::{ButtonConfig, Config};
use crate::store::{key, Store};
use crate::widgets::{ButtonBackend, Region};
use crate::{dbg_ts, BUTTON_COLOR_ACTIVE, BUTTON_COLOR_INACTIVE, DEFAULT_ICON_SIZE};

pub(crate) enum ButtonImage {
    Text(String),
    Svg(Handle),
    Bitmap(ImageSurface),
}

/// Shared interactive button UI: the chrome box plus the transient press
/// highlight (`active`, governed by `sticky`). The behavior — what it draws and
/// what it emits — lives in the `backend`.
pub(crate) struct Button {
    pub(crate) changed: bool,
    pub(crate) active: bool,
    /// A local latch: when set, a press toggles `active` and it stays after
    /// release. Always false for now (no config yet) — preserving today's
    /// transient-only highlight.
    sticky: bool,
    backend: Box<dyn ButtonBackend>,
}

/// The default button behavior: render a static image and emit configured keys
/// on press/release, or open a modal layer instead of sending a key.
pub(crate) struct DapContinuePauseButton {
    continue_icon: ButtonImage,
    pause_icon: ButtonImage,
    icon_width: f64,
    icon_height: f64,
}

pub(crate) struct PiModelButton;
pub(crate) struct PiThinkingButton;
pub(crate) struct PiWorkflowModeButton {
    plan_icon: ButtonImage,
    build_icon: ButtonImage,
    icon_width: f64,
    icon_height: f64,
}

pub(crate) struct KeyButton {
    keys: Vec<Key>,
    content: ButtonImage,
    icon_width: f64,
    icon_height: f64,
    /// If set, pressing this button opens the named layer as a momentary modal
    /// (e.g. a slider) instead of sending its keys.
    open_layer: Option<String>,
    /// If set, pressing this button opens the named full-bar transient layer.
    push_layer: Option<String>,
    /// If set, pressing this button closes the current transient layer.
    pop_layer: bool,
    /// If set, pressing this button sends a one-shot action to the local nvim bridge.
    nvim_action: Option<String>,
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

impl DapContinuePauseButton {
    pub(crate) fn new() -> Result<DapContinuePauseButton> {
        let icon_width = DEFAULT_ICON_SIZE;
        let icon_height = DEFAULT_ICON_SIZE;
        Ok(DapContinuePauseButton {
            continue_icon: try_load_image("debug_continue", None::<&str>, icon_width, icon_height)?,
            pause_icon: try_load_image("debug_pause", None::<&str>, icon_width, icon_height)?,
            icon_width: icon_width as f64,
            icon_height: icon_height as f64,
        })
    }

    fn running(store: &Store) -> bool {
        store.text(key::NVIM_DAP_STATE).unwrap_or("") == "running"
    }

    fn render_icon(&self, c: &Context, r: &Region, icon: &ButtonImage) {
        match icon {
            ButtonImage::Svg(svg) => {
                let x = r.left + (r.width / 2.0 - self.icon_width / 2.0).round();
                let y = r.y_shift + ((r.height as f64 - self.icon_height) / 2.0).round();
                svg.render_document(c, &Rectangle::new(x, y, self.icon_width, self.icon_height))
                    .unwrap();
            }
            ButtonImage::Bitmap(surf) => {
                let x = r.left + (r.width / 2.0 - self.icon_width / 2.0).round();
                let y = r.y_shift + ((r.height as f64 - self.icon_height) / 2.0).round();
                c.set_source_surface(surf, x, y).unwrap();
                c.rectangle(x, y, self.icon_width, self.icon_height);
                c.fill().unwrap();
            }
            ButtonImage::Text(_) => {}
        }
    }
}

impl ButtonBackend for DapContinuePauseButton {
    fn draw_content(&self, c: &Context, r: &Region, store: &Store) {
        let icon = if Self::running(store) {
            &self.pause_icon
        } else {
            &self.continue_icon
        };
        self.render_icon(c, r, icon);
    }

    fn on_press(&mut self, store: &Store) -> Option<Action> {
        if Self::running(store) {
            Some(Action::NvimBridge("dap.pause".to_string()))
        } else {
            Some(Action::NvimBridge("dap.continue".to_string()))
        }
    }

    fn on_release(&mut self, _store: &Store) -> Option<Action> {
        None
    }

    fn needs_redraw(&self, store: &Store) -> bool {
        store.is_dirty(key::NVIM_DAP_STATE).unwrap_or(false)
    }
}

fn draw_centered_text(c: &Context, text: &str, height: i32, left: f64, width: f64, y_shift: f64) {
    let extents = c.text_extents(text).unwrap();
    c.move_to(
        left + (width / 2.0 - extents.width() / 2.0 - extents.x_bearing()).round(),
        y_shift + (height as f64 / 2.0 - extents.height() / 2.0 - extents.y_bearing()).round(),
    );
    c.show_text(text).unwrap();
}

impl ButtonBackend for PiModelButton {
    fn draw_content(&self, c: &Context, r: &Region, store: &Store) {
        let model = store.text(key::PI_MODEL).unwrap_or("");
        let label = if model.is_empty() {
            "Model".to_string()
        } else {
            format!("Model: {model}")
        };
        draw_centered_text(c, &label, r.height, r.left, r.width, r.y_shift);
    }

    fn on_press(&mut self, _store: &Store) -> Option<Action> {
        Some(Action::Key(vec![Key::LeftCtrl, Key::L], Edge::Press))
    }

    fn on_release(&mut self, _store: &Store) -> Option<Action> {
        Some(Action::Key(vec![Key::LeftCtrl, Key::L], Edge::Release))
    }

    fn needs_redraw(&self, store: &Store) -> bool {
        store.is_dirty(key::PI_MODEL).unwrap_or(false)
    }
}

impl ButtonBackend for PiThinkingButton {
    fn draw_content(&self, c: &Context, r: &Region, store: &Store) {
        let thinking = store.text(key::PI_THINKING).unwrap_or("");
        let label = if thinking.is_empty() {
            "Thinking".to_string()
        } else {
            format!("Thinking: {thinking}")
        };
        draw_centered_text(c, &label, r.height, r.left, r.width, r.y_shift);
    }

    fn on_press(&mut self, _store: &Store) -> Option<Action> {
        Some(Action::Key(vec![Key::LeftShift, Key::Tab], Edge::Press))
    }

    fn on_release(&mut self, _store: &Store) -> Option<Action> {
        Some(Action::Key(vec![Key::LeftShift, Key::Tab], Edge::Release))
    }

    fn needs_redraw(&self, store: &Store) -> bool {
        store.is_dirty(key::PI_THINKING).unwrap_or(false)
    }
}

impl PiWorkflowModeButton {
    pub(crate) fn new() -> Result<PiWorkflowModeButton> {
        let icon_width = 38;
        let icon_height = 38;
        Ok(PiWorkflowModeButton {
            plan_icon: try_load_image("pi_plan", None::<&str>, icon_width, icon_height)?,
            build_icon: try_load_image("pi_build", None::<&str>, icon_width, icon_height)?,
            icon_width: icon_width as f64,
            icon_height: icon_height as f64,
        })
    }

    fn draw_icon_text(&self, c: &Context, r: &Region, label: &str, icon: &ButtonImage) {
        let gap = 10.0;
        let extents = c.text_extents(label).unwrap();
        let total_width = self.icon_width + gap + extents.width();
        let icon_x = r.left + (r.width / 2.0 - total_width / 2.0).round();
        let icon_y = r.y_shift + ((r.height as f64 - self.icon_height) / 2.0).round();
        match icon {
            ButtonImage::Svg(svg) => {
                svg.render_document(
                    c,
                    &Rectangle::new(icon_x, icon_y, self.icon_width, self.icon_height),
                )
                .unwrap();
            }
            ButtonImage::Bitmap(surf) => {
                c.set_source_surface(surf, icon_x, icon_y).unwrap();
                c.rectangle(icon_x, icon_y, self.icon_width, self.icon_height);
                c.fill().unwrap();
            }
            ButtonImage::Text(_) => {}
        }
        let text_x = icon_x + self.icon_width + gap - extents.x_bearing();
        let text_y = r.y_shift
            + (r.height as f64 / 2.0 - extents.height() / 2.0 - extents.y_bearing()).round();
        c.move_to(text_x, text_y);
        c.show_text(label).unwrap();
    }
}

impl ButtonBackend for PiWorkflowModeButton {
    fn draw_content(&self, c: &Context, r: &Region, store: &Store) {
        let mode = store.text(key::PI_WORKFLOW_MODE).unwrap_or("");
        match mode {
            "plan" => {
                c.set_source_rgb(196.0 / 255.0, 181.0 / 255.0, 253.0 / 255.0);
                self.draw_icon_text(c, r, "Plan", &self.plan_icon);
            }
            "build" => {
                c.set_source_rgb(96.0 / 255.0, 165.0 / 255.0, 250.0 / 255.0);
                self.draw_icon_text(c, r, "Build", &self.build_icon);
            }
            _ => {
                c.set_source_rgb(1.0, 1.0, 1.0);
                draw_centered_text(c, "Plan / Build", r.height, r.left, r.width, r.y_shift);
            }
        }
    }

    fn on_press(&mut self, _store: &Store) -> Option<Action> {
        Some(Action::Key(
            vec![Key::LeftCtrl, Key::LeftShift, Key::M],
            Edge::Press,
        ))
    }

    fn on_release(&mut self, _store: &Store) -> Option<Action> {
        Some(Action::Key(
            vec![Key::LeftCtrl, Key::LeftShift, Key::M],
            Edge::Release,
        ))
    }

    fn needs_redraw(&self, store: &Store) -> bool {
        store.is_dirty(key::PI_WORKFLOW_MODE).unwrap_or(false)
    }
}

impl KeyButton {
    pub(crate) fn new_text(
        text: String,
        keys: Vec<Key>,
        open_layer: Option<String>,
        push_layer: Option<String>,
        pop_layer: bool,
    ) -> KeyButton {
        KeyButton {
            keys,
            content: ButtonImage::Text(text),
            icon_width: 0.0,
            icon_height: 0.0,
            open_layer,
            push_layer,
            pop_layer,
            nvim_action: None,
        }
    }
    #[allow(clippy::too_many_arguments)]
    fn new_icon(
        path: impl AsRef<str>,
        theme: Option<impl AsRef<str>>,
        keys: Vec<Key>,
        icon_width: i32,
        icon_height: i32,
        open_layer: Option<String>,
        push_layer: Option<String>,
        pop_layer: bool,
    ) -> Result<KeyButton> {
        let content = try_load_image(path, theme, icon_width, icon_height)?;
        Ok(KeyButton {
            keys,
            content,
            icon_width: icon_width as f64,
            icon_height: icon_height as f64,
            open_layer,
            push_layer,
            pop_layer,
            nvim_action: None,
        })
    }
    pub(crate) fn new_nvim_action_text(
        text: impl Into<String>,
        action: impl Into<String>,
    ) -> KeyButton {
        KeyButton {
            keys: Vec::new(),
            content: ButtonImage::Text(text.into()),
            icon_width: 0.0,
            icon_height: 0.0,
            open_layer: None,
            push_layer: None,
            pop_layer: false,
            nvim_action: Some(action.into()),
        }
    }

    pub(crate) fn new_nvim_action_icon(
        icon: impl AsRef<str>,
        action: impl Into<String>,
    ) -> Result<KeyButton> {
        let icon_width = DEFAULT_ICON_SIZE;
        let icon_height = DEFAULT_ICON_SIZE;
        Ok(KeyButton {
            keys: Vec::new(),
            content: try_load_image(icon, None::<&str>, icon_width, icon_height)?,
            icon_width: icon_width as f64,
            icon_height: icon_height as f64,
            open_layer: None,
            push_layer: None,
            pop_layer: false,
            nvim_action: Some(action.into()),
        })
    }

    fn render(
        &self,
        c: &Context,
        height: i32,
        button_left_edge: f64,
        button_width: u64,
        y_shift: f64,
    ) {
        match &self.content {
            ButtonImage::Text(text) => {
                c.save().unwrap();
                if text == "×" {
                    c.set_font_size(42.0);
                }
                let extents = c.text_extents(text).unwrap();
                c.move_to(
                    button_left_edge
                        + (button_width as f64 / 2.0 - extents.width() / 2.0 - extents.x_bearing())
                            .round(),
                    y_shift
                        + (height as f64 / 2.0 - extents.height() / 2.0 - extents.y_bearing())
                            .round(),
                );
                c.show_text(text).unwrap();
                c.restore().unwrap();
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
        }
    }
}

impl ButtonBackend for KeyButton {
    fn draw_content(&self, c: &Context, r: &Region, _store: &Store) {
        self.render(c, r.height, r.left, r.width.ceil() as u64, r.y_shift);
    }
    fn on_press(&mut self, _store: &Store) -> Option<Action> {
        if let Some(layer) = &self.open_layer {
            Some(Action::OpenModal(layer.clone()))
        } else if let Some(layer) = &self.push_layer {
            Some(Action::PushLayer(layer.clone()))
        } else if self.pop_layer {
            Some(Action::PopLayer)
        } else if let Some(action) = &self.nvim_action {
            Some(Action::NvimBridge(action.clone()))
        } else if self.keys.is_empty() {
            None
        } else {
            Some(Action::Key(self.keys.clone(), Edge::Press))
        }
    }
    fn on_release(&mut self, _store: &Store) -> Option<Action> {
        if self.open_layer.is_some()
            || self.push_layer.is_some()
            || self.pop_layer
            || self.nvim_action.is_some()
            || self.keys.is_empty()
        {
            None
        } else {
            Some(Action::Key(self.keys.clone(), Edge::Release))
        }
    }
}

impl Button {
    pub(crate) fn changed(&self, store: &Store) -> bool {
        self.changed || self.backend.needs_redraw(store)
    }

    pub(crate) fn new(backend: Box<dyn ButtonBackend>) -> Button {
        Button {
            changed: false,
            active: false,
            sticky: false,
            backend,
        }
    }
    /// Build an interactive button from its config. Only the text/icon cases
    /// live here; the indicator/battery-N/A/spacer dispatch is decided one level
    /// up in [`crate::widgets::Widget::from_config`].
    pub(crate) fn from_config(cfg: ButtonConfig) -> Result<Button> {
        let open_layer = cfg.open_layer.clone();
        let push_layer = cfg.push_layer.clone();
        let pop_layer = cfg.pop_layer.unwrap_or(false);
        let backend = if let Some(text) = cfg.text {
            KeyButton::new_text(text, cfg.action, open_layer, push_layer, pop_layer)
        } else {
            let icon = cfg
                .icon
                .ok_or_else(|| anyhow!("button config requires either text or icon"))?;
            KeyButton::new_icon(
                &icon,
                cfg.theme,
                cfg.action,
                cfg.icon_width.unwrap_or(DEFAULT_ICON_SIZE),
                cfg.icon_height.unwrap_or(DEFAULT_ICON_SIZE),
                open_layer,
                push_layer,
                pop_layer,
            )?
        };
        Ok(Button::new(Box::new(backend)))
    }

    /// Press edge: light the transient highlight (unless this hands off to a
    /// modal, which must not leave the trigger lit) and return the backend's
    /// effect. Idempotent — re-pressing an already-active button is a no-op, so a
    /// finger sliding within the button never re-emits.
    pub(crate) fn on_press(&mut self, store: &Store) -> Option<Action> {
        let action = self.backend.on_press(store);
        if matches!(
            action,
            Some(
                Action::OpenModal(_)
                    | Action::PushLayer(_)
                    | Action::PopLayer
                    | Action::NvimBridge(_)
                    | Action::NvimDbConnect(_),
            )
        ) {
            return action;
        }
        if self.active {
            return None;
        }
        self.active = true;
        self.changed = true;
        let key = match &action {
            Some(Action::Key(keys, _)) => keys.first().copied(),
            _ => None,
        };
        eprintln!("[dbg {:.6}] set_active=true key={:?}", dbg_ts(), key);
        action
    }

    /// Release edge: clear the highlight (unless `sticky`) and return the
    /// backend's effect. Only acts when the button was actually active, so it
    /// never emits a stray key-up after the finger has already left.
    pub(crate) fn on_release(&mut self, store: &Store) -> Option<Action> {
        if self.sticky || !self.active {
            return None;
        }
        self.active = false;
        self.changed = true;
        let action = self.backend.on_release(store);
        let key = match &action {
            Some(Action::Key(keys, _)) => keys.first().copied(),
            _ => None,
        };
        eprintln!("[dbg {:.6}] set_active=false key={:?}", dbg_ts(), key);
        action
    }

    fn set_background_color(&self, c: &Context, color: f64) {
        c.set_source_rgb(color, color, color);
    }
    /// Render this button into its slot: the rounded chrome box (active or
    /// outline coloured), then the content. Returns the damage rect on a partial
    /// redraw (empty on a complete one, which the layer already damages whole).
    /// The cairo context is already rotated by the layer.
    pub(crate) fn draw(
        &mut self,
        c: &Context,
        config: &Config,
        region: &Region,
        store: &Store,
        complete_redraw: bool,
    ) -> Result<Vec<ClipRect>> {
        let radius = 8.0f64;
        let bot = (region.height as f64) * 0.15;
        let top = (region.height as f64) * 0.85;
        let left_edge = region.left;
        let button_width = region.width;

        let color = if self.active {
            BUTTON_COLOR_ACTIVE
        } else if config.show_button_outlines {
            BUTTON_COLOR_INACTIVE
        } else {
            0.0
        };
        if !complete_redraw {
            c.set_source_rgb(0.0, 0.0, 0.0);
            c.rectangle(
                left_edge,
                bot - radius,
                button_width,
                top - bot + radius * 2.0,
            );
            c.fill().unwrap();
        }
        self.set_background_color(c, color);
        // draw box with rounded corners
        c.new_sub_path();
        let left = left_edge + radius;
        let right = (left_edge + button_width.ceil()) - radius;
        c.arc(
            right,
            bot,
            radius,
            (-90.0f64).to_radians(),
            (0.0f64).to_radians(),
        );
        c.arc(
            right,
            top,
            radius,
            (0.0f64).to_radians(),
            (90.0f64).to_radians(),
        );
        c.arc(
            left,
            top,
            radius,
            (90.0f64).to_radians(),
            (180.0f64).to_radians(),
        );
        c.arc(
            left,
            bot,
            radius,
            (180.0f64).to_radians(),
            (270.0f64).to_radians(),
        );
        c.close_path();
        c.fill().unwrap();
        c.set_source_rgb(1.0, 1.0, 1.0);
        self.backend.draw_content(c, region, store);
        self.changed = false;

        let clips = if complete_redraw {
            vec![]
        } else {
            vec![ClipRect::new(
                region.height as u16 - top as u16 - radius as u16,
                left_edge as u16,
                region.height as u16 - bot as u16 + radius as u16,
                left_edge as u16 + button_width as u16,
            )]
        };
        Ok(clips)
    }
}
