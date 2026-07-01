use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as JsonValue};
use std::{
    env, fs,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    time::Duration,
};

pub(crate) const PROTOCOL: &str = "tiny-dfr.app.v1";
const REQUEST_TIMEOUT: Duration = Duration::from_millis(500);

#[allow(dead_code)]
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct AppBridgeState {
    #[serde(rename = "type")]
    pub(crate) message_type: Option<String>,
    pub(crate) protocol: Option<String>,
    pub(crate) instance_id: String,
    pub(crate) app: String,
    pub(crate) pid: Option<u32>,
    pub(crate) socket: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) surface: Option<String>,
    #[serde(default)]
    pub(crate) capabilities: Vec<String>,
    #[serde(default)]
    pub(crate) state: JsonValue,
    pub(crate) updated_at_ms: Option<i64>,
}

impl AppBridgeState {
    pub(crate) fn is_protocol_state(&self) -> bool {
        self.message_type.as_deref().unwrap_or("state") == "state"
            && self.protocol.as_deref().unwrap_or(PROTOCOL) == PROTOCOL
    }

    #[allow(dead_code)]
    pub(crate) fn action_socket(&self) -> Option<&str> {
        self.socket.as_deref().filter(|socket| !socket.is_empty())
    }

    #[allow(dead_code)]
    pub(crate) fn state_object(&self) -> Map<String, JsonValue> {
        match &self.state {
            JsonValue::Object(object) => object.clone(),
            _ => Map::new(),
        }
    }
}

#[derive(Serialize)]
struct AppActionRequest<'a> {
    #[serde(rename = "type")]
    message_type: &'a str,
    protocol: &'a str,
    id: &'a str,
    action: &'a str,
    #[serde(skip_serializing_if = "Map::is_empty")]
    params: Map<String, JsonValue>,
}

#[derive(Debug, Deserialize)]
struct AppActionResponse {
    ok: bool,
    error: Option<String>,
}

pub(crate) fn runtime_root() -> PathBuf {
    if let Some(dir) = env::var_os("TINY_DFR_RUNTIME_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("tiny-dfr");
    }
    let user = env::var("USER").unwrap_or_else(|_| unsafe { libc::geteuid() }.to_string());
    PathBuf::from(format!("/tmp/tiny-dfr-{user}"))
}

pub(crate) fn discover_app_states(app: Option<&str>) -> Vec<AppBridgeState> {
    let dir = runtime_root().join("apps");
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
            let state = serde_json::from_str::<AppBridgeState>(&raw).ok()?;
            if !state.is_protocol_state() {
                return None;
            }
            if app.is_some_and(|app| state.app != app) {
                return None;
            }
            Some(state)
        })
        .collect()
}

#[allow(dead_code)]
pub(crate) fn select_app_state<'a>(
    states: &'a [AppBridgeState],
    preferred_pid: Option<u32>,
    preferred_cwd: Option<&str>,
) -> Option<&'a AppBridgeState> {
    if let Some(pid) = preferred_pid {
        if let Some(state) = states.iter().find(|state| state.pid == Some(pid)) {
            return Some(state);
        }
        if let Some(state) = states
            .iter()
            .filter_map(|state| state.pid.map(|pid| (state, pid)))
            .find(|(_, pid)| process_is_descendant(*pid, preferred_pid.unwrap()))
            .map(|(state, _)| state)
        {
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

pub(crate) fn send_action(
    socket: &str,
    action: &str,
    params: Map<String, JsonValue>,
) -> Result<()> {
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("connecting to app bridge socket {socket}"))?;
    stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
    stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;

    let request = AppActionRequest {
        message_type: "action",
        protocol: PROTOCOL,
        id: "tiny-dfr",
        action,
        params,
    };
    let payload = serde_json::to_string(&request)?;
    stream.write_all(payload.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.trim().is_empty() {
        return Err(anyhow!("app bridge returned an empty response"));
    }
    let response: AppActionResponse = serde_json::from_str(&line)?;
    if response.ok {
        Ok(())
    } else {
        Err(anyhow!(
            "app bridge action failed: {}",
            response
                .error
                .unwrap_or_else(|| "unknown error".to_string())
        ))
    }
}

#[allow(dead_code)]
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

#[allow(dead_code)]
fn read_ppid(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, after_comm) = stat.rsplit_once(") ")?;
    let mut fields = after_comm.split_whitespace();
    fields.next()?; // state
    fields.next()?.parse::<u32>().ok()
}
