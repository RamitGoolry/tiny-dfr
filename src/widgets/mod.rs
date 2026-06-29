//! Touch Bar widgets. Each widget kind lives in its own module; this root hosts
//! the `Widget` enum (the *frame* — kind decides chrome / hit-eligibility) and
//! the three backend traits (the *fill* — behavior within a kind). Every backend
//! reads a shared `&State` to render and can only return an `Action` to act, so
//! the App stays the single effectful place.
pub(crate) mod button;
pub(crate) mod indicator;
pub(crate) mod slider;

pub(crate) use button::Button;
pub(crate) use slider::{BrightnessSlider, Slider};

use cairo::Context;
use drm::control::ClipRect;

use crate::action::Action;
use crate::battery::find_battery_device;
use crate::config::{ButtonConfig, Config};
use crate::state::State;
use crate::widgets::button::KeyButton;
use crate::widgets::indicator::{BatteryIndicator, ClockIndicator};

/// The slot a widget occupies in the (already-rotated) draw space, computed by
/// the layer from each cell's stretch. Widgets draw into it and never compute
/// their own geometry.
pub(crate) struct Region {
    /// Left edge along the bar's long axis (rotated x).
    pub(crate) left: f64,
    /// Width along the long axis.
    pub(crate) width: f64,
    /// Full bar height (the short axis).
    pub(crate) height: i32,
    /// Pixel-shift y offset applied to content.
    pub(crate) y_shift: f64,
}

/// A backend that fills a [`Button`]: draws its content as a function of `&State`
/// and turns press/release edges into `Action`s. The `Button` struct owns the
/// shared interaction UI (the highlight); the backend owns only behavior.
pub(crate) trait ButtonBackend {
    fn draw_content(&self, c: &Context, r: &Region, s: &State);
    fn on_press(&mut self, s: &State) -> Option<Action>;
    fn on_release(&mut self, s: &State) -> Option<Action>;
}

/// A passive widget: renders content as a function of `&State` and owns its own
/// redraw cadence (clock-by-tick, battery-by-poll). No interaction UI.
pub(crate) trait IndicatorBackend {
    fn draw_content(&self, c: &Context, r: &Region, s: &State);
    fn needs_redraw(&self, s: &State) -> bool;
    fn is_clock(&self) -> bool {
        false
    }
    fn needs_faster_refresh(&self) -> bool {
        false
    }
}

/// The effect a [`Slider`] drives. Every slider grabs/drags/throttles/draws
/// identically (the `Slider` struct), so the backend is just the effect: where
/// the level goes, and where its initial value is read from `&State` on grab.
pub(crate) trait SliderBackend {
    fn apply(&self, level: f64) -> Action;
    fn initial_level(&self, s: &State) -> f64;
}

/// A widget occupying a cell. `Button`/`Slider` are interactive and own shared
/// interaction UI; `Indicator` (clock/battery) and `Spacer` are passive.
pub(crate) enum Widget {
    Button(Button),
    Slider(Slider),
    Indicator(Box<dyn IndicatorBackend>),
    Spacer,
}

/// One slot in the button row: how many virtual columns it spans (`stretch`)
/// plus the widget that fills it.
pub(crate) struct Cell {
    pub(crate) stretch: usize,
    pub(crate) widget: Widget,
}

impl Widget {
    /// Decide the widget variant from a button's config. Text/icon become an
    /// interactive `Button`; `Time` a clock indicator; `Battery` a battery
    /// indicator (falling back to a "Battery N/A" button when no device exists);
    /// otherwise an empty `Spacer`.
    pub(crate) fn from_config(cfg: ButtonConfig) -> Widget {
        if cfg.text.is_some() || cfg.icon.is_some() {
            Widget::Button(Button::from_config(cfg))
        } else if let Some(time) = cfg.time {
            Widget::Indicator(Box::new(ClockIndicator::new(&time, cfg.locale.as_deref())))
        } else if let Some(battery_mode) = cfg.battery {
            if let Some(battery) = find_battery_device() {
                Widget::Indicator(Box::new(BatteryIndicator::new(battery, battery_mode, cfg.theme)))
            } else {
                let backend = KeyButton::new_text(
                    "Battery N/A".to_string(),
                    cfg.action,
                    cfg.open_layer.clone(),
                );
                Widget::Button(Button::new(Box::new(backend)))
            }
        } else {
            Widget::Spacer
        }
    }

    /// Whether this widget changed since the last draw (always false for a
    /// passive `Spacer`). Indicators own their cadence via `needs_redraw`.
    pub(crate) fn changed(&self, s: &State) -> bool {
        match self {
            Widget::Button(b) => b.changed,
            Widget::Slider(sl) => sl.changed,
            Widget::Indicator(i) => i.needs_redraw(s),
            Widget::Spacer => false,
        }
    }

    /// Whether a touch landing on this widget should be dispatched to it. Buttons
    /// and sliders are interactive; indicators and spacers are passive.
    pub(crate) fn interactive(&self) -> bool {
        matches!(self, Widget::Button(_) | Widget::Slider(_))
    }

    pub(crate) fn draw(
        &mut self,
        c: &Context,
        cfg: &Config,
        region: &Region,
        state: &State,
        width: i32,
        complete_redraw: bool,
    ) -> Vec<ClipRect> {
        match self {
            Widget::Button(b) => b.draw(c, cfg, region, state, complete_redraw),
            Widget::Slider(s) => s.draw(c, width, region.height, complete_redraw),
            Widget::Indicator(b) => draw_indicator(c, b.as_ref(), region, state, complete_redraw),
            Widget::Spacer => vec![],
        }
    }

    pub(crate) fn needs_faster_refresh(&self) -> bool {
        matches!(self, Widget::Indicator(i) if i.needs_faster_refresh())
    }

    pub(crate) fn is_clock(&self) -> bool {
        matches!(self, Widget::Indicator(i) if i.is_clock())
    }
}

/// Render a passive indicator into its slot: no chrome box, no active highlight.
/// On a partial redraw it clears the same cell extent a `Button` would (so damage
/// lines up) before painting the content, then returns the matching `ClipRect`;
/// on a complete redraw the layer already cleared and damaged the whole bar.
fn draw_indicator(
    c: &Context,
    backend: &dyn IndicatorBackend,
    region: &Region,
    state: &State,
    complete_redraw: bool,
) -> Vec<ClipRect> {
    let radius = 8.0f64;
    let bot = (region.height as f64) * 0.15;
    let top = (region.height as f64) * 0.85;
    let left_edge = region.left;
    let button_width = region.width;

    if !complete_redraw {
        c.set_source_rgb(0.0, 0.0, 0.0);
        c.rectangle(left_edge, bot - radius, button_width, top - bot + radius * 2.0);
        c.fill().unwrap();
    }
    backend.draw_content(c, region, state);

    if complete_redraw {
        vec![]
    } else {
        vec![ClipRect::new(
            region.height as u16 - top as u16 - radius as u16,
            left_edge as u16,
            region.height as u16 - bot as u16 + radius as u16,
            left_edge as u16 + button_width as u16,
        )]
    }
}
