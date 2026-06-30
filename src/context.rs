//! Hyprland focused-window context. Connects to the compositor's event socket
//! (`.socket2.sock`) and parses `activewindow>>class,title` so the bar can adapt to
//! what the user is doing. This runs in-process — possible only because tiny-dfr
//! now runs in the user session (it can reach `$XDG_RUNTIME_DIR/hypr/...`).
use std::io::Read;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

pub struct ContextListener {
    stream: Option<UnixStream>,
    /// Partial-line accumulator across reads.
    buf: Vec<u8>,
    class: String,
    title: String,
}

fn socket_path() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let his = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
    let mut p = PathBuf::from(runtime);
    p.push("hypr");
    p.push(his);
    p.push(".socket2.sock");
    Some(p)
}

impl ContextListener {
    pub fn new() -> ContextListener {
        ContextListener {
            stream: Self::try_connect(),
            buf: Vec::new(),
            class: String::new(),
            title: String::new(),
        }
    }

    fn try_connect() -> Option<UnixStream> {
        let path = socket_path()?;
        match UnixStream::connect(&path) {
            Ok(s) => match s.set_nonblocking(true) {
                Ok(()) => {
                    eprintln!("context: connected to Hyprland ({})", path.display());
                    Some(s)
                }
                Err(e) => {
                    eprintln!("context: set_nonblocking failed: {e}");
                    None
                }
            },
            Err(e) => {
                eprintln!("context: not connected to Hyprland ({e}); app layers idle");
                None
            }
        }
    }

    /// The socket fd for epoll registration, if connected.
    pub fn as_fd(&self) -> Option<BorrowedFd<'_>> {
        self.stream.as_ref().map(|s| s.as_fd())
    }

    pub fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    /// Try to reconnect if currently disconnected. Returns true when a new
    /// connection was just established (so the caller can re-register the fd).
    pub fn reconnect(&mut self) -> bool {
        if self.stream.is_none() {
            self.stream = Self::try_connect();
        }
        self.stream.is_some()
    }

    /// Drain pending events. Returns `Some(class)` when the focused window changed
    /// (class may be empty when focus moves to an empty workspace).
    pub fn poll(&mut self) -> Option<String> {
        self.stream.as_ref()?;
        let mut tmp = [0u8; 4096];
        let mut disconnected = false;
        loop {
            // Re-borrow per iteration so the stream borrow doesn't outlive the read
            // (parse happens after, with no stream borrow held).
            let stream = self.stream.as_mut().unwrap();
            match stream.read(&mut tmp) {
                Ok(0) => {
                    eprintln!("context: Hyprland socket closed; will retry");
                    disconnected = true;
                    break;
                }
                Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    eprintln!("context: read error: {e}");
                    disconnected = true;
                    break;
                }
            }
        }
        if disconnected {
            self.stream = None;
        }
        self.parse_lines().then(|| self.class.clone())
    }

    /// Parse complete lines out of `buf`; update class/title on `activewindow`.
    fn parse_lines(&mut self) -> bool {
        let mut changed = false;
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line[..line.len() - 1]);
            // `activewindow>>CLASS,TITLE` — class has no comma; title may.
            if let Some(rest) = line.strip_prefix("activewindow>>") {
                let (class, title) = match rest.split_once(',') {
                    Some((c, t)) => (c.to_string(), t.to_string()),
                    None => (rest.to_string(), String::new()),
                };
                if class != self.class || title != self.title {
                    self.class = class;
                    self.title = title;
                    changed = true;
                }
            }
        }
        changed
    }
}
