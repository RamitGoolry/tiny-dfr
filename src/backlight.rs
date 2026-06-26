use crate::config::Config;
use crate::TIMEOUT_MS;
use anyhow::{anyhow, Result};
use input::event::{
    switch::{Switch, SwitchEvent, SwitchState},
    Event,
};
use std::{
    cmp::min,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::Instant,
};

const MAX_DISPLAY_BRIGHTNESS: u32 = 509;
const MAX_TOUCH_BAR_BRIGHTNESS: u32 = 255;
const BRIGHTNESS_DIM_TIMEOUT: i32 = TIMEOUT_MS * 3; // should be a multiple of TIMEOUT_MS
const BRIGHTNESS_OFF_TIMEOUT: i32 = TIMEOUT_MS * 6; // should be a multiple of TIMEOUT_MS
const DIMMED_BRIGHTNESS: u32 = 1;

fn read_attr(path: &Path, attr: &str) -> u32 {
    fs::read_to_string(path.join(attr))
        .unwrap_or_else(|_| panic!("Failed to read {attr}"))
        .trim()
        .parse::<u32>()
        .unwrap_or_else(|_| panic!("Failed to parse {attr}"))
}

/// Like `read_attr` but never panics — for hot paths (e.g. the slider polling
/// the current brightness) where a transient read failure should be tolerated.
fn try_read_attr(path: &Path, attr: &str) -> Option<u32> {
    fs::read_to_string(path.join(attr))
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
}

fn find_backlight() -> Result<PathBuf> {
    for entry in fs::read_dir("/sys/class/backlight/")? {
        let entry = entry?;
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();

        if ["display-pipe", "228600000.dsi.0", "appletb_backlight"]
            .iter()
            .any(|s| name.contains(s))
        {
            return Ok(entry.path());
        }
    }
    Err(anyhow!("No Touch Bar backlight device found"))
}

fn find_display_backlight() -> Result<PathBuf> {
    for entry in fs::read_dir("/sys/class/backlight/")? {
        let entry = entry?;
        if [
            "apple-panel-bl",
            "gmux_backlight",
            "intel_backlight",
            "acpi_video0",
        ]
        .iter()
        .any(|s| entry.file_name().to_string_lossy().contains(s))
        {
            return Ok(entry.path());
        }
    }
    Err(anyhow!("No Built-in Retina Display backlight device found"))
}

fn set_backlight(mut file: &File, value: u32) {
    file.write_all(format!("{}\n", value).as_bytes()).unwrap();
}

pub struct BacklightManager {
    last_active: Instant,
    max_bl: u32,
    current_bl: u32,
    lid_state: SwitchState,
    bl_file: Option<File>,
    display_bl_path: Option<PathBuf>,
    display_bl_file: Option<File>,
    display_max_bl: u32,
}

impl BacklightManager {
    pub fn new() -> BacklightManager {
        // T1 Macs (appletbdrm) have no dedicated Touch Bar backlight sysfs device;
        // tolerate its absence and just skip Touch Bar brightness control there.
        let bl_path = find_backlight()
            .inspect_err(|e| {
                eprintln!("No Touch Bar backlight device, brightness control disabled: {e}")
            })
            .ok();
        let display_bl_path = find_display_backlight()
            .inspect_err(|e| eprintln!("Failed to find display backlight sysfs path: {e}"))
            .ok();
        let bl_file = bl_path.as_ref().map(|p| {
            OpenOptions::new()
                .write(true)
                .open(p.join("brightness"))
                .unwrap()
        });
        let (max_bl, current_bl) = match &bl_path {
            Some(p) => (read_attr(p, "max_brightness"), read_attr(p, "brightness")),
            None => (MAX_TOUCH_BAR_BRIGHTNESS, MAX_TOUCH_BAR_BRIGHTNESS),
        };
        // Display backlight write handle for the brightness slider, opened here
        // (before the privilege drop) so the daemon can still write it as nobody.
        let display_bl_file = display_bl_path.as_ref().and_then(|p| {
            OpenOptions::new()
                .write(true)
                .open(p.join("brightness"))
                .ok()
        });
        let display_max_bl = display_bl_path
            .as_ref()
            .map(|p| read_attr(p, "max_brightness"))
            .unwrap_or(MAX_DISPLAY_BRIGHTNESS);
        BacklightManager {
            bl_file,
            lid_state: SwitchState::Off,
            max_bl,
            current_bl,
            last_active: Instant::now(),
            display_bl_path,
            display_bl_file,
            display_max_bl,
        }
    }
    fn display_to_touchbar(display: u32, active_brightness: u32) -> u32 {
        let normalized = display as f64 / MAX_DISPLAY_BRIGHTNESS as f64;
        // Add one so that the touch bar does not turn off
        let adjusted = (normalized.powf(0.5) * active_brightness as f64) as u32 + 1;
        adjusted.min(MAX_TOUCH_BAR_BRIGHTNESS) // Clamp the value to the maximum allowed brightness
    }
    pub fn process_event(&mut self, event: &Event) {
        match event {
            Event::Keyboard(_) | Event::Pointer(_) | Event::Gesture(_) | Event::Touch(_) => {
                self.last_active = Instant::now();
            }
            Event::Switch(SwitchEvent::Toggle(toggle)) => {
                if let Some(Switch::Lid) = toggle.switch() {
                    self.lid_state = toggle.switch_state();
                    println!("Lid Switch event: {:?}", self.lid_state);
                    if toggle.switch_state() == SwitchState::Off {
                        self.last_active = Instant::now();
                    }
                }
            }
            _ => {}
        }
    }
    /// Mark activity so the bar stays lit — called for raw digitizer touches,
    /// which no longer go through libinput's `process_event`.
    pub fn wake(&mut self) {
        self.last_active = Instant::now();
    }
    pub fn update_backlight(&mut self, cfg: &Config) {
        let since_last_active = (Instant::now() - self.last_active).as_millis() as u64;
        let new_bl = min(
            self.max_bl,
            if self.lid_state == SwitchState::On {
                0
            } else if since_last_active < BRIGHTNESS_DIM_TIMEOUT as u64 {
                if cfg.adaptive_brightness {
                    let brightness = if let Some(path) = &self.display_bl_path {
                        read_attr(path, "brightness")
                    } else {
                        self.max_bl / 2
                    };
                    BacklightManager::display_to_touchbar(brightness, cfg.active_brightness)
                } else {
                    cfg.active_brightness
                }
            } else if since_last_active < BRIGHTNESS_OFF_TIMEOUT as u64 {
                DIMMED_BRIGHTNESS
            } else {
                0
            },
        );
        if self.current_bl != new_bl {
            self.current_bl = new_bl;
            if let Some(file) = &self.bl_file {
                set_backlight(file, self.current_bl);
            }
        }
    }
    pub fn current_bl(&self) -> u32 {
        self.current_bl
    }
    /// Current display backlight level in 0.0..=1.0, read fresh from sysfs.
    pub fn display_level(&self) -> f64 {
        let cur = self
            .display_bl_path
            .as_ref()
            .and_then(|p| try_read_attr(p, "brightness"))
            .unwrap_or(0);
        (cur as f64 / self.display_max_bl as f64).clamp(0.0, 1.0)
    }
    /// Set the display backlight from a 0.0..=1.0 level (clamped just above off
    /// so the slider can never black the screen out completely).
    pub fn set_display_level(&self, level: f64) {
        if let Some(file) = &self.display_bl_file {
            let value = (level.clamp(0.0, 1.0) * self.display_max_bl as f64).round() as u32;
            set_backlight(file, value.max(1));
        }
    }
}
