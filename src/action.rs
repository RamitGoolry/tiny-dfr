//! The outbound half of the widget model: a touch produces an `Action` that the
//! App applies against the world (emit a key, set brightness, open/close a modal
//! layer). Widgets are fire-and-forget — a backend can only *return* an `Action`,
//! so the App stays the single effectful place.
use input_linux::Key;

/// Whether a key event is the press (down) or release (up) edge.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Edge {
    Press,
    Release,
}

/// The effect a widget asks the App to perform. Applied centrally in `apply`.
pub(crate) enum Action {
    /// Emit the given keys at the given edge.
    Key(Vec<Key>, Edge),
    /// Set the display backlight to `level` (0.0..=1.0).
    SetBrightness(f64),
    /// Set the keyboard backlight to `level` (0.0..=1.0).
    SetKbdIllum(f64),
    /// Set the system (ALSA Master) volume to `level` (0.0..=1.0).
    SetVolume(f64),
    /// Enter the named layer as a momentary modal (e.g. a slider).
    OpenModal(String),
    /// Leave the current modal layer.
    CloseModal,
}
