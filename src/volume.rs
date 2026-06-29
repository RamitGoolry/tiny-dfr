//! System volume control for the volume slider, via the ALSA hardware mixer.
//! Unlike brightness/keyboard-illumination (sysfs files opened as root before the
//! privilege drop), the mixer talks to `/dev/snd/controlC0`, which is reachable as
//! `nobody` once the daemon joins the `audio` group (see the PrivDrop group list in
//! `real_main`). The `Master` element is resolved fresh on every call rather than
//! cached, because an `alsa::mixer::Selem` borrows its `Mixer` and would otherwise
//! make this struct self-referential.
//!
//! Caveat: this drives the *hardware* `Master`. On systems where PipeWire/Pulse
//! presents its own software volume, that may differ from what the user perceives
//! as "system volume".
use alsa::mixer::{Mixer, SelemChannelId, SelemId};

/// Candidate simple-control names, tried in order; the first one that exists and
/// carries a playback volume wins. `Master` is the desktop norm, but Apple
/// internal codecs (e.g. CS8409/CS42L83) expose only `PCM`, so it must be in the
/// list for the slider to work on this hardware.
const CANDIDATE_CONTROLS: &[&str] = &["Master", "PCM", "Speaker", "Headphone", "Digital"];

struct Inner {
    mixer: Mixer,
    selem_id: SelemId,
    min: i64,
    max: i64,
}

pub struct VolumeMixer {
    inner: Option<Inner>,
}

impl VolumeMixer {
    pub fn new() -> VolumeMixer {
        VolumeMixer {
            inner: Self::try_open(),
        }
    }

    fn try_open() -> Option<Inner> {
        let mixer = Mixer::new("hw:0", false).ok()?;
        for &name in CANDIDATE_CONTROLS {
            let selem_id = SelemId::new(name, 0);
            // The Selem borrows `mixer` only for this statement (it is a temporary
            // dropped at the `;`), so `mixer` is free to move into `Inner` below.
            let range = mixer
                .find_selem(&selem_id)
                .filter(|s| s.has_playback_volume())
                .map(|s| s.get_playback_volume_range());
            if let Some((min, max)) = range {
                eprintln!("volume slider: using ALSA control '{name}'");
                return Some(Inner {
                    mixer,
                    selem_id,
                    min,
                    max,
                });
            }
        }
        eprintln!("volume slider disabled: no playback-volume control found on hw:0");
        None
    }

    /// Current Master volume in 0.0..=1.0, read fresh (after pumping mixer events
    /// so external changes are reflected).
    pub fn level(&self) -> f64 {
        let Some(inner) = &self.inner else {
            return 0.0;
        };
        let _ = inner.mixer.handle_events();
        let Some(selem) = inner.mixer.find_selem(&inner.selem_id) else {
            return 0.0;
        };
        let span = (inner.max - inner.min) as f64;
        if span <= 0.0 {
            return 0.0;
        }
        let vol = selem
            .get_playback_volume(SelemChannelId::FrontLeft)
            .unwrap_or(inner.min);
        ((vol - inner.min) as f64 / span).clamp(0.0, 1.0)
    }

    /// Set the Master volume from a 0.0..=1.0 level, unmuting if the element has a
    /// playback switch (dragging the slider implies the user wants it audible).
    pub fn set_level(&self, level: f64) {
        let Some(inner) = &self.inner else {
            return;
        };
        let Some(selem) = inner.mixer.find_selem(&inner.selem_id) else {
            return;
        };
        let span = (inner.max - inner.min) as f64;
        let value = inner.min + (level.clamp(0.0, 1.0) * span).round() as i64;
        let _ = selem.set_playback_volume_all(value);
        if selem.has_playback_switch() {
            let _ = selem.set_playback_switch_all(1);
        }
    }
}
