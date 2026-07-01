//! Generic reactive state store for tiny-dfr runtime facts.
//!
//! The store is keyed by validated dotted [`StateKey`]s and keeps key-level dirty
//! tracking. Widgets and app code use typed accessors so the rest of the codebase
//! does not manually pattern-match [`Value`] everywhere.
use anyhow::{anyhow, Result};
use std::{
    collections::{HashMap, HashSet},
    fmt,
};

#[allow(dead_code)]
pub(crate) mod key {
    pub(crate) const HARDWARE_BRIGHTNESS: &str = "hardware.brightness";
    pub(crate) const HARDWARE_KBD_ILLUM: &str = "hardware.kbd_illum";
    pub(crate) const AUDIO_VOLUME: &str = "audio.volume";
    pub(crate) const CONTEXT_FOCUS_CLASS: &str = "context.focus.class";
    pub(crate) const CONTEXT_FOCUS_TITLE: &str = "context.focus.title";
    pub(crate) const MEDIA_ACTIVE_PLAYER: &str = "media.active.player";
    pub(crate) const MEDIA_ACTIVE_TRACK_ID: &str = "media.active.track_id";
    pub(crate) const MEDIA_ACTIVE_STATUS: &str = "media.active.status";
    pub(crate) const MEDIA_ACTIVE_TITLE: &str = "media.active.title";
    pub(crate) const MEDIA_ACTIVE_ART_URL: &str = "media.active.art_url";
    pub(crate) const MEDIA_ACTIVE_LENGTH: &str = "media.active.length";
    pub(crate) const MEDIA_ACTIVE_POSITION: &str = "media.active.position";
    pub(crate) const CHROMIUM_MEDIA_ACTIVE: &str = "chromium.media.active";
    pub(crate) const CHROMIUM_TABS_AVAILABLE: &str = "chromium.tabs.available";
    pub(crate) const CHROMIUM_TABS_JSON: &str = "chromium.tabs.json";
    pub(crate) const CHROMIUM_TABS_COUNT: &str = "chromium.tabs.count";
    pub(crate) const CHROMIUM_TABS_ACTIVE_INDEX: &str = "chromium.tabs.active_index";
    pub(crate) const NVIM_BRIDGE_AVAILABLE: &str = "nvim.bridge.available";
    pub(crate) const NVIM_BRIDGE_SOCKET: &str = "nvim.bridge.socket";
    pub(crate) const NVIM_PID: &str = "nvim.pid";
    pub(crate) const NVIM_CWD: &str = "nvim.cwd";
    pub(crate) const NVIM_FILE: &str = "nvim.file";
    pub(crate) const NVIM_FILETYPE: &str = "nvim.filetype";
    pub(crate) const NVIM_MODE: &str = "nvim.mode";
    pub(crate) const NVIM_DAP_ACTIVE: &str = "nvim.dap.active";
    pub(crate) const NVIM_DAP_STATE: &str = "nvim.dap.state";
    pub(crate) const NVIM_DAP_ADAPTER: &str = "nvim.dap.adapter";
    pub(crate) const NVIM_DAP_SESSION: &str = "nvim.dap.session";
    pub(crate) const NVIM_DAPUI_AVAILABLE: &str = "nvim.dapui.available";
    pub(crate) const NVIM_DAPUI_VISIBLE: &str = "nvim.dapui.visible";
    pub(crate) const NVIM_NEOTEST_AVAILABLE: &str = "nvim.neotest.available";
    pub(crate) const NVIM_NEOTEST_SUMMARY_VISIBLE: &str = "nvim.neotest.summary_visible";
    pub(crate) const NVIM_NEOTEST_UPDATED_AT_MS: &str = "nvim.neotest.updated_at_ms";
    pub(crate) const NVIM_DBUI_AVAILABLE: &str = "nvim.dbui.available";
    pub(crate) const NVIM_DBUI_VISIBLE: &str = "nvim.dbui.visible";
    pub(crate) const NVIM_DBUI_IN_BUFFER: &str = "nvim.dbui.in_buffer";
    pub(crate) const NVIM_DBUI_CONNECTIONS_JSON: &str = "nvim.dbui.connections_json";
    pub(crate) const PI_MODEL: &str = "pi.model";
    pub(crate) const PI_PROVIDER: &str = "pi.provider";
    pub(crate) const PI_THINKING: &str = "pi.thinking";
    pub(crate) const PI_WORKFLOW_MODE: &str = "pi.workflow.mode";
    pub(crate) const TERMINAL_AVAILABLE: &str = "terminal.available";
    pub(crate) const TERMINAL_NAME: &str = "terminal.name";
    pub(crate) const TERMINAL_KIND: &str = "terminal.kind";
    pub(crate) const TERMINAL_ROOT_PID: &str = "terminal.root_pid";
    pub(crate) const TERMINAL_COMMAND: &str = "terminal.command";
    pub(crate) const TERMINAL_CWD: &str = "terminal.cwd";
    pub(crate) const TERMINAL_APP: &str = "terminal.app";
    pub(crate) const TERMINAL_APP_PID: &str = "terminal.app_pid";
    pub(crate) const TERMINAL_NVIM_PID: &str = "terminal.nvim_pid";
    pub(crate) const TERMINAL_TMUX_CLIENT_PID: &str = "terminal.tmux.client_pid";
    pub(crate) const TERMINAL_TMUX_CLIENT_TTY: &str = "terminal.tmux.client_tty";
    pub(crate) const TERMINAL_TMUX_SESSION: &str = "terminal.tmux.session";
    pub(crate) const TERMINAL_TMUX_WINDOW: &str = "terminal.tmux.window";
    pub(crate) const TERMINAL_TMUX_PANE_INDEX: &str = "terminal.tmux.pane_index";
    pub(crate) const TERMINAL_TMUX_PANE_ID: &str = "terminal.tmux.pane_id";
    pub(crate) const TERMINAL_TMUX_PANE_PID: &str = "terminal.tmux.pane_pid";
    pub(crate) const TERMINAL_TMUX_PANE_COMMAND: &str = "terminal.tmux.pane_command";
    pub(crate) const TERMINAL_TMUX_PANE_PATH: &str = "terminal.tmux.pane_path";
    pub(crate) const TERMINAL_TMUX_PANE_TITLE: &str = "terminal.tmux.pane_title";
}

/// Validated dotted state key, e.g. `hardware.brightness`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct StateKey(String);

impl StateKey {
    pub(crate) fn new(key: impl Into<String>) -> Result<StateKey> {
        let key = key.into();
        validate_key(&key)?;
        Ok(StateKey(key))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for StateKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl AsRef<str> for StateKey {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl TryFrom<&str> for StateKey {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self> {
        StateKey::new(value)
    }
}

impl TryFrom<String> for StateKey {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self> {
        StateKey::new(value)
    }
}

fn validate_key(key: &str) -> Result<()> {
    if key.is_empty() {
        return Err(anyhow!("state key must not be empty"));
    }
    for segment in key.split('.') {
        if segment.is_empty() {
            return Err(anyhow!("state key `{key}` contains an empty segment"));
        }
        if !segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(anyhow!(
                "state key `{key}` contains invalid segment `{segment}`"
            ));
        }
    }
    Ok(())
}

/// Scalar runtime value. Composite JSON-like values can be added later when a
/// source/widget needs them; for now sources should extract scalar fields into
/// dotted keys.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Value {
    Bool(bool),
    Number(f64),
    Text(String),
}

#[derive(Default)]
pub(crate) struct Store {
    values: HashMap<StateKey, Value>,
    dirty: HashSet<StateKey>,
}

impl Store {
    pub(crate) fn new() -> Store {
        Store::default()
    }

    pub(crate) fn set(&mut self, key: impl TryInto<StateKey>, value: Value) -> Result<()> {
        let key = key.try_into().map_err(|_| anyhow!("invalid state key"))?;
        if self.values.get(&key) != Some(&value) {
            self.values.insert(key.clone(), value);
            self.dirty.insert(key);
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn value(&self, key: &str) -> Result<&Value> {
        let key = StateKey::new(key)?;
        self.values
            .get(&key)
            .ok_or_else(|| anyhow!("missing Store key `{key}`"))
    }

    #[allow(dead_code)]
    pub(crate) fn bool(&self, key: &str) -> Result<bool> {
        match self.value(key)? {
            Value::Bool(value) => Ok(*value),
            other => Err(anyhow!("Store key `{key}` expected bool, found {other:?}")),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn number(&self, key: &str) -> Result<f64> {
        match self.value(key)? {
            Value::Number(value) => Ok(*value),
            other => Err(anyhow!(
                "Store key `{key}` expected number, found {other:?}"
            )),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn text(&self, key: &str) -> Result<&str> {
        match self.value(key)? {
            Value::Text(value) => Ok(value),
            other => Err(anyhow!("Store key `{key}` expected text, found {other:?}")),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn is_dirty(&self, key: &str) -> Result<bool> {
        let key = StateKey::new(key)?;
        Ok(self.dirty.contains(&key))
    }

    #[allow(dead_code)]
    pub(crate) fn dirty_keys(&self) -> impl Iterator<Item = &StateKey> {
        self.dirty.iter()
    }

    pub(crate) fn clear_dirty(&mut self) {
        self.dirty.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_state_keys() {
        for key in [
            "hardware.brightness",
            "context.focus.class",
            "ci-build.status_1",
            "A.B-c_1",
        ] {
            assert!(StateKey::new(key).is_ok(), "{key} should be valid");
        }

        for key in [
            "",
            ".leading",
            "trailing.",
            "double..dot",
            "has space",
            "slash/key",
        ] {
            assert!(StateKey::new(key).is_err(), "{key} should be invalid");
        }
    }

    #[test]
    fn set_and_get_typed_values() {
        let mut store = Store::new();
        store
            .set("hardware.brightness", Value::Number(0.5))
            .unwrap();
        store
            .set("context.focus.class", Value::Text("kitty".into()))
            .unwrap();
        store.set("feature.enabled", Value::Bool(true)).unwrap();

        assert_eq!(store.number("hardware.brightness").unwrap(), 0.5);
        assert_eq!(store.text("context.focus.class").unwrap(), "kitty");
        assert!(store.bool("feature.enabled").unwrap());
        assert!(store.number("context.focus.class").is_err());
        assert!(store.text("missing.key").is_err());
    }

    #[test]
    fn tracks_dirty_keys_until_cleared() {
        let mut store = Store::new();
        store.set("audio.volume", Value::Number(0.25)).unwrap();
        assert!(store.is_dirty("audio.volume").unwrap());
        assert_eq!(store.dirty_keys().count(), 1);

        store.clear_dirty();
        assert!(!store.is_dirty("audio.volume").unwrap());

        store.set("audio.volume", Value::Number(0.25)).unwrap();
        assert!(!store.is_dirty("audio.volume").unwrap());

        store.set("audio.volume", Value::Number(0.5)).unwrap();
        assert!(store.is_dirty("audio.volume").unwrap());
    }
}
