//! Keyboard-backlight LED control for the illumination slider. The same shape as
//! the display backlight in `backlight.rs`: a sysfs `brightness`/`max_brightness`
//! pair under `/sys/class/leds/*kbd_backlight*`. The write fd is opened as root in
//! `real_main` (before the privilege drop) so the daemon can still write it as
//! `nobody`. All fields are `Option`-tolerant so machines without the LED just
//! disable the control instead of panicking.
use anyhow::{anyhow, Result};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

fn find_kbd_backlight() -> Result<PathBuf> {
    for entry in fs::read_dir("/sys/class/leds/")? {
        let entry = entry?;
        if entry
            .file_name()
            .to_string_lossy()
            .contains("kbd_backlight")
        {
            return Ok(entry.path());
        }
    }
    Err(anyhow!("No keyboard backlight LED device found"))
}

/// Like `backlight::try_read_attr` — never panics, for the hot read path.
fn try_read_attr(path: &Path, attr: &str) -> Option<u32> {
    fs::read_to_string(path.join(attr))
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
}

pub struct KbdBacklight {
    /// LED sysfs dir, kept for reading `brightness` fresh each `level()`.
    path: Option<PathBuf>,
    /// Write handle to `brightness`, opened as root before the privilege drop.
    file: Option<File>,
    max: u32,
}

impl KbdBacklight {
    pub fn new() -> KbdBacklight {
        let path = find_kbd_backlight()
            .inspect_err(|e| {
                eprintln!("No keyboard backlight LED, illumination control disabled: {e}")
            })
            .ok();
        let file = path
            .as_ref()
            .and_then(|p| OpenOptions::new().write(true).open(p.join("brightness")).ok());
        let max = path
            .as_ref()
            .and_then(|p| try_read_attr(p, "max_brightness"))
            .unwrap_or(255);
        KbdBacklight { path, file, max }
    }

    /// Current keyboard backlight level in 0.0..=1.0, read fresh from sysfs.
    pub fn level(&self) -> f64 {
        let cur = self
            .path
            .as_ref()
            .and_then(|p| try_read_attr(p, "brightness"))
            .unwrap_or(0);
        (cur as f64 / self.max as f64).clamp(0.0, 1.0)
    }

    /// Set the keyboard backlight from a 0.0..=1.0 level. Unlike the display, 0 is
    /// a valid value (keyboard light fully off), so it is not floored to 1.
    pub fn set_level(&self, level: f64) {
        if let Some(mut file) = self.file.as_ref() {
            let value = (level.clamp(0.0, 1.0) * self.max as f64).round() as u32;
            let _ = file.write_all(format!("{}\n", value).as_bytes());
        }
    }
}
