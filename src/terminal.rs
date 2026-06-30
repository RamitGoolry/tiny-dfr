use std::{collections::HashMap, fs, path::PathBuf, process::Command};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TerminalKind {
    Direct,
    Tmux,
    #[default]
    Unknown,
}

impl TerminalKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            TerminalKind::Direct => "direct",
            TerminalKind::Tmux => "tmux",
            TerminalKind::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TerminalApp {
    Nvim,
    Pi,
    Ssh,
    Shell,
    #[default]
    Unknown,
}

impl TerminalApp {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            TerminalApp::Nvim => "nvim",
            TerminalApp::Pi => "pi",
            TerminalApp::Ssh => "ssh",
            TerminalApp::Shell => "shell",
            TerminalApp::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TmuxPaneState {
    pub(crate) client_pid: Option<u32>,
    pub(crate) client_tty: String,
    pub(crate) session: String,
    pub(crate) window: String,
    pub(crate) pane_index: String,
    pub(crate) pane_id: String,
    pub(crate) pane_pid: Option<u32>,
    pub(crate) pane_command: String,
    pub(crate) pane_path: String,
    pub(crate) pane_title: String,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TerminalState {
    pub(crate) available: bool,
    pub(crate) terminal: String,
    pub(crate) kind: TerminalKind,
    pub(crate) root_pid: Option<u32>,
    pub(crate) command: String,
    pub(crate) cwd: String,
    pub(crate) app: TerminalApp,
    pub(crate) app_pid: Option<u32>,
    pub(crate) nvim_pid: Option<u32>,
    pub(crate) tmux: Option<TmuxPaneState>,
    pub(crate) ghostty_pid_window_count: usize,
    pub(crate) ambiguous: bool,
}

pub(crate) struct TerminalContextClient;

#[derive(Clone, Debug)]
struct ActiveWindowInfo {
    class: String,
    pid: u32,
}

#[derive(Clone, Debug)]
struct HyprlandWindowInfo {
    class: String,
    initial_class: String,
    pid: u32,
}

#[derive(Clone, Debug)]
struct ProcInfo {
    pid: u32,
    ppid: u32,
    comm: String,
    cwd: String,
}

impl TerminalContextClient {
    pub(crate) fn new() -> TerminalContextClient {
        TerminalContextClient
    }

    pub(crate) fn refresh(&mut self, focused_class: &str) -> TerminalState {
        let active = active_window_info();
        let active_class = active
            .as_ref()
            .map(|window| window.class.as_str())
            .unwrap_or("");
        if !is_ghostty_class(focused_class) && !is_ghostty_class(active_class) {
            return TerminalState::default();
        }
        let Some(root_pid) = active.map(|window| window.pid) else {
            return TerminalState {
                available: true,
                terminal: "ghostty".to_string(),
                kind: TerminalKind::Unknown,
                root_pid: None,
                ..TerminalState::default()
            };
        };

        let ghostty_pid_window_count = ghostty_windows_for_pid(root_pid).unwrap_or(1);
        if ghostty_pid_window_count > 1 {
            return TerminalState {
                available: true,
                terminal: "ghostty".to_string(),
                kind: TerminalKind::Unknown,
                root_pid: Some(root_pid),
                ghostty_pid_window_count,
                ambiguous: true,
                ..TerminalState::default()
            };
        }

        let procs = read_process_table();
        let descendants = descendants_of(root_pid, &procs);

        if let Some(tmux) = focused_tmux_pane(root_pid, &procs, &descendants) {
            let pane_pid = tmux.pane_pid;
            let nvim_pid = pane_pid.and_then(|pid| find_descendant_command(pid, &procs, "nvim"));
            let app = if nvim_pid.is_some()
                || classify_command(&tmux.pane_command) == TerminalApp::Nvim
            {
                TerminalApp::Nvim
            } else {
                classify_command(&tmux.pane_command)
            };
            let app_pid = match app {
                TerminalApp::Nvim => nvim_pid.or(pane_pid),
                _ => pane_pid,
            };
            return TerminalState {
                available: true,
                terminal: "ghostty".to_string(),
                kind: TerminalKind::Tmux,
                root_pid: Some(root_pid),
                command: tmux.pane_command.clone(),
                cwd: tmux.pane_path.clone(),
                app,
                app_pid,
                nvim_pid,
                tmux: Some(tmux),
                ghostty_pid_window_count,
                ambiguous: false,
            };
        }

        let direct = direct_terminal_app(root_pid, &procs, &descendants);
        TerminalState {
            available: true,
            terminal: "ghostty".to_string(),
            kind: TerminalKind::Direct,
            root_pid: Some(root_pid),
            command: direct
                .as_ref()
                .map(|proc| proc.comm.clone())
                .unwrap_or_default(),
            cwd: direct
                .as_ref()
                .map(|proc| proc.cwd.clone())
                .unwrap_or_default(),
            app: direct
                .as_ref()
                .map(|proc| classify_command(&proc.comm))
                .unwrap_or(TerminalApp::Unknown),
            app_pid: direct.as_ref().map(|proc| proc.pid),
            nvim_pid: direct
                .as_ref()
                .filter(|proc| classify_command(&proc.comm) == TerminalApp::Nvim)
                .map(|proc| proc.pid),
            tmux: None,
            ghostty_pid_window_count,
            ambiguous: false,
        }
    }
}

fn is_ghostty_class(class: &str) -> bool {
    class.to_ascii_lowercase().contains("ghostty")
}

fn active_window_info() -> Option<ActiveWindowInfo> {
    let output = Command::new("hyprctl")
        .args(["activewindow", "-j"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let class = value.get("class")?.as_str()?.to_string();
    let pid = value
        .get("pid")?
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())?;
    Some(ActiveWindowInfo { class, pid })
}

fn hyprland_windows() -> Option<Vec<HyprlandWindowInfo>> {
    let output = Command::new("hyprctl")
        .args(["clients", "-j"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let values: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).ok()?;
    Some(
        values
            .into_iter()
            .filter_map(|value| {
                let pid = value
                    .get("pid")?
                    .as_u64()
                    .and_then(|pid| u32::try_from(pid).ok())?;
                Some(HyprlandWindowInfo {
                    class: value
                        .get("class")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    initial_class: value
                        .get("initialClass")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    pid,
                })
            })
            .collect(),
    )
}

fn ghostty_windows_for_pid(pid: u32) -> Option<usize> {
    Some(
        hyprland_windows()?
            .into_iter()
            .filter(|window| window.pid == pid)
            .filter(|window| {
                is_ghostty_class(&window.class) || is_ghostty_class(&window.initial_class)
            })
            .count(),
    )
}

fn read_process_table() -> HashMap<u32, ProcInfo> {
    let mut out = HashMap::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let Some(ppid) = read_ppid(pid) else {
            continue;
        };
        let comm = fs::read_to_string(format!("/proc/{pid}/comm"))
            .unwrap_or_default()
            .trim()
            .to_string();
        let cwd = fs::read_link(format!("/proc/{pid}/cwd"))
            .unwrap_or_else(|_| PathBuf::new())
            .display()
            .to_string();
        out.insert(
            pid,
            ProcInfo {
                pid,
                ppid,
                comm,
                cwd,
            },
        );
    }
    out
}

fn read_ppid(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, after_comm) = stat.rsplit_once(") ")?;
    let mut fields = after_comm.split_whitespace();
    fields.next()?; // state
    fields.next()?.parse::<u32>().ok()
}

fn descendants_of(root: u32, procs: &HashMap<u32, ProcInfo>) -> Vec<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for proc in procs.values() {
        children.entry(proc.ppid).or_default().push(proc.pid);
    }

    let mut out = Vec::new();
    let mut stack = children.get(&root).cloned().unwrap_or_default();
    while let Some(pid) = stack.pop() {
        out.push(pid);
        if let Some(next) = children.get(&pid) {
            stack.extend(next);
        }
    }
    out
}

fn find_descendant_command(
    root: u32,
    procs: &HashMap<u32, ProcInfo>,
    command: &str,
) -> Option<u32> {
    let descendants = descendants_of(root, procs);
    descendants.into_iter().find(|pid| {
        procs
            .get(pid)
            .is_some_and(|proc| command_matches(&proc.comm, command))
    })
}

fn direct_terminal_app<'a>(
    _root_pid: u32,
    procs: &'a HashMap<u32, ProcInfo>,
    descendants: &[u32],
) -> Option<&'a ProcInfo> {
    for wanted in ["nvim", "pi", "ssh", "zsh", "bash", "fish"] {
        if let Some(proc) = descendants
            .iter()
            .filter_map(|pid| procs.get(pid))
            .find(|proc| command_matches(&proc.comm, wanted))
        {
            return Some(proc);
        }
    }
    None
}

fn focused_tmux_pane(
    root_pid: u32,
    procs: &HashMap<u32, ProcInfo>,
    descendants: &[u32],
) -> Option<TmuxPaneState> {
    let descendant_set: std::collections::HashSet<u32> = descendants.iter().copied().collect();
    let mut candidates = list_tmux_clients()
        .into_iter()
        .filter(|client| {
            client.client_pid.is_some_and(|pid| {
                descendant_set.contains(&pid) || is_ancestor(root_pid, pid, procs)
            })
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|client| client.client_pid.unwrap_or(0));
    candidates.pop()
}

fn is_ancestor(ancestor: u32, pid: u32, procs: &HashMap<u32, ProcInfo>) -> bool {
    let mut current = pid;
    for _ in 0..128 {
        if current == ancestor {
            return true;
        }
        let Some(proc) = procs.get(&current) else {
            return false;
        };
        if proc.ppid == 0 || proc.ppid == current {
            return false;
        }
        current = proc.ppid;
    }
    false
}

fn list_tmux_clients() -> Vec<TmuxPaneState> {
    let format = "#{client_pid}\t#{client_tty}\t#{session_name}\t#{window_index}\t#{pane_index}\t#{pane_id}\t#{pane_pid}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_title}";
    let output = Command::new("tmux")
        .args(["list-clients", "-F", format])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_tmux_client)
        .collect()
}

fn parse_tmux_client(line: &str) -> Option<TmuxPaneState> {
    let fields = line.split('\t').collect::<Vec<_>>();
    if fields.len() < 10 {
        return None;
    }
    Some(TmuxPaneState {
        client_pid: fields[0].parse::<u32>().ok(),
        client_tty: fields[1].to_string(),
        session: fields[2].to_string(),
        window: fields[3].to_string(),
        pane_index: fields[4].to_string(),
        pane_id: fields[5].to_string(),
        pane_pid: fields[6].parse::<u32>().ok(),
        pane_command: fields[7].to_string(),
        pane_path: fields[8].to_string(),
        pane_title: fields[9].to_string(),
    })
}

fn command_matches(comm: &str, wanted: &str) -> bool {
    comm == wanted || comm.ends_with(&format!("/{wanted}"))
}

fn classify_command(command: &str) -> TerminalApp {
    let command = command.rsplit('/').next().unwrap_or(command);
    if command_matches(command, "nvim") || command_matches(command, "vim") {
        TerminalApp::Nvim
    } else if command_matches(command, "pi") {
        TerminalApp::Pi
    } else if command_matches(command, "ssh") {
        TerminalApp::Ssh
    } else if matches!(command, "zsh" | "bash" | "fish" | "sh") {
        TerminalApp::Shell
    } else {
        TerminalApp::Unknown
    }
}
