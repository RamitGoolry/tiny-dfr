//! The inbound half of the widget model: the world the App owns, observed into a
//! plain struct each iteration. Widgets render as a pure function of it and never
//! mutate it. Thin for now (just the backlight level the brightness slider reads);
//! later features add fields fed by their own sources.
#[derive(Default)]
pub(crate) struct State {
    /// Current display backlight level, 0.0..=1.0.
    pub(crate) brightness: f64,
    /// Current keyboard backlight level, 0.0..=1.0.
    pub(crate) kbd_illum: f64,
}
