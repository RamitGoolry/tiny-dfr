use input_linux::evdev::EvdevHandle;
use input_linux_sys::{
    ev_get_abs, input_absinfo, input_event, ABS_X, BTN_TOUCH, EV_ABS, EV_KEY, EV_SYN, SYN_REPORT,
};
use std::{
    fs::{self, File, OpenOptions},
    mem,
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd},
        unix::fs::OpenOptionsExt,
    },
    path::PathBuf,
    time::{Duration, Instant},
};

/// The T1 (2017) Touch Bar touch surface, as exposed in USB config 2.
const DIGITIZER_NAME: &str = "Apple Inc. iBridge Touchpad";
/// After BTN_TOUCH drops, how long the position stream must stay quiet before we
/// call the touch released. A still-hold keeps BTN_TOUCH=1 (never times out); a
/// moving lift releases this long after the last motion.
const RELEASE_TIMEOUT: Duration = Duration::from_millis(150);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TouchPhase {
    Down,
    Motion,
    Up,
}

pub struct TouchSample {
    pub phase: TouchPhase,
    /// Bar coordinates: x in 0..width, y a fixed centre (the surface only reports
    /// a usable X anyway).
    pub x: f64,
    pub y: f64,
}

/// Reads the Touch Bar digitizer straight from evdev. We do this because the
/// device's BTN_TOUCH signalling is broken — it drops to 0 the instant a finger
/// moves, even mid-drag — so libinput discards all the drag motion. Here ABS_X is
/// the source of truth, and a touch is only released when BTN_TOUCH is 0 *and*
/// motion has gone quiet for RELEASE_TIMEOUT.
pub struct TouchReader {
    evdev: EvdevHandle<File>,
    abs_min: i32,
    abs_span: f64,
    width: f64,
    y_center: f64,

    down: bool,
    btn_touch: bool,
    last_x: i32,
    pending_x: i32,
    last_activity: Instant,
}

impl TouchReader {
    /// Find + open the digitizer (non-blocking) and read its ABS_X range. Returns
    /// None if the device isn't present or can't be opened.
    pub fn open(width: u16, height: u16) -> Option<TouchReader> {
        let path = find_digitizer()?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .ok()?;
        let evdev = EvdevHandle::new(file);
        let mut info: input_absinfo = unsafe { mem::zeroed() };
        if unsafe { ev_get_abs(evdev.as_raw_fd(), ABS_X as u32, &mut info) }.is_err() {
            return None;
        }
        Some(TouchReader {
            evdev,
            abs_min: info.minimum,
            abs_span: (info.maximum - info.minimum).max(1) as f64,
            width: width as f64,
            y_center: height as f64 / 2.0,
            down: false,
            btn_touch: false,
            last_x: info.minimum,
            pending_x: info.minimum,
            last_activity: Instant::now(),
        })
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.evdev.as_fd()
    }

    fn map_x(&self, abs_x: i32) -> f64 {
        (((abs_x - self.abs_min) as f64 / self.abs_span) * self.width).clamp(0.0, self.width)
    }

    /// Drain pending evdev events, emitting Down/Motion samples into `out`.
    pub fn poll(&mut self, out: &mut Vec<TouchSample>) {
        let mut buf: [input_event; 64] = unsafe { mem::zeroed() };
        loop {
            let n = match self.evdev.read(&mut buf) {
                Ok(n) if n > 0 => n,
                _ => break,
            };
            for ev in &buf[..n] {
                let (ty, code) = (ev.type_ as i32, ev.code as i32);
                if ty == EV_ABS && code == ABS_X {
                    self.pending_x = ev.value;
                } else if ty == EV_KEY && code == BTN_TOUCH {
                    self.btn_touch = ev.value != 0;
                } else if ty == EV_SYN && code == SYN_REPORT {
                    self.end_frame(out);
                }
            }
            if n < buf.len() {
                break;
            }
        }
    }

    fn end_frame(&mut self, out: &mut Vec<TouchSample>) {
        if self.btn_touch && !self.down {
            self.down = true;
            self.last_x = self.pending_x;
            self.last_activity = Instant::now();
            out.push(TouchSample {
                phase: TouchPhase::Down,
                x: self.map_x(self.pending_x),
                y: self.y_center,
            });
        } else if self.down && self.pending_x != self.last_x {
            self.last_x = self.pending_x;
            self.last_activity = Instant::now();
            out.push(TouchSample {
                phase: TouchPhase::Motion,
                x: self.map_x(self.pending_x),
                y: self.y_center,
            });
        } else if self.btn_touch {
            // Held still: BTN_TOUCH is asserted but nothing moved — keep alive.
            self.last_activity = Instant::now();
        }
    }

    /// Call once per loop iteration: releases the touch if BTN_TOUCH is down and
    /// motion has been quiet past the timeout. Returns the Up sample when it fires.
    pub fn check_release(&mut self) -> Option<TouchSample> {
        if self.down && !self.btn_touch && self.last_activity.elapsed() > RELEASE_TIMEOUT {
            self.down = false;
            return Some(TouchSample {
                phase: TouchPhase::Up,
                x: self.map_x(self.last_x),
                y: self.y_center,
            });
        }
        None
    }
}

fn find_digitizer() -> Option<PathBuf> {
    for entry in fs::read_dir("/sys/class/input").ok()?.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("event") {
            continue;
        }
        if let Ok(dev_name) = fs::read_to_string(entry.path().join("device/name")) {
            if dev_name.trim() == DIGITIZER_NAME {
                return Some(PathBuf::from(format!(
                    "/dev/input/{}",
                    name.to_string_lossy()
                )));
            }
        }
    }
    None
}
