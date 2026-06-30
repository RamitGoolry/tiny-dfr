use drm::control::ClipRect;
use ::input::Libinput;
use input_linux::uinput::UInputHandle;
use nix::{
    errno::Errno,
    sys::{
        epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags},
        signal::{SigSet, Signal},
    },
};
use privdrop::PrivDrop;
use std::{
    fs::OpenOptions,
    os::fd::AsFd,
    panic::{self, AssertUnwindSafe},
    time::Instant,
};
use udev::MonitorBuilder;

mod action;
mod app;
mod backlight;
mod battery;
mod config;
mod display;
mod fonts;
mod function_layer;
mod input;
mod kbd_backlight;
mod layer;
mod pixel_shift;
mod state;
mod touch;
mod volume;
mod widgets;

use crate::app::App;
use crate::config::ConfigManager;
use crate::input::Interface;
use crate::kbd_backlight::KbdBacklight;
use backlight::BacklightManager;
use display::DrmBackend;
use touch::TouchReader;

const BUTTON_SPACING_PX: i32 = 16;
const BUTTON_COLOR_INACTIVE: f64 = 0.200;
const BUTTON_COLOR_ACTIVE: f64 = 0.400;
const DEFAULT_ICON_SIZE: i32 = 48;
const TIMEOUT_MS: i32 = 10 * 1000;
/// While a touch is in flight, cap the epoll wait this low so the time-based touch
/// release (TouchReader::check_release) fires promptly instead of sleeping out
/// TIMEOUT_MS. Keep it <= the reader's RELEASE_TIMEOUT.
const TOUCH_ACTIVE_POLL_MS: i32 = 16;

/// Wall-clock epoch seconds for [dbg] log timestamps — lines up with evtest's
/// `time …` stamps so device events and daemon handling can be correlated.
pub(crate) fn dbg_ts() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn main() {
    let mut drm = DrmBackend::open_card().expect("failed to open the Touch Bar DRM device");
    let (height, width) = drm.mode().size();
    let _ = panic::catch_unwind(AssertUnwindSafe(|| real_main(&mut drm)));
    let crash_bitmap = include_bytes!("crash_bitmap.raw");
    let mut map = drm.map().expect("crash handler: failed to map the DRM framebuffer");
    let data = map.as_mut();
    let mut wptr = 0;
    for byte in crash_bitmap {
        for i in 0..8 {
            let bit = ((byte >> i) & 0x1) == 0;
            let color = if bit { 0xFF } else { 0x0 };
            data[wptr] = color;
            data[wptr + 1] = color;
            data[wptr + 2] = color;
            data[wptr + 3] = color;
            wptr += 4;
        }
    }
    drop(map);
    drm.dirty(&[ClipRect::new(0, 0, height, width)])
        .expect("crash handler: failed to flush the crash screen");
    let mut sigset = SigSet::empty();
    sigset.add(Signal::SIGTERM);
    sigset.wait().expect("failed to wait on SIGTERM");
}


fn real_main(drm: &mut DrmBackend) {
    let (height, width) = drm.mode().size();
    let (db_width, db_height) = drm
        .fb_info()
        .expect("failed to query DRM framebuffer info")
        .size();
    // Root resources: opened before the privilege drop so their fds survive as
    // `nobody`. (drm is already open; uinput + the raw digitizer open here.)
    let uinput = UInputHandle::new(
        OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .expect("failed to open /dev/uinput (is the uinput kernel module loaded?)"),
    );
    let backlight = BacklightManager::new();
    // Keyboard backlight LED write fd, opened as root so it survives as `nobody`.
    let kbd = KbdBacklight::new();
    // Volume bridge IPC files, created as root so `nobody` can use them after the
    // drop; the user-session helper applies them to PipeWire. See volume.rs.
    volume::init_ipc();
    // The T1 digitizer is read raw (see touch.rs) — libinput mangles its drags.
    let mut touch_reader = TouchReader::open(width, height);
    let mut cfg_mgr = ConfigManager::new();

    // drop privileges to the input and video groups. (Volume goes through the
    // file bridge in volume.rs now, so no audio-group / ALSA access is needed.)
    let groups = ["input", "video"];

    PrivDrop::default()
        .user("nobody")
        .group_list(&groups)
        .apply()
        .unwrap_or_else(|e| panic!("Failed to drop privileges: {}", e));

    // App owns the bar state + dispatch; its constructor runs the uinput device
    // setup, which must stay AFTER the privilege drop above.
    let mut app = App::new(&cfg_mgr, width, db_width, db_height, uinput, backlight, kbd);

    let mut input_main = Libinput::new_with_udev(Interface);
    input_main
        .udev_assign_seat("seat0")
        .expect("failed to assign libinput to udev seat0");
    let udev_monitor = MonitorBuilder::new()
        .expect("failed to create the udev monitor")
        .match_subsystem("power_supply")
        .expect("failed to filter the udev monitor to power_supply")
        .listen()
        .expect("failed to start the udev monitor");
    let epoll = Epoll::new(EpollCreateFlags::empty()).expect("failed to create the epoll instance");
    epoll
        .add(input_main.as_fd(), EpollEvent::new(EpollFlags::EPOLLIN, 0))
        .expect("failed to register the libinput fd with epoll");
    epoll
        .add(cfg_mgr.fd(), EpollEvent::new(EpollFlags::EPOLLIN, 2))
        .expect("failed to register the config-watch fd with epoll");
    epoll
        .add(&udev_monitor, EpollEvent::new(EpollFlags::EPOLLIN, 3))
        .expect("failed to register the udev monitor with epoll");
    if let Some(reader) = &touch_reader {
        epoll
            .add(reader.as_fd(), EpollEvent::new(EpollFlags::EPOLLIN, 4))
            .expect("failed to register the digitizer fd with epoll");
    }

    loop {
        app.reload_config(&mut cfg_mgr, width);
        app.resolve_and_log();

        let touch_down = touch_reader.as_ref().is_some_and(|r| r.is_down());
        let next_timeout_ms = app.next_timeout(touch_down);

        // The world the widgets render from, snapshotted once per iteration and
        // threaded into both the draw and the touch dispatch.
        let state = app.state();

        app.tick();
        app.render(drm, &state);

        match epoll.wait(
            &mut [EpollEvent::new(EpollFlags::EPOLLIN, 0)],
            next_timeout_ms as u16,
        ) {
            Err(Errno::EINTR) | Ok(_) => 0,
            e => e.expect("epoll wait failed"),
        };

        _ = udev_monitor.iter().last();

        let t_in = Instant::now();
        input_main.dispatch().expect("libinput dispatch failed");
        let t_dispatch = t_in.elapsed();
        let mut n_events = 0u32;
        for event in &mut input_main.clone() {
            n_events += 1;
            app.on_libinput(event);
        }
        let t_drain = t_in.elapsed();
        // Only log when the input path itself is slow, to catch the stall bursts.
        if t_drain.as_millis() > 50 || n_events > 100 {
            eprintln!(
                "[dbg {:.6}] INPUT dispatch={}ms drain_total={}ms events={}",
                dbg_ts(),
                t_dispatch.as_millis(),
                t_drain.as_millis(),
                n_events
            );
        }

        // ----- Touch Bar digitizer: read raw and drive the dispatch (touch.rs).
        // Single-touch, so we use slot 0. libinput can't track this device's drags.
        if let Some(reader) = &mut touch_reader {
            let mut samples = Vec::new();
            reader.poll(&mut samples);
            if let Some(up) = reader.check_release() {
                samples.push(up);
            }
            for s in samples {
                app.on_touch(s, width, height, &state);
            }
        }
        app.update_backlight();
    }
}
