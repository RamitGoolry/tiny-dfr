use ::input::Libinput;
use drm::control::ClipRect;
use input_linux::uinput::UInputHandle;
use nix::{
    errno::Errno,
    sys::{
        epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags},
        signal::{SigSet, Signal},
    },
};
use std::{
    fs::OpenOptions,
    os::fd::AsFd,
    panic::{self, AssertUnwindSafe},
    time::Instant,
};
use udev::MonitorBuilder;

mod action;
mod app;
mod app_bridge;
mod backlight;
mod battery;
mod chromium;
mod config;
mod context;
mod display;
mod event;
mod fonts;
mod function_layer;
mod input;
mod kbd_backlight;
mod layer;
mod mpris;
mod nvim_bridge;
mod pi_state;
mod pixel_shift;
mod remote;
mod store;
mod terminal;
mod touch;
mod volume;
mod widgets;

use crate::app::{App, AppGeometry};
use crate::config::ConfigManager;
use crate::context::ContextListener;
use crate::event::AppEvent;
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
    let mut map = drm
        .map()
        .expect("crash handler: failed to map the DRM framebuffer");
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
    // tiny-dfr runs in the user session — no privilege drop. Device access comes
    // from group membership + udev rules (see the README): uinput, the digitizer,
    // the backlights, and the DRM card are all opened directly as the user.
    let uinput = UInputHandle::new(
        OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .expect("failed to open /dev/uinput (is the tiny-dfr udev rule installed?)"),
    );
    let backlight = BacklightManager::new();
    let kbd = KbdBacklight::new();
    // The T1 digitizer is read raw (see touch.rs) — libinput mangles its drags.
    let mut touch_reader = TouchReader::open(width, height);
    let mut cfg_mgr = ConfigManager::new();

    // App owns the bar state + dispatch.
    let mut app = App::new(
        &cfg_mgr,
        AppGeometry {
            width,
            height,
            db_width,
            db_height,
        },
        uinput,
        backlight,
        kbd,
    );

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
    // Hyprland focused-window events drive the app-aware layers (context.rs).
    let mut context = ContextListener::new();
    if let Some(fd) = context.as_fd() {
        epoll
            .add(fd, EpollEvent::new(EpollFlags::EPOLLIN, 5))
            .expect("failed to register the Hyprland context fd with epoll");
    }
    if let Some(fd) = app.remote_wake_fd() {
        epoll
            .add(fd, EpollEvent::new(EpollFlags::EPOLLIN, 6))
            .expect("failed to register the remote bridge wake fd with epoll");
    }

    loop {
        handle_app_event(&mut app, AppEvent::ConfigReload, &mut cfg_mgr, drm);
        app.resolve_and_log();

        let touch_down = touch_reader.as_ref().is_some_and(|r| r.is_down());
        let next_timeout_ms = app.next_timeout(touch_down);

        if let Err(e) = app.refresh_sources() {
            eprintln!("source refresh failed: {e:#}");
            let _ = app.render_error(drm, &e);
        }

        handle_app_event(&mut app, AppEvent::Tick, &mut cfg_mgr, drm);
        if let Err(e) = app.render(drm) {
            eprintln!("render fallback failed: {e:#}");
        }

        match epoll.wait(
            &mut [EpollEvent::new(EpollFlags::EPOLLIN, 0)],
            next_timeout_ms as u16,
        ) {
            Err(Errno::EINTR) | Ok(_) => 0,
            e => e.expect("epoll wait failed"),
        };

        if app.drain_remote_wake() {
            handle_app_event(&mut app, AppEvent::RemoteChanged, &mut cfg_mgr, drm);
        }

        _ = udev_monitor.iter().last();

        let t_in = Instant::now();
        input_main.dispatch().expect("libinput dispatch failed");
        let t_dispatch = t_in.elapsed();
        let mut n_events = 0u32;
        for event in &mut input_main.clone() {
            n_events += 1;
            handle_app_event(&mut app, AppEvent::Libinput(event), &mut cfg_mgr, drm);
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
                handle_app_event(&mut app, AppEvent::Touch(s), &mut cfg_mgr, drm);
            }
        }

        // ----- Hyprland focused-window context -> app-aware layer (context.rs).
        if let Some((class, title)) = context.poll() {
            handle_app_event(
                &mut app,
                AppEvent::FocusChanged { class, title },
                &mut cfg_mgr,
                drm,
            );
        }
        // Reconnect if Hyprland went away (e.g. compositor restart) and re-register.
        if !context.is_connected() && context.reconnect() {
            if let Some(fd) = context.as_fd() {
                let _ = epoll.add(fd, EpollEvent::new(EpollFlags::EPOLLIN, 5));
            }
        }
        app.update_backlight();
    }
}

fn handle_app_event(
    app: &mut App,
    event: AppEvent,
    cfg_mgr: &mut ConfigManager,
    drm: &mut DrmBackend,
) {
    if let Err(e) = app.handle(event, cfg_mgr) {
        eprintln!("app event failed: {e:#}");
        let _ = app.render_error(drm, &e);
    }
}
