use serde::Deserialize;
use std::{env, fs, path::PathBuf};

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct PiState {
    pub(crate) pid: u32,
    pub(crate) model: Option<String>,
    pub(crate) provider: Option<String>,
    pub(crate) thinking: Option<String>,
    pub(crate) workflow_mode: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) updated_at_ms: Option<i64>,
}

#[derive(Debug, Default)]
pub(crate) struct PiStateClient {
    states: Vec<PiState>,
}

impl PiStateClient {
    pub(crate) fn new() -> PiStateClient {
        PiStateClient::default()
    }

    pub(crate) fn refresh(
        &mut self,
        preferred_pid: Option<u32>,
        preferred_cwd: Option<&str>,
    ) -> Option<PiState> {
        self.states = discover_states();
        select_state(&self.states, preferred_pid, preferred_cwd).cloned()
    }
}

fn select_state<'a>(
    states: &'a [PiState],
    preferred_pid: Option<u32>,
    preferred_cwd: Option<&str>,
) -> Option<&'a PiState> {
    if let Some(pid) = preferred_pid {
        if let Some(state) = states.iter().find(|state| state.pid == pid) {
            return Some(state);
        }
    }
    if let Some(cwd) = preferred_cwd.filter(|cwd| !cwd.is_empty()) {
        if let Some(state) = states
            .iter()
            .find(|state| state.cwd.as_deref() == Some(cwd))
        {
            return Some(state);
        }
    }
    states
        .iter()
        .max_by_key(|state| state.updated_at_ms.unwrap_or(0))
}

fn discover_states() -> Vec<PiState> {
    let dir = runtime_root().join("pi");
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                return None;
            }
            let raw = fs::read_to_string(path).ok()?;
            serde_json::from_str::<PiState>(&raw).ok()
        })
        .collect()
}

fn runtime_root() -> PathBuf {
    if let Some(dir) = env::var_os("TINY_DFR_RUNTIME_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("tiny-dfr");
    }
    let user = env::var("USER").unwrap_or_else(|_| unsafe { libc::geteuid() }.to_string());
    PathBuf::from(format!("/tmp/tiny-dfr-{user}"))
}
