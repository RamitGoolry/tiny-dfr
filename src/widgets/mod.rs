//! Touch Bar widgets. Each widget kind lives in its own module; this root will
//! grow the `Widget` enum and its lifecycle dispatch (on_press/on_release/
//! on_drag -> Action, draw) as the composable model lands. For now it just hosts
//! and re-exports the widget types.
pub(crate) mod button;
pub(crate) mod indicator;
pub(crate) mod slider;

pub(crate) use button::{Button, ButtonImage};
pub(crate) use slider::{Slider, SliderBackend};
