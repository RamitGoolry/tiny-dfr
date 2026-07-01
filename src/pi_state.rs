use crate::app_bridge::{self, AppBridgeState};
use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::{Map, Value as JsonValue};

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct PiState {
    pub(crate) pid: u32,
    pub(crate) model: Option<String>,
    pub(crate) provider: Option<String>,
    pub(crate) thinking: Option<String>,
    pub(crate) workflow_mode: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) updated_at_ms: Option<i64>,
    #[serde(skip)]
    pub(crate) app_bridge_socket: Option<String>,
    #[serde(skip)]
    pub(crate) capabilities: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct PiAppState {
    provider: Option<String>,
    model: Option<String>,
    thinking: Option<String>,
    workflow_mode: Option<String>,
}

impl PiState {
    fn from_app_bridge(state: AppBridgeState) -> Option<PiState> {
        let app: PiAppState = serde_json::from_value(state.state.clone()).ok()?;
        Some(PiState {
            pid: state.pid?,
            model: app.model,
            provider: app.provider,
            thinking: app.thinking,
            workflow_mode: app.workflow_mode,
            cwd: state.cwd,
            updated_at_ms: state.updated_at_ms,
            app_bridge_socket: state.socket,
            capabilities: state.capabilities,
        })
    }
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

    pub(crate) fn can_send_action(
        &mut self,
        action: &str,
        preferred_pid: Option<u32>,
        preferred_cwd: Option<&str>,
    ) -> bool {
        if self.states.is_empty() {
            self.states = discover_states();
        }
        select_state(&self.states, preferred_pid, preferred_cwd).is_some_and(|state| {
            state
                .app_bridge_socket
                .as_deref()
                .is_some_and(|socket| !socket.is_empty())
                && state
                    .capabilities
                    .iter()
                    .any(|capability| capability == action)
        })
    }

    pub(crate) fn send_action(
        &mut self,
        action: &str,
        preferred_pid: Option<u32>,
        preferred_cwd: Option<&str>,
    ) -> Result<()> {
        if self.states.is_empty() {
            self.states = discover_states();
        }
        let state = select_state(&self.states, preferred_pid, preferred_cwd)
            .ok_or_else(|| anyhow!("no pi app bridge state is available"))?;
        let socket = state
            .app_bridge_socket
            .as_deref()
            .ok_or_else(|| anyhow!("selected pi state has no app bridge socket"))?;
        app_bridge::send_action(socket, action, Map::<String, JsonValue>::new())
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
    app_bridge::discover_app_states(Some("pi"))
        .into_iter()
        .filter_map(PiState::from_app_bridge)
        .collect()
}
