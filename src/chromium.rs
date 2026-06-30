use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:9222";
const POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ChromiumTab {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) url: String,
    pub(crate) favicon_url: String,
    pub(crate) active: bool,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ChromiumTabsState {
    pub(crate) available: bool,
    pub(crate) tabs_json: String,
    pub(crate) count: usize,
    pub(crate) active_index: Option<usize>,
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

pub(crate) struct ChromiumClient {
    endpoint: String,
    last_refresh: Option<Instant>,
    cached: ChromiumTabsState,
    /// Stable local tab ordering. CDP `/json/list` is not guaranteed to match
    /// Chromium's tab strip and can shuffle by recency/activity; keep known
    /// target IDs in-place, drop closed tabs, and append newly-seen tabs.
    tab_order: Vec<String>,
}

impl ChromiumClient {
    pub(crate) fn new(endpoint: Option<String>) -> ChromiumClient {
        ChromiumClient {
            endpoint: endpoint.unwrap_or_else(|| DEFAULT_ENDPOINT.to_string()),
            last_refresh: None,
            cached: ChromiumTabsState {
                available: false,
                tabs_json: "[]".to_string(),
                count: 0,
                active_index: None,
            },
            tab_order: Vec::new(),
        }
    }

    pub(crate) fn refresh_tabs(
        &mut self,
        focused_title: &str,
        force: bool,
    ) -> Result<ChromiumTabsState> {
        let now = Instant::now();
        if !force
            && self
                .last_refresh
                .is_some_and(|last| now.duration_since(last) < POLL_INTERVAL)
        {
            return Ok(self.cached.clone());
        }
        self.last_refresh = Some(now);

        match self.fetch_tabs(focused_title) {
            Ok(state) => {
                self.cached = state;
                Ok(self.cached.clone())
            }
            Err(err) => {
                self.cached = ChromiumTabsState {
                    available: false,
                    tabs_json: "[]".to_string(),
                    count: 0,
                    active_index: None,
                };
                Err(err)
            }
        }
    }

    pub(crate) fn unavailable_state(&self) -> ChromiumTabsState {
        ChromiumTabsState {
            available: false,
            tabs_json: "[]".to_string(),
            count: 0,
            active_index: None,
        }
    }

    pub(crate) fn activate_tab(&self, id: &str) -> Result<()> {
        let url = format!("{}/json/activate/{id}", self.endpoint());
        ureq::get(&url)
            .call()
            .with_context(|| format!("activating Chromium tab `{id}` via {url}"))?;
        Ok(())
    }

    fn fetch_tabs(&mut self, focused_title: &str) -> Result<ChromiumTabsState> {
        let url = format!("{}/json/list", self.endpoint());
        let body = ureq::get(&url)
            .header("Accept", "application/json")
            .call()
            .with_context(|| format!("fetching Chromium DevTools targets from {url}"))?
            .body_mut()
            .read_to_string()
            .context("reading Chromium DevTools response body")?;
        let targets: Vec<DevToolsTarget> =
            serde_json::from_str(&body).context("parsing Chromium DevTools target list")?;

        let mut tabs: Vec<ChromiumTab> = targets
            .into_iter()
            .filter(|target| target.target_type == "page")
            .map(|target| {
                let title = decode_html_entities(&target.title.unwrap_or_default());
                ChromiumTab {
                    active: is_probably_active_tab(&title, focused_title),
                    id: target.id,
                    title,
                    url: target.url.unwrap_or_default(),
                    favicon_url: target.favicon_url.unwrap_or_default(),
                }
            })
            .collect();

        tabs = self.stable_order(tabs);

        if !tabs.iter().any(|tab| tab.active) && tabs.len() == 1 {
            if let Some(tab) = tabs.first_mut() {
                tab.active = true;
            }
        }
        let active_index = tabs.iter().position(|tab| tab.active);
        let tabs_json = serde_json::to_string(&tabs).context("serializing Chromium tabs")?;
        Ok(ChromiumTabsState {
            available: true,
            tabs_json,
            count: tabs.len(),
            active_index,
        })
    }

    fn stable_order(&mut self, tabs: Vec<ChromiumTab>) -> Vec<ChromiumTab> {
        let incoming_ids: Vec<String> = tabs.iter().map(|tab| tab.id.clone()).collect();
        let mut by_id: HashMap<String, ChromiumTab> =
            tabs.into_iter().map(|tab| (tab.id.clone(), tab)).collect();
        let live_ids: HashSet<String> = by_id.keys().cloned().collect();

        // Drop closed tabs from the remembered order.
        self.tab_order.retain(|id| live_ids.contains(id));

        // Append newly discovered tabs in the order CDP returned them this time.
        for id in incoming_ids {
            if !self.tab_order.contains(&id) {
                self.tab_order.push(id);
            }
        }

        self.tab_order
            .iter()
            .filter_map(|id| by_id.remove(id))
            .collect()
    }

    fn endpoint(&self) -> &str {
        self.endpoint.trim_end_matches('/')
    }
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

fn is_probably_active_tab(tab_title: &str, focused_title: &str) -> bool {
    let tab_title = tab_title.trim();
    let focused_title = focused_title.trim();
    !tab_title.is_empty()
        && !focused_title.is_empty()
        && (tab_title == focused_title
            || focused_title.contains(tab_title)
            || tab_title.contains(focused_title))
}

#[cfg(test)]
mod tests {
    use super::decode_html_entities;

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
