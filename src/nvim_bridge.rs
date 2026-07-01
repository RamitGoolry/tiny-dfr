use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as JsonValue};
use std::{fs, path::PathBuf};

use crate::app_bridge::{self, AppBridgeState};

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct NvimDapState {
    pub(crate) active: bool,
    pub(crate) state: Option<String>,
    pub(crate) adapter: Option<String>,
    pub(crate) session: Option<String>,
    pub(crate) updated_at_ms: Option<i64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct NvimDapUiState {
    pub(crate) available: bool,
    pub(crate) visible: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct NvimNeotestState {
    pub(crate) available: bool,
    pub(crate) summary_visible: bool,
    pub(crate) updated_at_ms: Option<i64>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct NvimDbConnection {
    pub(crate) name: String,
    pub(crate) source: String,
    pub(crate) key_name: String,
    pub(crate) is_connected: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct NvimDbuiState {
    pub(crate) available: bool,
    pub(crate) visible: bool,
    pub(crate) in_buffer: bool,
    #[serde(default)]
    pub(crate) connections: Vec<NvimDbConnection>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct NvimAppState {
    mode: Option<String>,
    file: Option<String>,
    filetype: Option<String>,
    #[serde(default)]
    dap: NvimDapState,
    #[serde(default)]
    dapui: NvimDapUiState,
    #[serde(default)]
    neotest: NvimNeotestState,
    #[serde(default)]
    dbui: NvimDbuiState,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct NvimBridgeState {
    pub(crate) pid: u32,
    pub(crate) socket: String,
    pub(crate) cwd: Option<String>,
    pub(crate) mode: Option<String>,
    pub(crate) file: Option<String>,
    pub(crate) filetype: Option<String>,
    #[serde(default)]
    pub(crate) dap: NvimDapState,
    #[serde(default)]
    pub(crate) dapui: NvimDapUiState,
    #[serde(default)]
    pub(crate) neotest: NvimNeotestState,
    #[serde(default)]
    pub(crate) dbui: NvimDbuiState,
}

impl NvimBridgeState {
    fn from_app_bridge(state: AppBridgeState) -> Option<NvimBridgeState> {
        let app: NvimAppState = serde_json::from_value(state.state.clone()).ok()?;
        Some(NvimBridgeState {
            pid: state.pid?,
            socket: state.socket?,
            cwd: state.cwd,
            mode: app.mode,
            file: app.file,
            filetype: app.filetype,
            dap: app.dap,
            dapui: app.dapui,
            neotest: app.neotest,
            dbui: app.dbui,
        })
    }
}

#[derive(Debug, Default)]
pub(crate) struct NvimBridgeSnapshot {
    pub(crate) available: bool,
    pub(crate) selected: Option<NvimBridgeState>,
}

#[derive(Debug, Default)]
pub(crate) struct NvimBridgeClient {
    states: Vec<NvimBridgeState>,
}

impl NvimBridgeClient {
    pub(crate) fn new() -> NvimBridgeClient {
        NvimBridgeClient::default()
    }

    pub(crate) fn refresh(&mut self, preferred_pid: Option<u32>) -> NvimBridgeSnapshot {
        self.states = discover_states();
        NvimBridgeSnapshot {
            available: !self.states.is_empty(),
            selected: self.selected_state(preferred_pid).cloned(),
        }
    }

    pub(crate) fn send_action(&mut self, action: &str, preferred_pid: Option<u32>) -> Result<()> {
        self.send_action_with_db_key(action, None, preferred_pid)
    }

    pub(crate) fn send_db_connect(
        &mut self,
        db_key_name: &str,
        preferred_pid: Option<u32>,
    ) -> Result<()> {
        self.send_action_with_db_key("dbui.connect", Some(db_key_name), preferred_pid)
    }

    fn send_action_with_db_key(
        &mut self,
        action: &str,
        db_key_name: Option<&str>,
        preferred_pid: Option<u32>,
    ) -> Result<()> {
        if self.states.is_empty() {
            self.states = discover_states();
        }
        let state = self
            .selected_state(preferred_pid)
            .ok_or_else(|| anyhow!("no nvim bridge socket is available"))?;
        send_action(&state.socket, action, db_key_name)
            .with_context(|| format!("sending nvim bridge action `{action}` to {}", state.socket))
    }

    fn selected_state(&self, preferred_pid: Option<u32>) -> Option<&NvimBridgeState> {
        if let Some(pid) = preferred_pid {
            if let Some(state) = self.states.iter().find(|state| state.pid == pid) {
                return Some(state);
            }
            if let Some(state) = self
                .states
                .iter()
                .find(|state| process_is_descendant(state.pid, pid))
            {
                return Some(state);
            }
            return None;
        }
        self.states.iter().max_by_key(|state| {
            let dap_rank = if state.dap.active || state.dapui.visible {
                1_i64
            } else {
                0_i64
            };
            (dap_rank, state.dap.updated_at_ms.unwrap_or(0))
        })
    }
}

fn discover_states() -> Vec<NvimBridgeState> {
    app_bridge::discover_app_states(Some("nvim"))
        .into_iter()
        .filter_map(NvimBridgeState::from_app_bridge)
        .filter(|state| process_exists(state.pid) && fs::metadata(&state.socket).is_ok())
        .collect()
}

fn process_exists(pid: u32) -> bool {
    PathBuf::from(format!("/proc/{pid}")).exists()
}

fn process_is_descendant(pid: u32, ancestor: u32) -> bool {
    let mut current = pid;
    for _ in 0..128 {
        if current == ancestor {
            return true;
        }
        let Some(ppid) = read_ppid(current) else {
            return false;
        };
        if ppid == 0 || ppid == current {
            return false;
        }
        current = ppid;
    }
    false
}

fn read_ppid(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, after_comm) = stat.rsplit_once(") ")?;
    let mut fields = after_comm.split_whitespace();
    fields.next()?; // state
    fields.next()?.parse::<u32>().ok()
}

fn send_action(socket: &str, action: &str, db_key_name: Option<&str>) -> Result<()> {
    let mut params = Map::<String, JsonValue>::new();
    if let Some(db_key_name) = db_key_name {
        params.insert(
            "db_key_name".to_string(),
            JsonValue::String(db_key_name.to_string()),
        );
    }
    app_bridge::send_action(socket, action, params)
        .with_context(|| format!("sending nvim bridge action `{action}` to {socket}"))
}
