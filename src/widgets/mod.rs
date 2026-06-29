//! Touch Bar widgets. Each widget kind lives in its own module; this root hosts
//! the `Widget` enum and its lifecycle dispatch (draw, hit-test eligibility,
//! change tracking) over the button-row cells. The `Slider` is still driven via
//! `Layer::Slider` and is not part of this enum yet.
pub(crate) mod button;
pub(crate) mod indicator;
pub(crate) mod slider;

pub(crate) use button::Button;
pub(crate) use slider::{Slider, SliderBackend};

use cairo::Context;
use drm::control::ClipRect;

use crate::battery::find_battery_device;
use crate::config::{ButtonConfig, Config};
use crate::widgets::indicator::Indicator;

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

/// A widget occupying a cell in the button row. `Button` is interactive and
/// drawn with chrome; `Indicator` (clock/battery) and `Spacer` are passive:
/// not hit-testable, no active highlight, no chrome box.
pub(crate) enum Widget {
    Button(Button),
    Indicator(Indicator),
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
            Widget::Indicator(Indicator::new_clock(&time, cfg.locale.as_deref()))
        } else if let Some(battery_mode) = cfg.battery {
            if let Some(battery) = find_battery_device() {
                Widget::Indicator(Indicator::new_battery(battery, battery_mode, cfg.theme))
            } else {
                let mut button = Button::new_text("Battery N/A".to_string(), cfg.action);
                button.open_layer = cfg.open_layer.clone();
                Widget::Button(button)
            }
        } else {
            Widget::Spacer
        }
    }

    /// Whether this widget changed since the last draw (always false for a
    /// passive `Spacer`).
    pub(crate) fn changed(&self) -> bool {
        match self {
            Widget::Button(b) => b.changed,
            Widget::Indicator(i) => i.changed,
            Widget::Spacer => false,
        }
    }

    /// Whether a touch landing on this widget should be dispatched to it. Only
    /// real buttons are interactive; indicators and spacers are passive.
    pub(crate) fn interactive(&self) -> bool {
        matches!(self, Widget::Button(_))
    }

    pub(crate) fn draw(
        &mut self,
        c: &Context,
        cfg: &Config,
        region: &Region,
        complete_redraw: bool,
    ) -> Vec<ClipRect> {
        match self {
            Widget::Button(b) => b.draw(c, cfg, region, complete_redraw),
            Widget::Indicator(i) => i.draw(c, cfg, region, complete_redraw),
            Widget::Spacer => vec![],
        }
    }

    pub(crate) fn needs_faster_refresh(&self) -> bool {
        match self {
            Widget::Indicator(i) => i.needs_faster_refresh(),
            _ => false,
        }
    }

    pub(crate) fn is_clock(&self) -> bool {
        matches!(self, Widget::Indicator(i) if i.is_clock())
    }

    pub(crate) fn is_battery(&self) -> bool {
        matches!(self, Widget::Indicator(i) if i.is_battery())
    }

    /// Mark the underlying widget as changed so the next draw repaints it.
    pub(crate) fn mark_changed(&mut self) {
        match self {
            Widget::Button(b) => b.changed = true,
            Widget::Indicator(i) => i.changed = true,
            Widget::Spacer => {}
        }
    }
}
