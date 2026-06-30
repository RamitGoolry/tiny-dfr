use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    env, fs,
    io::{ErrorKind, Read, Write},
    os::{
        fd::{AsFd, BorrowedFd},
        unix::net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    sync::{mpsc, Arc, Mutex},
    thread,
    time::Duration,
};

use crate::nvim_bridge::NvimBridgeState;

const ACTION_TIMEOUT: Duration = Duration::from_millis(500);
const REMOTE_THREAD_IDLE_SLEEP: Duration = Duration::from_millis(20);

#[allow(dead_code)]
#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct RemoteTmuxState {
    pub(crate) pane_id: String,
    pub(crate) pane_command: String,
    pub(crate) pane_path: String,
    pub(crate) pane_pid: Option<u32>,
    pub(crate) client_tty: String,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct RemoteContext {
    pub(crate) session_id: String,
    pub(crate) host: String,
    pub(crate) kind: String,
    pub(crate) app: String,
    pub(crate) command: String,
    pub(crate) cwd: String,
    pub(crate) updated_at_ms: i64,
    pub(crate) nvim_pid: Option<u32>,
    pub(crate) nvim_bridge: Option<NvimBridgeState>,
    pub(crate) tmux_match_source: Option<String>,
    pub(crate) nvim_match_source: Option<String>,
    pub(crate) tmux: Option<RemoteTmuxState>,
}

#[derive(Serialize)]
struct RemoteActionRequest<'a> {
    id: &'a str,
    action: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    db_key_name: Option<&'a str>,
    #[serde(rename = "type")]
    message_type: &'a str,
}

#[derive(Clone, Debug, Default)]
struct RemoteSharedState {
    contexts: HashMap<String, RemoteContext>,
    signature: String,
    connected_sessions: usize,
}

struct RemoteConnection {
    stream: UnixStream,
    buffer: Vec<u8>,
    session_id: Option<String>,
}

enum RemoteCommand {
    SendAction {
        action: String,
        db_key_name: Option<String>,
    },
}

pub(crate) struct RemoteClient {
    wake_reader: Option<UnixStream>,
    shared: Arc<Mutex<RemoteSharedState>>,
    command_tx: Option<mpsc::Sender<RemoteCommand>>,
    last_signature: String,
}

impl RemoteClient {
    pub(crate) fn new() -> RemoteClient {
        let shared = Arc::new(Mutex::new(RemoteSharedState::default()));
        let (command_tx, command_rx) = mpsc::channel();
        let wake_pair = UnixStream::pair();
        let (wake_reader, wake_writer) = match wake_pair {
            Ok((reader, writer)) => {
                let _ = reader.set_nonblocking(true);
                (Some(reader), Some(writer))
            }
            Err(err) => {
                eprintln!("remote bridge unavailable: failed to create wake socket: {err}");
                (None, None)
            }
        };

        if let Some(wake_writer) = wake_writer {
            let thread_shared = Arc::clone(&shared);
            if let Err(err) = thread::Builder::new()
                .name("tiny-dfr-remote".to_string())
                .spawn(move || remote_thread(thread_shared, command_rx, wake_writer))
            {
                eprintln!("remote bridge unavailable: spawn failed: {err}");
            }
        }

        RemoteClient {
            wake_reader,
            shared,
            command_tx: Some(command_tx),
            last_signature: String::new(),
        }
    }

    pub(crate) fn wake_fd(&self) -> Option<BorrowedFd<'_>> {
        self.wake_reader.as_ref().map(|reader| reader.as_fd())
    }

    pub(crate) fn drain_wake(&mut self) -> bool {
        let Some(reader) = &mut self.wake_reader else {
            return false;
        };
        let mut drained = false;
        loop {
            let mut buf = [0_u8; 64];
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => drained = true,
                Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                Err(err) => {
                    eprintln!("remote bridge: wake drain failed: {err}");
                    break;
                }
            }
        }
        drained
    }

    pub(crate) fn refresh(&mut self) -> bool {
        let signature = self
            .shared
            .lock()
            .map(|shared| shared.signature.clone())
            .unwrap_or_default();
        if signature != self.last_signature {
            self.last_signature = signature;
            true
        } else {
            false
        }
    }

    pub(crate) fn latest_context(&self) -> Option<RemoteContext> {
        self.shared.lock().ok().and_then(|shared| {
            shared
                .contexts
                .values()
                .max_by_key(|context| context.updated_at_ms)
                .cloned()
        })
    }

    pub(crate) fn is_active(&self) -> bool {
        self.shared
            .lock()
            .is_ok_and(|shared| shared.connected_sessions > 0 || !shared.contexts.is_empty())
    }

    pub(crate) fn send_action(&mut self, action: &str, db_key_name: Option<&str>) -> Result<()> {
        if self.latest_context().is_none() {
            return Err(anyhow!("no remote tiny-dfr context is available"));
        }
        let Some(tx) = &self.command_tx else {
            return Err(anyhow!("remote bridge command channel is unavailable"));
        };
        tx.send(RemoteCommand::SendAction {
            action: action.to_string(),
            db_key_name: db_key_name.map(str::to_string),
        })
        .context("sending remote bridge command")
    }
}

fn remote_thread(
    shared: Arc<Mutex<RemoteSharedState>>,
    command_rx: mpsc::Receiver<RemoteCommand>,
    mut wake_writer: UnixStream,
) {
    let listener = match bind_listener() {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("remote bridge unavailable: {err:#}");
            return;
        }
    };
    let mut clients = Vec::<RemoteConnection>::new();
    loop {
        process_commands(&command_rx, &mut clients, &shared, &mut wake_writer);
        accept_pending(&listener, &mut clients, &shared, &mut wake_writer);
        read_pending(&mut clients, &shared, &mut wake_writer);
        thread::sleep(REMOTE_THREAD_IDLE_SLEEP);
    }
}

fn process_commands(
    command_rx: &mpsc::Receiver<RemoteCommand>,
    clients: &mut [RemoteConnection],
    shared: &Arc<Mutex<RemoteSharedState>>,
    wake_writer: &mut UnixStream,
) {
    while let Ok(command) = command_rx.try_recv() {
        match command {
            RemoteCommand::SendAction {
                action,
                db_key_name,
            } => {
                if let Err(err) =
                    send_action_to_latest(&action, db_key_name.as_deref(), clients, shared)
                {
                    eprintln!("remote bridge: action `{action}` failed: {err:#}");
                    wake(wake_writer);
                }
            }
        }
    }
}

fn send_action_to_latest(
    action: &str,
    db_key_name: Option<&str>,
    clients: &mut [RemoteConnection],
    shared: &Arc<Mutex<RemoteSharedState>>,
) -> Result<()> {
    let session_id = latest_context(shared)
        .ok_or_else(|| anyhow!("no remote tiny-dfr context is available"))?
        .session_id;
    let connection = clients
        .iter_mut()
        .find(|client| client.session_id.as_deref() == Some(session_id.as_str()))
        .ok_or_else(|| anyhow!("remote session `{session_id}` is not connected"))?;
    let request = RemoteActionRequest {
        id: "tiny-dfr",
        action,
        db_key_name,
        message_type: "action",
    };
    let payload = serde_json::to_string(&request)?;
    connection
        .stream
        .set_nonblocking(false)
        .context("making remote bridge stream blocking for action write")?;
    connection.stream.set_write_timeout(Some(ACTION_TIMEOUT))?;
    let result = connection
        .stream
        .write_all(payload.as_bytes())
        .and_then(|_| connection.stream.write_all(b"\n"))
        .and_then(|_| connection.stream.flush())
        .with_context(|| format!("sending remote action `{action}` to {session_id}"));
    let _ = connection.stream.set_nonblocking(true);
    result
}

fn accept_pending(
    listener: &UnixListener,
    clients: &mut Vec<RemoteConnection>,
    shared: &Arc<Mutex<RemoteSharedState>>,
    wake_writer: &mut UnixStream,
) {
    let mut accepted = false;
    loop {
        match listener.accept() {
            Ok((stream, _addr)) => {
                if let Err(err) = stream.set_nonblocking(true) {
                    eprintln!("remote bridge: failed to make client nonblocking: {err}");
                    continue;
                }
                clients.push(RemoteConnection {
                    stream,
                    buffer: Vec::new(),
                    session_id: None,
                });
                accepted = true;
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => break,
            Err(err) => {
                eprintln!("remote bridge: accept failed: {err}");
                break;
            }
        }
    }
    if accepted {
        update_connected_count(shared, clients.len(), wake_writer);
    }
}

fn read_pending(
    clients: &mut Vec<RemoteConnection>,
    shared: &Arc<Mutex<RemoteSharedState>>,
    wake_writer: &mut UnixStream,
) {
    let mut idx = 0;
    while idx < clients.len() {
        let mut remove = false;
        loop {
            let mut chunk = [0_u8; 4096];
            match clients[idx].stream.read(&mut chunk) {
                Ok(0) => {
                    remove = true;
                    break;
                }
                Ok(n) => {
                    clients[idx].buffer.extend_from_slice(&chunk[..n]);
                    while let Some(line) = pop_line(&mut clients[idx].buffer) {
                        handle_line(idx, &line, clients, shared, wake_writer);
                    }
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                Err(err) => {
                    eprintln!("remote bridge: read failed: {err}");
                    remove = true;
                    break;
                }
            }
        }
        if remove {
            if let Some(session_id) = clients[idx].session_id.clone() {
                if remove_context(shared, &session_id, wake_writer) {
                    eprintln!("remote bridge: session {session_id} disconnected");
                }
            }
            clients.remove(idx);
            update_connected_count(shared, clients.len(), wake_writer);
        } else {
            idx += 1;
        }
    }
}

fn handle_line(
    client_idx: usize,
    line: &str,
    clients: &mut [RemoteConnection],
    shared: &Arc<Mutex<RemoteSharedState>>,
    wake_writer: &mut UnixStream,
) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        eprintln!("remote bridge: invalid JSONL message: {line}");
        return;
    };
    let message_type = value
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    if let Some(session_id) = value.get("session_id").and_then(|value| value.as_str()) {
        clients[client_idx].session_id = Some(session_id.to_string());
    }
    match message_type {
        "hello" => {}
        "context" => match serde_json::from_value::<RemoteContext>(value) {
            Ok(context) => {
                let changed = upsert_context(shared, context.clone());
                if changed {
                    eprintln!(
                        "remote bridge: session={} host={} kind={} app={} cmd={} tmux_match={} nvim_match={}",
                        context.session_id,
                        context.host,
                        context.kind,
                        context.app,
                        context.command,
                        context.tmux_match_source.as_deref().unwrap_or(""),
                        context.nvim_match_source.as_deref().unwrap_or(""),
                    );
                    wake(wake_writer);
                }
            }
            Err(err) => eprintln!("remote bridge: failed to parse context: {err}"),
        },
        "action_result" => {
            if value.get("ok").and_then(|value| value.as_bool()) == Some(false) {
                eprintln!("remote bridge action failed: {value}");
                wake(wake_writer);
            }
        }
        _ => eprintln!("remote bridge: unknown message type `{message_type}`"),
    }
}

fn latest_context(shared: &Arc<Mutex<RemoteSharedState>>) -> Option<RemoteContext> {
    shared.lock().ok().and_then(|shared| {
        shared
            .contexts
            .values()
            .max_by_key(|context| context.updated_at_ms)
            .cloned()
    })
}

fn upsert_context(shared: &Arc<Mutex<RemoteSharedState>>, context: RemoteContext) -> bool {
    let Ok(mut shared) = shared.lock() else {
        return false;
    };
    let before = shared.signature.clone();
    shared.contexts.insert(context.session_id.clone(), context);
    shared.signature = contexts_signature(&shared.contexts, shared.connected_sessions);
    before != shared.signature
}

fn remove_context(
    shared: &Arc<Mutex<RemoteSharedState>>,
    session_id: &str,
    wake_writer: &mut UnixStream,
) -> bool {
    let Ok(mut shared) = shared.lock() else {
        return false;
    };
    let before = shared.signature.clone();
    shared.contexts.remove(session_id);
    shared.signature = contexts_signature(&shared.contexts, shared.connected_sessions);
    let changed = before != shared.signature;
    if changed {
        wake(wake_writer);
    }
    changed
}

fn update_connected_count(
    shared: &Arc<Mutex<RemoteSharedState>>,
    count: usize,
    wake_writer: &mut UnixStream,
) {
    let Ok(mut shared) = shared.lock() else {
        return;
    };
    let before = shared.signature.clone();
    shared.connected_sessions = count;
    shared.signature = contexts_signature(&shared.contexts, shared.connected_sessions);
    if before != shared.signature {
        wake(wake_writer);
    }
}

fn contexts_signature(
    contexts: &HashMap<String, RemoteContext>,
    connected_sessions: usize,
) -> String {
    let mut values = contexts.values().map(context_signature).collect::<Vec<_>>();
    values.sort();
    format!("connected={connected_sessions};{}", values.join(";"))
}

fn context_signature(context: &RemoteContext) -> String {
    let nvim = context.nvim_bridge.as_ref();
    format!(
        "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
        context.session_id,
        context.host,
        context.kind,
        context.app,
        context.command,
        context.cwd,
        context.nvim_pid.unwrap_or(0),
        nvim.map(|state| state.dap.active).unwrap_or(false),
        nvim.and_then(|state| state.dap.state.as_deref())
            .unwrap_or(""),
        nvim.map(|state| state.dapui.visible).unwrap_or(false),
        nvim.map(|state| state.neotest.summary_visible)
            .unwrap_or(false),
        nvim.map(|state| state.dbui.visible || state.dbui.in_buffer)
            .unwrap_or(false),
    )
}

fn wake(wake_writer: &mut UnixStream) {
    let _ = wake_writer.write_all(b"r");
    let _ = wake_writer.flush();
}

fn pop_line(buffer: &mut Vec<u8>) -> Option<String> {
    let pos = buffer.iter().position(|byte| *byte == b'\n')?;
    let line = buffer.drain(..=pos).collect::<Vec<_>>();
    Some(
        String::from_utf8_lossy(&line[..line.len().saturating_sub(1)])
            .trim()
            .to_string(),
    )
}

fn bind_listener() -> Result<UnixListener> {
    let path = socket_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("removing stale {}", path.display())),
    }
    let listener =
        UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    listener
        .set_nonblocking(true)
        .context("making remote bridge listener nonblocking")?;
    eprintln!("remote bridge listening on {}", path.display());
    Ok(listener)
}

fn socket_path() -> PathBuf {
    runtime_dir().join("tiny-dfr/remote.sock")
}

fn runtime_dir() -> PathBuf {
    env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::geteuid() })))
}
