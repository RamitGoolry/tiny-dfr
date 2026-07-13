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
    /// Set the default PipeWire sink volume to `level` (0.0..=1.0).
    SetVolume(f64),
    /// Send the active MPRIS player to the previous track.
    MediaPrevious,
    /// Toggle play/pause on the active MPRIS player.
    MediaPlayPause,
    /// Send the active MPRIS player to the next track.
    MediaNext,
    /// Seek the active MPRIS player to an absolute position in microseconds.
    MediaSeek(f64),
    /// Activate a browser tab through the currently selected protocol backend.
    BrowserActivateTab(String),
    /// Launch an app command through the compositor.
    LaunchApp(String),
    /// Send a JSONL action to the selected local Neovim tiny-dfr bridge.
    NvimBridge(String),
    /// Connect/open a DBUI query buffer for the configured DB key.
    NvimDbConnect(String),
    /// Send a protocol action to the selected app bridge, with optional key fallback.
    AppBridge {
        app: String,
        action: String,
        fallback_keys: Vec<Key>,
        edge: Edge,
    },
    /// Open a named layer as a full-bar transient overlay.
    PushLayer(String),
    /// Close the current transient layer and return to the previous bar state.
    PopLayer,
    /// Enter the named layer as a momentary modal (e.g. a slider).
    OpenModal(String),
    /// Leave the current modal layer.
    CloseModal,
}
