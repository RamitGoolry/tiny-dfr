//! Browser tab discovery and activation behind protocol-specific backends.
//!
//! Chromium-family browsers expose a small HTTP CDP target API, while
//! Firefox-family browsers (including Zen) expose WebDriver BiDi over a
//! WebSocket. The rest of tiny-dfr only deals with [`BrowserTab`] and
//! [`BrowserTabsState`].
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as JsonValue};
use std::{
    collections::{HashMap, HashSet},
    env, fs,
    net::TcpStream,
    path::PathBuf,
    time::{Duration, Instant},
};
use tungstenite::{connect, stream::MaybeTlsStream, Message, WebSocket};

const CDP_DEFAULT_ENDPOINT: &str = "http://127.0.0.1:9223";
const BIDI_DEFAULT_ENDPOINT: &str = "ws://127.0.0.1:9222/session";
const POLL_INTERVAL: Duration = Duration::from_millis(500);
const BIDI_ERROR_RETRY: Duration = Duration::from_secs(5);
const BIDI_IO_TIMEOUT: Duration = Duration::from_secs(2);
const BIDI_SESSION_FILE: &str = "tiny-dfr/bidi-session";

#[derive(Clone, Debug, Serialize)]
pub(crate) struct BrowserTab {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) url: String,
    pub(crate) favicon_url: String,
    pub(crate) active: bool,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct BrowserTabsState {
    pub(crate) available: bool,
    pub(crate) tabs_json: String,
    pub(crate) count: usize,
    pub(crate) active_index: Option<usize>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq)]
pub(crate) struct BrowserMediaTiming {
    pub(crate) length_us: f64,
    pub(crate) position_us: f64,
}

impl BrowserTabsState {
    pub(crate) fn unavailable() -> Self {
        Self {
            available: false,
            tabs_json: "[]".to_string(),
            count: 0,
            active_index: None,
        }
    }

    fn from_tabs(tabs: Vec<BrowserTab>) -> Result<Self> {
        let active_index = tabs.iter().position(|tab| tab.active);
        let tabs_json = serde_json::to_string(&tabs).context("serializing browser tabs")?;
        Ok(Self {
            available: true,
            count: tabs.len(),
            tabs_json,
            active_index,
        })
    }
}

/// Operations the UI needs from any browser debugging protocol.
pub(crate) trait BrowserBackend {
    fn protocol_name(&self) -> &'static str;
    fn refresh_tabs(&mut self, focused_title: &str, force: bool) -> Result<BrowserTabsState>;
    fn activate_tab(&mut self, id: &str) -> Result<()>;
    fn media_timing(&mut self) -> Result<Option<BrowserMediaTiming>> {
        Ok(None)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BrowserKind {
    Cdp,
    Bidi,
}

impl BrowserKind {
    pub(crate) fn from_window_class(class: &str) -> Option<Self> {
        let class = class.to_ascii_lowercase();
        if class.contains("chromium") || class.contains("chrome") {
            Some(Self::Cdp)
        } else if class == "zen"
            || class.contains("zen-browser")
            || class.contains("zen-alpha")
            || class.contains("firefox")
        {
            Some(Self::Bidi)
        } else {
            None
        }
    }
}

/// Selects the protocol backend from the focused browser window.
pub(crate) struct BrowserClient {
    cdp: CdpBackend,
    bidi: BidiBackend,
    active: Option<BrowserKind>,
}

impl BrowserClient {
    pub(crate) fn new() -> Self {
        Self {
            cdp: CdpBackend::new(None),
            bidi: BidiBackend::new(None),
            active: None,
        }
    }

    pub(crate) fn refresh_tabs(
        &mut self,
        kind: Option<BrowserKind>,
        focused_title: &str,
        force: bool,
    ) -> Result<BrowserTabsState> {
        self.active = kind;
        let Some(kind) = kind else {
            return Ok(BrowserTabsState::unavailable());
        };
        self.backend_mut(kind).refresh_tabs(focused_title, force)
    }

    pub(crate) fn activate_tab(&mut self, id: &str) -> Result<()> {
        let kind = self
            .active
            .ok_or_else(|| anyhow!("cannot activate tab without a focused browser"))?;
        self.backend_mut(kind).activate_tab(id)
    }

    pub(crate) fn media_timing(&mut self) -> Result<Option<BrowserMediaTiming>> {
        let Some(kind) = self.active else {
            return Ok(None);
        };
        self.backend_mut(kind).media_timing()
    }

    pub(crate) fn active_protocol_name(&mut self) -> &'static str {
        match self.active {
            Some(kind) => self.backend_mut(kind).protocol_name(),
            None => "browser",
        }
    }

    fn backend_mut(&mut self, kind: BrowserKind) -> &mut dyn BrowserBackend {
        match kind {
            BrowserKind::Cdp => &mut self.cdp,
            BrowserKind::Bidi => &mut self.bidi,
        }
    }
}

#[derive(Debug, Deserialize)]
struct DevToolsTarget {
    id: String,
    title: Option<String>,
    url: Option<String>,
    #[serde(rename = "faviconUrl")]
    favicon_url: Option<String>,
    #[serde(rename = "type")]
    target_type: String,
}

struct CdpBackend {
    endpoint: String,
    last_refresh: Option<Instant>,
    cached: BrowserTabsState,
    tab_order: Vec<String>,
}

impl CdpBackend {
    fn new(endpoint: Option<String>) -> Self {
        Self {
            endpoint: endpoint.unwrap_or_else(|| CDP_DEFAULT_ENDPOINT.to_string()),
            last_refresh: None,
            cached: BrowserTabsState::unavailable(),
            tab_order: Vec::new(),
        }
    }

    fn endpoint(&self) -> &str {
        self.endpoint.trim_end_matches('/')
    }

    fn fetch_tabs(&mut self, focused_title: &str) -> Result<BrowserTabsState> {
        let url = format!("{}/json/list", self.endpoint());
        let body = ureq::get(&url)
            .header("Accept", "application/json")
            .call()
            .with_context(|| format!("fetching CDP targets from {url}"))?
            .body_mut()
            .read_to_string()
            .context("reading CDP target response")?;
        let targets: Vec<DevToolsTarget> =
            serde_json::from_str(&body).context("parsing CDP target list")?;

        let mut tabs: Vec<BrowserTab> = targets
            .into_iter()
            .filter(|target| target.target_type == "page")
            .map(|target| {
                let title = decode_html_entities(&target.title.unwrap_or_default());
                BrowserTab {
                    active: titles_probably_match(&title, focused_title),
                    id: target.id,
                    title,
                    url: target.url.unwrap_or_default(),
                    favicon_url: target.favicon_url.unwrap_or_default(),
                }
            })
            .collect();
        tabs = stable_order(&mut self.tab_order, tabs);
        mark_only_tab_active(&mut tabs);
        BrowserTabsState::from_tabs(tabs)
    }
}

impl BrowserBackend for CdpBackend {
    fn protocol_name(&self) -> &'static str {
        "CDP"
    }

    fn refresh_tabs(&mut self, focused_title: &str, force: bool) -> Result<BrowserTabsState> {
        if !refresh_due(self.last_refresh, force) {
            return Ok(self.cached.clone());
        }
        self.last_refresh = Some(Instant::now());
        match self.fetch_tabs(focused_title) {
            Ok(state) => {
                self.cached = state;
                Ok(self.cached.clone())
            }
            Err(err) => {
                self.cached = BrowserTabsState::unavailable();
                Err(err)
            }
        }
    }

    fn activate_tab(&mut self, id: &str) -> Result<()> {
        let url = format!("{}/json/activate/{id}", self.endpoint());
        ureq::get(&url)
            .call()
            .with_context(|| format!("activating CDP tab `{id}` via {url}"))?;
        Ok(())
    }
}

type BidiSocket = WebSocket<MaybeTlsStream<TcpStream>>;

struct BidiBackend {
    endpoint: String,
    socket: Option<BidiSocket>,
    session_id: Option<String>,
    session_path: Option<PathBuf>,
    next_id: u64,
    last_refresh: Option<Instant>,
    retry_after: Option<Instant>,
    cached: BrowserTabsState,
    tab_order: Vec<String>,
    active_context: Option<String>,
}

impl BidiBackend {
    fn new(endpoint: Option<String>) -> Self {
        Self::with_session_path(endpoint, default_bidi_session_path())
    }

    fn with_session_path(endpoint: Option<String>, session_path: Option<PathBuf>) -> Self {
        let session_id = session_path.as_ref().and_then(load_session_id);
        Self {
            endpoint: endpoint.unwrap_or_else(|| BIDI_DEFAULT_ENDPOINT.to_string()),
            socket: None,
            session_id,
            session_path,
            next_id: 1,
            last_refresh: None,
            retry_after: None,
            cached: BrowserTabsState::unavailable(),
            tab_order: Vec::new(),
            active_context: None,
        }
    }

    fn ensure_connected(&mut self) -> Result<()> {
        if self.socket.is_some() {
            return Ok(());
        }

        // A BiDi session outlives a dropped WebSocket. Reattach to its session
        // resource after a transient I/O failure instead of trying to create a
        // second session (Firefox permits only one active session).
        if let Some(session_id) = self.session_id.as_deref() {
            let session_endpoint =
                format!("{}/{}", self.endpoint.trim_end_matches('/'), session_id);
            if let Ok((mut socket, _)) = connect(session_endpoint.as_str()) {
                set_bidi_timeouts(&mut socket)?;
                self.socket = Some(socket);
                return Ok(());
            }
            self.clear_session_id();
        }

        let (mut socket, _) = connect(self.endpoint.as_str())
            .with_context(|| format!("connecting to WebDriver BiDi at {}", self.endpoint))?;
        set_bidi_timeouts(&mut socket)?;
        let id = self.take_id();
        let result = send_bidi_command(
            &mut socket,
            id,
            "session.new",
            json!({ "capabilities": {} }),
        )?;
        let session_id = result
            .get("sessionId")
            .and_then(JsonValue::as_str)
            .context("BiDi session.new response missing sessionId")?
            .to_string();
        persist_session_id(self.session_path.as_ref(), &session_id)?;
        self.session_id = Some(session_id);
        self.socket = Some(socket);
        Ok(())
    }

    fn command(&mut self, method: &str, params: JsonValue) -> Result<JsonValue> {
        self.ensure_connected()?;
        let id = self.take_id();
        let result = send_bidi_command(
            self.socket.as_mut().expect("connected socket"),
            id,
            method,
            params,
        );
        if result.is_err() {
            self.socket = None;
        }
        result
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn clear_session_id(&mut self) {
        self.session_id = None;
        if let Some(path) = &self.session_path {
            let _ = fs::remove_file(path);
        }
    }

    fn fetch_tabs(&mut self, focused_title: &str) -> Result<BrowserTabsState> {
        let result = self.command("browsingContext.getTree", json!({ "maxDepth": 0 }))?;
        let contexts = result
            .get("contexts")
            .and_then(JsonValue::as_array)
            .context("BiDi getTree response missing contexts")?;
        let context_rows: Vec<(String, String)> = contexts
            .iter()
            .map(|context| {
                Ok((
                    context
                        .get("context")
                        .and_then(JsonValue::as_str)
                        .context("BiDi context missing id")?
                        .to_string(),
                    context
                        .get("url")
                        .and_then(JsonValue::as_str)
                        .unwrap_or_default()
                        .to_string(),
                ))
            })
            .collect::<Result<_>>()?;

        let mut tabs = Vec::with_capacity(context_rows.len());
        for (id, url) in context_rows {
            let metadata = self.tab_metadata(&id).unwrap_or_default();
            tabs.push(BrowserTab {
                active: titles_probably_match(&metadata.title, focused_title),
                id,
                title: metadata.title,
                url,
                favicon_url: metadata.favicon_url,
            });
        }
        tabs = stable_order(&mut self.tab_order, tabs);
        mark_only_tab_active(&mut tabs);
        self.active_context = tabs.iter().find(|tab| tab.active).map(|tab| tab.id.clone());
        BrowserTabsState::from_tabs(tabs)
    }

    fn tab_metadata(&mut self, context: &str) -> Result<TabMetadata> {
        // Return JSON text so parsing does not depend on BiDi's nested remote-value
        // representation for JavaScript objects.
        let expression = r#"JSON.stringify({title: document.title || "", favicon_url: document.querySelector('link[rel~="icon"]')?.href || ""})"#;
        let result = self.evaluate(context, expression)?;
        parse_bidi_string_result(&result)
            .and_then(|value| serde_json::from_str(value).context("parsing BiDi tab metadata"))
    }

    fn evaluate(&mut self, context: &str, expression: &str) -> Result<JsonValue> {
        self.command(
            "script.evaluate",
            json!({
                "expression": expression,
                "target": { "context": context },
                "awaitPromise": false,
            }),
        )
    }

    fn active_media_timing(&mut self) -> Result<Option<BrowserMediaTiming>> {
        let Some(context) = self.active_context.clone() else {
            return Ok(None);
        };
        let expression = r#"(() => { const all = [...document.querySelectorAll('video,audio')]; const media = all.find(item => !item.paused && !item.ended) || all.find(item => Number.isFinite(item.duration) && item.duration > 0); if (!media) return ''; return JSON.stringify({length_us: Number.isFinite(media.duration) ? media.duration * 1000000 : 0, position_us: Number.isFinite(media.currentTime) ? media.currentTime * 1000000 : 0}); })()"#;
        let result = self.evaluate(&context, expression)?;
        let value = parse_bidi_string_result(&result)?;
        if value.is_empty() {
            return Ok(None);
        }
        serde_json::from_str(value)
            .map(Some)
            .context("parsing BiDi media timing")
    }
}

impl BrowserBackend for BidiBackend {
    fn protocol_name(&self) -> &'static str {
        "WebDriver BiDi"
    }

    fn refresh_tabs(&mut self, focused_title: &str, force: bool) -> Result<BrowserTabsState> {
        if !force && self.retry_after.is_some_and(|retry| Instant::now() < retry) {
            return Ok(self.cached.clone());
        }
        if !refresh_due(self.last_refresh, force) {
            return Ok(self.cached.clone());
        }
        self.last_refresh = Some(Instant::now());
        match self.fetch_tabs(focused_title) {
            Ok(state) => {
                self.retry_after = None;
                self.cached = state;
                Ok(self.cached.clone())
            }
            Err(err) => {
                self.cached = BrowserTabsState::unavailable();
                self.socket = None;
                self.active_context = None;
                self.retry_after = Some(Instant::now() + BIDI_ERROR_RETRY);
                Err(err)
            }
        }
    }

    fn activate_tab(&mut self, id: &str) -> Result<()> {
        self.command("browsingContext.activate", json!({ "context": id }))?;
        Ok(())
    }

    fn media_timing(&mut self) -> Result<Option<BrowserMediaTiming>> {
        self.active_media_timing()
    }
}

impl Drop for BidiBackend {
    fn drop(&mut self) {
        let Some(mut socket) = self.socket.take() else {
            return;
        };
        let id = self.take_id();
        if send_bidi_command(&mut socket, id, "session.end", json!({})).is_ok() {
            self.clear_session_id();
        }
    }
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
struct TabMetadata {
    title: String,
    favicon_url: String,
}

fn default_bidi_session_path() -> Option<PathBuf> {
    env::var_os("XDG_RUNTIME_DIR").map(|runtime| PathBuf::from(runtime).join(BIDI_SESSION_FILE))
}

fn load_session_id(path: &PathBuf) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn persist_session_id(path: Option<&PathBuf>, session_id: &str) -> Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating BiDi session directory {}", parent.display()))?;
    }
    fs::write(path, session_id)
        .with_context(|| format!("persisting BiDi session ID to {}", path.display()))
}

fn set_bidi_timeouts(socket: &mut BidiSocket) -> Result<()> {
    if let MaybeTlsStream::Plain(stream) = socket.get_mut() {
        stream
            .set_read_timeout(Some(BIDI_IO_TIMEOUT))
            .context("setting BiDi read timeout")?;
        stream
            .set_write_timeout(Some(BIDI_IO_TIMEOUT))
            .context("setting BiDi write timeout")?;
    }
    Ok(())
}

fn send_bidi_command(
    socket: &mut BidiSocket,
    id: u64,
    method: &str,
    params: JsonValue,
) -> Result<JsonValue> {
    let request = json!({ "id": id, "method": method, "params": params });
    socket
        .send(Message::Text(request.to_string().into()))
        .with_context(|| format!("sending BiDi command `{method}`"))?;

    loop {
        let message = socket
            .read()
            .with_context(|| format!("waiting for BiDi command `{method}`"))?;
        let Message::Text(text) = message else {
            continue;
        };
        let response: JsonValue =
            serde_json::from_str(text.as_ref()).context("parsing BiDi response")?;
        if response.get("id").and_then(JsonValue::as_u64) != Some(id) {
            // Events and out-of-order responses are irrelevant because tiny-dfr
            // issues one synchronous command at a time.
            continue;
        }
        if response.get("type").and_then(JsonValue::as_str) == Some("error") {
            let code = response
                .get("error")
                .and_then(JsonValue::as_str)
                .unwrap_or("unknown error");
            let message = response
                .get("message")
                .and_then(JsonValue::as_str)
                .unwrap_or_default();
            return Err(anyhow!("BiDi `{method}` failed: {code}: {message}"));
        }
        return response
            .get("result")
            .cloned()
            .with_context(|| format!("BiDi `{method}` response missing result"));
    }
}

fn parse_bidi_string_result(result: &JsonValue) -> Result<&str> {
    if result.get("type").and_then(JsonValue::as_str) != Some("success") {
        return Err(anyhow!("BiDi script evaluation failed"));
    }
    result
        .pointer("/result/value")
        .and_then(JsonValue::as_str)
        .context("BiDi script result was not a string")
}

fn refresh_due(last_refresh: Option<Instant>, force: bool) -> bool {
    force || last_refresh.is_none_or(|last| last.elapsed() >= POLL_INTERVAL)
}

fn stable_order(tab_order: &mut Vec<String>, tabs: Vec<BrowserTab>) -> Vec<BrowserTab> {
    let incoming_ids: Vec<String> = tabs.iter().map(|tab| tab.id.clone()).collect();
    let mut by_id: HashMap<String, BrowserTab> =
        tabs.into_iter().map(|tab| (tab.id.clone(), tab)).collect();
    let live_ids: HashSet<String> = by_id.keys().cloned().collect();
    tab_order.retain(|id| live_ids.contains(id));
    for id in incoming_ids {
        if !tab_order.contains(&id) {
            tab_order.push(id);
        }
    }
    tab_order.iter().filter_map(|id| by_id.remove(id)).collect()
}

fn mark_only_tab_active(tabs: &mut [BrowserTab]) {
    if !tabs.iter().any(|tab| tab.active) && tabs.len() == 1 {
        tabs[0].active = true;
    }
}

pub(crate) fn titles_probably_match(tab_title: &str, focused_title: &str) -> bool {
    let tab_title = tab_title.trim();
    let focused_title = focused_title.trim();
    !tab_title.is_empty()
        && !focused_title.is_empty()
        && (tab_title == focused_title
            || focused_title.contains(tab_title)
            || tab_title.contains(focused_title))
}

fn decode_html_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }

    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '&' {
            out.push(chars[i]);
            i += 1;
            continue;
        }

        if i + 2 < chars.len() && chars[i + 1] == '#' {
            let mut j = i + 2;
            let mut radix = 10;
            if matches!(chars.get(j), Some('x' | 'X')) {
                radix = 16;
                j += 1;
            }
            let digits_start = j;
            while j < chars.len() && chars[j].is_digit(radix) {
                j += 1;
            }
            if j > digits_start {
                let digits: String = chars[digits_start..j].iter().collect();
                if let Ok(value) = u32::from_str_radix(&digits, radix) {
                    if let Some(decoded) = char::from_u32(value) {
                        out.push(decoded);
                        if matches!(chars.get(j), Some(';')) {
                            j += 1;
                        }
                        i = j;
                        continue;
                    }
                }
            }
        } else {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_ascii_alphabetic() && j - i <= 8 {
                j += 1;
            }
            let has_semicolon = matches!(chars.get(j), Some(';'));
            let name: String = chars[i + 1..j].iter().collect();
            let decoded = match name.as_str() {
                "amp" => Some('&'),
                "apos" => Some('\''),
                "quot" => Some('"'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                _ => None,
            };
            if let Some(decoded) = decoded {
                out.push(decoded);
                i = j + usize::from(has_semicolon);
                continue;
            }
        }

        out.push(chars[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_browser_window_classes() {
        assert_eq!(
            BrowserKind::from_window_class("chromium"),
            Some(BrowserKind::Cdp)
        );
        assert_eq!(
            BrowserKind::from_window_class("google-chrome"),
            Some(BrowserKind::Cdp)
        );
        assert_eq!(
            BrowserKind::from_window_class("zen"),
            Some(BrowserKind::Bidi)
        );
        assert_eq!(
            BrowserKind::from_window_class("zen-alpha"),
            Some(BrowserKind::Bidi)
        );
        assert_eq!(
            BrowserKind::from_window_class("firefox"),
            Some(BrowserKind::Bidi)
        );
        assert_eq!(BrowserKind::from_window_class("ghostty"), None);
    }

    #[test]
    fn parses_bidi_string_metadata() {
        let response = json!({
            "type": "success",
            "realm": "realm-id",
            "result": {
                "type": "string",
                "value": "{\"title\":\"Zen tab\",\"favicon_url\":\"https://example.com/icon.png\"}"
            }
        });
        let metadata: TabMetadata =
            serde_json::from_str(parse_bidi_string_result(&response).unwrap()).unwrap();
        assert_eq!(
            metadata,
            TabMetadata {
                title: "Zen tab".into(),
                favicon_url: "https://example.com/icon.png".into(),
            }
        );
    }

    #[test]
    fn persists_bidi_session_for_process_restarts() {
        let path = env::temp_dir().join(format!("tiny-dfr-bidi-session-{}", std::process::id()));
        let _ = fs::remove_file(&path);
        persist_session_id(Some(&path), "session-123").unwrap();
        let backend = BidiBackend::with_session_path(None, Some(path.clone()));
        assert_eq!(backend.session_id.as_deref(), Some("session-123"));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn parses_bidi_media_timing() {
        let response = json!({
            "type": "success",
            "realm": "realm-id",
            "result": {
                "type": "string",
                "value": "{\"length_us\":125000000.0,\"position_us\":42000000.0}"
            }
        });
        let timing: BrowserMediaTiming =
            serde_json::from_str(parse_bidi_string_result(&response).unwrap()).unwrap();
        assert_eq!(timing.length_us, 125_000_000.0);
        assert_eq!(timing.position_us, 42_000_000.0);
    }

    #[test]
    #[ignore = "requires Zen/Firefox listening on WebDriver BiDi port 9222"]
    fn lists_tabs_from_a_live_bidi_browser() {
        let mut backend = BidiBackend::new(None);
        let created = backend
            .command("browsingContext.create", json!({ "type": "tab" }))
            .unwrap();
        let context = created.get("context").and_then(JsonValue::as_str).unwrap();
        backend
            .command(
                "browsingContext.navigate",
                json!({
                    "context": context,
                    "url": "data:text/html,%3Ctitle%3Etiny-dfr%20Zen%20probe%3C/title%3E",
                    "wait": "complete",
                }),
            )
            .unwrap();
        backend.activate_tab(context).unwrap();

        let state = backend.refresh_tabs("tiny-dfr Zen probe", true).unwrap();
        assert!(state.available);
        assert!(state.count >= 1);
        assert!(
            state.tabs_json.contains("tiny-dfr Zen probe"),
            "unexpected tabs: {}",
            state.tabs_json
        );
    }

    #[test]
    fn decodes_html_entities_in_tab_titles() {
        assert_eq!(decode_html_entities("They&#39re here"), "They're here");
        assert_eq!(decode_html_entities("They&#39;re here"), "They're here");
        assert_eq!(
            decode_html_entities("They&#x27;re &amp; gone"),
            "They're & gone"
        );
        assert_eq!(
            decode_html_entities("&quot;Hi&quot; &lt;tag&gt;"),
            "\"Hi\" <tag>"
        );
    }
}
