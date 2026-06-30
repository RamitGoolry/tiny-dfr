use anyhow::{Context, Result};
use std::collections::HashMap;
use zbus::{
    blocking::{fdo::DBusProxy, Connection, Proxy},
    zvariant::{OwnedObjectPath, OwnedValue},
};

const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";

#[derive(Clone, Debug, Default)]
pub(crate) struct MediaState {
    pub(crate) player: String,
    pub(crate) track_id: String,
    pub(crate) status: String,
    pub(crate) title: String,
    pub(crate) art_url: String,
    pub(crate) length_us: f64,
    pub(crate) position_us: f64,
}

pub(crate) struct MprisClient {
    conn: Option<Connection>,
    active_player: Option<String>,
    active_track_id: Option<String>,
}

impl MprisClient {
    pub(crate) fn new() -> MprisClient {
        let conn = Connection::session()
            .inspect_err(|e| eprintln!("mpris: failed to connect to session bus: {e}"))
            .ok();
        MprisClient {
            conn,
            active_player: None,
            active_track_id: None,
        }
    }

    pub(crate) fn refresh(&mut self) -> Result<Option<MediaState>> {
        let Some(conn) = &self.conn else {
            return Ok(None);
        };
        let dbus = DBusProxy::new(conn).context("creating DBus proxy")?;
        let names = dbus.list_names().context("listing DBus names")?;
        let players: Vec<String> = names
            .into_iter()
            .map(|name| name.to_string())
            .filter(|name| name.starts_with("org.mpris.MediaPlayer2."))
            .collect();

        let mut fallback = None;
        for player in players {
            let Ok(state) = read_player(conn, &player) else {
                continue;
            };
            if state.status == "Playing" {
                self.active_player = Some(player);
                self.active_track_id = (!state.track_id.is_empty()).then(|| state.track_id.clone());
                return Ok(Some(state));
            }
            if fallback.is_none() && !state.art_url.is_empty() && state.length_us > 0.0 {
                fallback = Some((player, state));
            }
        }

        if let Some((player, state)) = fallback {
            self.active_player = Some(player);
            self.active_track_id = (!state.track_id.is_empty()).then(|| state.track_id.clone());
            Ok(Some(state))
        } else {
            self.active_player = None;
            self.active_track_id = None;
            Ok(None)
        }
    }

    pub(crate) fn previous(&self) -> Result<()> {
        self.call_active("Previous")
    }

    pub(crate) fn play_pause(&self) -> Result<()> {
        self.call_active("PlayPause")
    }

    pub(crate) fn next(&self) -> Result<()> {
        self.call_active("Next")
    }

    pub(crate) fn seek(&self, position_us: f64) -> Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        let Some(player) = &self.active_player else {
            return Ok(());
        };
        let Some(track_id) = &self.active_track_id else {
            return Ok(());
        };
        let track_id = OwnedObjectPath::try_from(track_id.as_str())?;
        let proxy = player_proxy(conn, player)?;
        proxy.call::<_, _, ()>("SetPosition", &(track_id, position_us.max(0.0) as i64))?;
        Ok(())
    }

    fn call_active(&self, method: &str) -> Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        let Some(player) = &self.active_player else {
            return Ok(());
        };
        let proxy = player_proxy(conn, player)?;
        proxy.call::<_, _, ()>(method, &())?;
        Ok(())
    }
}

fn player_proxy(conn: &Connection, player: &str) -> Result<Proxy<'static>> {
    Proxy::new_owned(
        conn.clone(),
        player.to_string(),
        MPRIS_PATH.to_string(),
        PLAYER_IFACE.to_string(),
    )
    .context("creating MPRIS player proxy")
}

fn read_player(conn: &Connection, player: &str) -> Result<MediaState> {
    let proxy = player_proxy(conn, player)?;
    let status: String = proxy.get_property("PlaybackStatus").unwrap_or_default();
    let position_us: i64 = proxy.get_property("Position").unwrap_or_default();
    let metadata: HashMap<String, OwnedValue> = proxy.get_property("Metadata").unwrap_or_default();
    let track_id = metadata
        .get("mpris:trackid")
        .and_then(|value| OwnedObjectPath::try_from(value.clone()).ok())
        .map(|path| path.to_string())
        .unwrap_or_default();
    let title = metadata
        .get("xesam:title")
        .and_then(|value| String::try_from(value.clone()).ok())
        .unwrap_or_default();
    let art_url = metadata
        .get("mpris:artUrl")
        .and_then(|value| String::try_from(value.clone()).ok())
        .unwrap_or_default();
    let length_us = metadata
        .get("mpris:length")
        .and_then(|value| i64::try_from(value.clone()).ok())
        .unwrap_or_default();

    Ok(MediaState {
        player: player.to_string(),
        track_id,
        status,
        title,
        art_url,
        length_us: length_us.max(0) as f64,
        position_us: position_us.max(0) as f64,
    })
}
