//! Touch Bar widgets. Each widget kind lives in its own module; this root will
//! grow the `Widget` enum and its lifecycle dispatch (on_press/on_release/
//! on_drag -> Action, draw) as the composable model lands. For now it just hosts
//! and re-exports the widget types.
pub(crate) mod button;
pub(crate) mod indicator;
pub(crate) mod slider;

pub(crate) use button::{Button, ButtonImage};
pub(crate) use slider::{Slider, SliderBackend};

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
