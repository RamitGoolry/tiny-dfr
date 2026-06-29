use anyhow::{anyhow, Result};
use cairo::{Context, Format, ImageSurface};
use chrono::{
    format::{Item as ChronoItem, StrftimeItems},
    Local, Locale,
};
use librsvg_rebind::{prelude::HandleExt, Handle, Rectangle};

use crate::battery::{get_battery_state, BatteryIconMode, BatteryImages, BatteryState};
use crate::state::State;
use crate::widgets::button::try_load_image;
use crate::widgets::{IndicatorBackend, Region};
use crate::DEFAULT_ICON_SIZE;

/// A passive clock readout. Renders white; the App drives its redraw via the
/// minute/second boundary (see the main loop's `displays_time` complete-redraw),
/// so `needs_redraw` stays false to avoid a redundant partial repaint.
pub(crate) struct ClockIndicator {
    format: Vec<ChronoItem<'static>>,
    locale: Locale,
}

impl ClockIndicator {
    pub(crate) fn new(format: &str, locale_str: Option<&str>) -> Result<ClockIndicator> {
        let format_str = if format == "24hr" {
            "%H:%M    %a %-e %b"
        } else if format == "12hr" {
            "%-l:%M %p    %a %-e %b"
        } else {
            format
        };

        let format_items = StrftimeItems::new(format_str).parse_to_owned().map_err(|e| {
            anyhow!("invalid time format '{format}' (see the config file for valid examples): {e:?}")
        })?;

        let locale = locale_str
            .and_then(|l| Locale::try_from(l).ok())
            .unwrap_or(Locale::POSIX);
        Ok(ClockIndicator {
            format: format_items,
            locale,
        })
    }
}

impl IndicatorBackend for ClockIndicator {
    fn draw_content(&self, c: &Context, r: &Region, _s: &State) {
        c.set_source_rgb(1.0, 1.0, 1.0);
        let current_time = Local::now();
        let formatted_time = current_time
            .format_localized_with_items(self.format.iter(), self.locale)
            .to_string();
        let time_extents = c.text_extents(&formatted_time).unwrap();
        c.move_to(
            r.left + (r.width / 2.0 - time_extents.width() / 2.0).round(),
            r.y_shift + (r.height as f64 / 2.0 + time_extents.height() / 2.0).round(),
        );
        c.show_text(&formatted_time).unwrap();
    }
    fn needs_redraw(&self, _s: &State) -> bool {
        false
    }
    fn is_clock(&self) -> bool {
        true
    }
    fn needs_faster_refresh(&self) -> bool {
        self.format.iter().any(|item| {
            use chrono::format::{Item, Numeric};
            matches!(
                item,
                Item::Numeric(Numeric::Second, _)
                    | Item::Numeric(Numeric::Nanosecond, _)
                    | Item::Numeric(Numeric::Timestamp, _)
            )
        })
    }
}

/// A passive battery readout, tinted by charge state. Polled, not event-driven,
/// so `needs_redraw` is always true: on a battery layer the cell repaints every
/// loop iteration (the same cadence as today's per-iteration mark).
pub(crate) struct BatteryIndicator {
    device: String,
    mode: BatteryIconMode,
    images: BatteryImages,
}

impl BatteryIndicator {
    fn load_battery_image(icon: &str, theme: Option<impl AsRef<str>>) -> Result<Handle> {
        match try_load_image(icon, theme, DEFAULT_ICON_SIZE, DEFAULT_ICON_SIZE)? {
            crate::widgets::button::ButtonImage::Svg(svg) => Ok(svg),
            _ => Err(anyhow!("battery icon '{icon}' is not an SVG")),
        }
    }

    pub(crate) fn new(
        battery: String,
        battery_mode: String,
        theme: Option<impl AsRef<str>>,
    ) -> Result<BatteryIndicator> {
        let bolt = Self::load_battery_image("bolt", theme.as_ref())?;
        let mut plain = Vec::new();
        let mut charging = Vec::new();
        for icon in [
            "battery_0_bar",
            "battery_1_bar",
            "battery_2_bar",
            "battery_3_bar",
            "battery_4_bar",
            "battery_5_bar",
            "battery_6_bar",
            "battery_full",
        ] {
            plain.push(Self::load_battery_image(icon, theme.as_ref())?);
        }
        for icon in [
            "battery_charging_20",
            "battery_charging_30",
            "battery_charging_50",
            "battery_charging_60",
            "battery_charging_80",
            "battery_charging_90",
            "battery_charging_full",
        ] {
            charging.push(Self::load_battery_image(icon, theme.as_ref())?);
        }
        let battery_mode = match battery_mode.as_str() {
            "icon" => BatteryIconMode::Icon,
            "percentage" => BatteryIconMode::Percentage,
            "both" => BatteryIconMode::Both,
            other => {
                return Err(anyhow!(
                    "invalid battery mode '{other}', accepted modes: icon, percentage, both"
                ))
            }
        };
        Ok(BatteryIndicator {
            device: battery,
            mode: battery_mode,
            images: BatteryImages {
                plain,
                bolt,
                charging,
            },
        })
    }

    /// Set the cairo source to the charge-state colour — green charging, red low,
    /// white otherwise — so the readout still conveys charge state now that there
    /// is no coloured chrome box behind it.
    fn set_content_color(&self, c: &Context) {
        match get_battery_state(&self.device).1 {
            BatteryState::Charging => c.set_source_rgb(0.25, 0.85, 0.35),
            BatteryState::Low => c.set_source_rgb(0.95, 0.25, 0.25),
            BatteryState::NotCharging => c.set_source_rgb(1.0, 1.0, 1.0),
        }
    }
}

impl IndicatorBackend for BatteryIndicator {
    fn draw_content(&self, c: &Context, r: &Region, _s: &State) {
        self.set_content_color(c);
        let height = r.height;
        let button_left_edge = r.left;
        let button_width = r.width.ceil() as u64;
        let y_shift = r.y_shift;

        let (capacity, state) = get_battery_state(&self.device);
        let icons = &self.images;
        let icon = if self.mode.should_draw_icon() {
            Some(match state {
                BatteryState::Charging => match capacity {
                    0..=20 => &icons.charging[0],
                    21..=30 => &icons.charging[1],
                    31..=50 => &icons.charging[2],
                    51..=60 => &icons.charging[3],
                    61..=80 => &icons.charging[4],
                    81..=99 => &icons.charging[5],
                    _ => &icons.charging[6],
                },
                _ => match capacity {
                    0 => &icons.plain[0],
                    1..=20 => &icons.plain[1],
                    21..=30 => &icons.plain[2],
                    31..=50 => &icons.plain[3],
                    51..=60 => &icons.plain[4],
                    61..=80 => &icons.plain[5],
                    81..=99 => &icons.plain[6],
                    _ => &icons.plain[7],
                },
            })
        } else if state == BatteryState::Charging {
            Some(&icons.bolt)
        } else {
            None
        };
        let percent_str = format!("{:.0}%", capacity);
        let extents = c.text_extents(&percent_str).unwrap();
        let mut width = extents.width();
        let mut text_offset = 0;
        if let Some(svg) = icon {
            if !self.mode.should_draw_text() {
                width = DEFAULT_ICON_SIZE as f64;
            } else {
                width += DEFAULT_ICON_SIZE as f64;
            }
            text_offset = DEFAULT_ICON_SIZE;
            let x = button_left_edge + (button_width as f64 / 2.0 - width / 2.0).round();
            let y = y_shift + ((height as f64 - DEFAULT_ICON_SIZE as f64) / 2.0).round();

            // librsvg ignores the cairo source colour, so render the glyph to a
            // stencil and paint the current source (the charge colour) through
            // its alpha. Doing the SVG render on a separate context also keeps it
            // from resetting `c`'s source, so the % text below stays tinted too.
            let stencil =
                ImageSurface::create(Format::ARgb32, DEFAULT_ICON_SIZE, DEFAULT_ICON_SIZE).unwrap();
            svg.render_document(
                &Context::new(&stencil).unwrap(),
                &Rectangle::new(0.0, 0.0, DEFAULT_ICON_SIZE as f64, DEFAULT_ICON_SIZE as f64),
            )
            .unwrap();
            c.mask_surface(&stencil, x, y).unwrap();
        }
        if self.mode.should_draw_text() {
            c.move_to(
                button_left_edge
                    + (button_width as f64 / 2.0 - width / 2.0 + text_offset as f64).round(),
                y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
            );
            c.show_text(&percent_str).unwrap();
        }
    }
    fn needs_redraw(&self, _s: &State) -> bool {
        true
    }
}
