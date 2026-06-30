//! In-process volume control via PipeWire's `wpctl`. Possible now that tiny-dfr
//! runs in the user session (it inherits `XDG_RUNTIME_DIR` and can reach the
//! PipeWire socket). `wpctl` is used rather than a native binding so the slider's
//! 0..1 maps to the exact value the keys and the OSD use — no curve mismatch.
//!
//! Reads (the grab anchor) are synchronous and rare. Writes go through a
//! background thread that coalesces a fast drag down to the newest value, so we
//! don't spawn a `wpctl` per drag sample and the final value still lands.
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;

const SINK: &str = "@DEFAULT_AUDIO_SINK@";

pub struct VolumeMixer {
    tx: Sender<f64>,
}

impl VolumeMixer {
    pub fn new() -> VolumeMixer {
        let (tx, rx) = mpsc::channel::<f64>();
        thread::spawn(move || apply_loop(rx));
        VolumeMixer { tx }
    }

    /// Request a new sink volume (0.0..=1.0). Non-blocking — the apply thread
    /// pushes the newest value to PipeWire.
    pub fn set_level(&self, level: f64) {
        let _ = self.tx.send(level.clamp(0.0, 1.0));
    }
}

/// Block for a value, coalesce to the newest queued one, apply it, repeat. While
/// `wpctl` runs, further drag samples queue; the next iteration collapses them to
/// the latest, so the apply rate is bounded by `wpctl`'s own runtime.
fn apply_loop(rx: Receiver<f64>) {
    while let Ok(mut level) = rx.recv() {
        loop {
            match rx.try_recv() {
                Ok(l) => level = l,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        let _ = Command::new("wpctl")
            .args(["set-volume", "-l", "1.0", SINK, &format!("{level:.4}")])
            .status();
    }
}

/// Current sink volume (0.0..=1.0), read synchronously. Called only on slider
/// grab. Returns 0.0 if `wpctl` is missing or its output can't be parsed.
pub fn current_level() -> f64 {
    let out = match Command::new("wpctl").args(["get-volume", SINK]).output() {
        Ok(o) => o,
        Err(_) => return 0.0,
    };
    // "Volume: 0.52"  or  "Volume: 0.52 [MUTED]"
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0)
        .clamp(0.0, 1.0)
}
