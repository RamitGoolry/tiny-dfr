//! PipeWire volume bridge (daemon side). The daemon runs as `nobody` and cannot
//! reach the user's PipeWire socket, so the volume slider exchanges plain files
//! under `/run/tiny-dfr` with a user-session helper (`tiny-dfr-volume`):
//!
//!   - `set_level` writes the desired 0.0..=1.0 level to `volume.target`; the
//!     helper applies it with `wpctl set-volume`.
//!   - `level()` reads the live level the helper publishes to `volume.current`
//!     (so the fill and the grab anchor match what the OSD/keys show).
//!
//! `init_ipc` creates the files as root (before the privilege drop) and makes them
//! world-writable, so `nobody` can write `target`/read `current` and the user's
//! helper can read `target`/write `current`. The directory itself comes from the
//! unit's `RuntimeDirectory=tiny-dfr` (writable even under `ProtectSystem=strict`).
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const IPC_DIR: &str = "/run/tiny-dfr";
const TARGET_PATH: &str = "/run/tiny-dfr/volume.target";
const CURRENT_PATH: &str = "/run/tiny-dfr/volume.current";

/// Create the bridge files as root, before the privilege drop. Mode 0666 so both
/// the dropped daemon and the user-session helper can read and write them. Best
/// effort: on failure the volume slider simply does nothing until the bridge is up.
pub fn init_ipc() {
    // The directory is normally provided by `RuntimeDirectory=tiny-dfr`; create it
    // too so a non-systemd launch still works where /run is writable.
    if let Err(e) = fs::create_dir_all(IPC_DIR) {
        eprintln!("volume bridge: cannot create {IPC_DIR} ({e}); volume slider disabled");
        return;
    }
    for path in [TARGET_PATH, CURRENT_PATH] {
        if !Path::new(path).exists() {
            if let Err(e) = fs::write(path, "") {
                eprintln!("volume bridge: cannot create {path}: {e}");
                continue;
            }
        }
        if let Err(e) = fs::set_permissions(path, fs::Permissions::from_mode(0o666)) {
            eprintln!("volume bridge: cannot chmod {path}: {e}");
        }
    }
}

/// File-backed handle to the volume bridge. Cheap to construct; the helper does
/// the real PipeWire work.
pub struct VolumeMixer;

impl VolumeMixer {
    pub fn new() -> VolumeMixer {
        VolumeMixer
    }

    /// Current sink volume in 0.0..=1.0, as published by the helper. Falls back to
    /// 0.0 when the file is missing or caught mid-write (transient — re-read next
    /// frame).
    pub fn level(&self) -> f64 {
        fs::read_to_string(CURRENT_PATH)
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .unwrap_or(0.0)
            .clamp(0.0, 1.0)
    }

    /// Request a new sink volume by writing the target the helper applies.
    pub fn set_level(&self, level: f64) {
        let level = level.clamp(0.0, 1.0);
        if let Err(e) = fs::write(TARGET_PATH, format!("{level:.4}\n")) {
            eprintln!("volume bridge: failed to write target: {e}");
        }
    }
}
