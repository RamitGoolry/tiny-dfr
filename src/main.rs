use cairo::{Format, ImageSurface};
use chrono::{Local, Timelike};
use drm::control::ClipRect;
use ::input::{
    event::{
        keyboard::{KeyState, KeyboardEvent, KeyboardEventTrait},
        Event,
    },
    Libinput,
};
use input_linux::{uinput::UInputHandle, EventKind, Key};
use input_linux_sys::{input_id, uinput_setup};
use libc::c_char;
use nix::{
    errno::Errno,
    sys::{
        epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags},
        signal::{SigSet, Signal},
    },
};
use privdrop::PrivDrop;
use std::{
    cmp::min,
    collections::HashMap,
    fs::OpenOptions,
    os::fd::{AsFd, AsRawFd},
    panic::{self, AssertUnwindSafe},
    time::{Duration, Instant},
};
use udev::MonitorBuilder;

mod backlight;
mod battery;
mod config;
mod display;
mod fonts;
mod function_layer;
mod input;
mod layer;
mod pixel_shift;
mod touch;
mod widgets;

use crate::config::ConfigManager;
use crate::input::Interface;
use backlight::BacklightManager;
use display::DrmBackend;
use layer::{Layer, LayerStore, ResolverState, TouchTarget};
use pixel_shift::PixelShiftManager;
use widgets::SliderBackend;
use touch::{TouchPhase, TouchReader};

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
    let mut drm = DrmBackend::open_card().unwrap();
    let (height, width) = drm.mode().size();
    let _ = panic::catch_unwind(AssertUnwindSafe(|| real_main(&mut drm)));
    let crash_bitmap = include_bytes!("crash_bitmap.raw");
    let mut map = drm.map().unwrap();
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
    drm.dirty(&[ClipRect::new(0, 0, height, width)]).unwrap();
    let mut sigset = SigSet::empty();
    sigset.add(Signal::SIGTERM);
    sigset.wait().unwrap();
}

/// Finish an in-flight touch: deactivate the button, or release a slider and pop
/// its modal layer. Shared by the Up and Cancel handlers.
fn end_touch<F: AsRawFd>(
    removed: Option<TouchTarget>,
    store: &mut LayerStore,
    rstate: &mut ResolverState,
    uinput: &mut UInputHandle<F>,
    needs_complete_redraw: &mut bool,
) {
    match removed {
        Some(TouchTarget::Button { layer, btn }) => {
            if let Layer::Buttons(fl) = store.get_mut(&layer) {
                fl.buttons[btn].1.set_active(uinput, false);
            }
        }
        Some(TouchTarget::Slider { layer }) => {
            if let Layer::Slider(s) = store.get_mut(&layer) {
                s.release();
            }
            rstate.modal = None;
            *needs_complete_redraw = true;
        }
        None => {}
    }
}

fn real_main(drm: &mut DrmBackend) {
    let (height, width) = drm.mode().size();
    let (db_width, db_height) = drm.fb_info().unwrap().size();
    let mut uinput = UInputHandle::new(OpenOptions::new().write(true).open("/dev/uinput").unwrap());
    let mut backlight = BacklightManager::new();
    // The T1 digitizer is read raw (see touch.rs) — libinput mangles its drags.
    // Opened here, before the privilege drop, so the fd survives as `nobody`.
    let mut touch_reader = TouchReader::open(width, height);
    let mut cfg_mgr = ConfigManager::new();
    let (mut cfg, mut store) = cfg_mgr.load_config(width);
    let mut pixel_shift = PixelShiftManager::new();
    let mut last = Instant::now();

    // drop privileges to input and video group
    let groups = ["input", "video"];

    PrivDrop::default()
        .user("nobody")
        .group_list(&groups)
        .apply()
        .unwrap_or_else(|e| panic!("Failed to drop privileges: {}", e));

    let mut surface =
        ImageSurface::create(Format::ARgb32, db_width as i32, db_height as i32).unwrap();
    let mut rstate = ResolverState::default();
    let mut needs_complete_redraw = true;
    let mut prev_active = String::new();

    let mut input_main = Libinput::new_with_udev(Interface);
    input_main.udev_assign_seat("seat0").unwrap();
    let udev_monitor = MonitorBuilder::new()
        .unwrap()
        .match_subsystem("power_supply")
        .unwrap()
        .listen()
        .unwrap();
    let epoll = Epoll::new(EpollCreateFlags::empty()).unwrap();
    epoll
        .add(input_main.as_fd(), EpollEvent::new(EpollFlags::EPOLLIN, 0))
        .unwrap();
    epoll
        .add(cfg_mgr.fd(), EpollEvent::new(EpollFlags::EPOLLIN, 2))
        .unwrap();
    epoll
        .add(&udev_monitor, EpollEvent::new(EpollFlags::EPOLLIN, 3))
        .unwrap();
    if let Some(reader) = &touch_reader {
        epoll
            .add(reader.as_fd(), EpollEvent::new(EpollFlags::EPOLLIN, 4))
            .unwrap();
    }
    uinput.set_evbit(EventKind::Key).unwrap();
    for k in Key::iter() {
        uinput.set_keybit(k).unwrap();
    }
    let mut dev_name_c = [0 as c_char; 80];
    let dev_name = "Dynamic Function Row Virtual Input Device".as_bytes();
    for i in 0..dev_name.len() {
        dev_name_c[i] = dev_name[i] as c_char;
    }
    uinput
        .dev_setup(&uinput_setup {
            id: input_id {
                bustype: 0x19,
                vendor: 0x1209,
                product: 0x316E,
                version: 1,
            },
            ff_effects_max: 0,
            name: dev_name_c,
        })
        .unwrap();
    uinput.dev_create().unwrap();

    let mut touches = HashMap::new();
    let mut last_redraw_ts = {
        let active = store.resolve(&rstate);
        if store.get(&active).faster_refresh() {
            Local::now().second()
        } else {
            Local::now().minute()
        }
    };
    loop {
        if cfg_mgr.update_config(&mut cfg, &mut store, width) {
            rstate = ResolverState::default();
            touches.clear();
            needs_complete_redraw = true;
        }
        let active = store.resolve(&rstate);
        if active != prev_active {
            eprintln!("[dbg {:.6}] LAYER {} -> {}", dbg_ts(), prev_active, active);
            prev_active = active.clone();
        }

        let now = Local::now();
        let ms_left = ((60 - now.second()) * 1000) as i32;
        let mut next_timeout_ms = min(ms_left, TIMEOUT_MS);

        if cfg.enable_pixel_shift {
            let (pixel_shift_needs_redraw, pixel_shift_next_timeout_ms) = pixel_shift.update();
            if pixel_shift_needs_redraw {
                needs_complete_redraw = true;
            }
            next_timeout_ms = min(next_timeout_ms, pixel_shift_next_timeout_ms);
        }

        // A touch awaiting release is on a timer, not an fd event — poll fast
        // enough to catch it, else the loop sleeps up to TIMEOUT_MS and the
        // release (and the key-up it sends) lags by seconds.
        if touch_reader.as_ref().is_some_and(|r| r.is_down()) {
            next_timeout_ms = min(next_timeout_ms, TOUCH_ACTIVE_POLL_MS);
        }

        let current_ts = if store.get(&active).faster_refresh() {
            Local::now().second()
        } else {
            Local::now().minute()
        };
        if store.get(&active).displays_time() && (current_ts != last_redraw_ts) {
            needs_complete_redraw = true;
            last_redraw_ts = current_ts;
        }
        if store.get(&active).displays_battery() {
            store.get_mut(&active).mark_batteries_changed();
        }

        if needs_complete_redraw || store.get(&active).needs_redraw() {
            let shift = if cfg.enable_pixel_shift {
                pixel_shift.get()
            } else {
                (0.0, 0.0)
            };
            let t_r = Instant::now();
            let clips = store.get_mut(&active).draw(
                &cfg,
                width as i32,
                height as i32,
                &surface,
                shift,
                needs_complete_redraw,
            );
            let t_draw = t_r.elapsed();
            let data = surface.data().unwrap();
            drm.map().unwrap().as_mut()[..data.len()].copy_from_slice(&data);
            // Partial (per-button) damage, as before; the probe times the push.
            let t_d = Instant::now();
            drm.dirty(&clips).unwrap();
            let t_dirty = t_d.elapsed();
            eprintln!(
                "[dbg {:.6}] REDRAW draw={}ms dirty={}ms clips={} complete={}",
                dbg_ts(),
                t_draw.as_millis(),
                t_dirty.as_millis(),
                clips.len(),
                needs_complete_redraw
            );
            needs_complete_redraw = false;
        }

        match epoll.wait(
            &mut [EpollEvent::new(EpollFlags::EPOLLIN, 0)],
            next_timeout_ms as u16,
        ) {
            Err(Errno::EINTR) | Ok(_) => 0,
            e => e.unwrap(),
        };

        _ = udev_monitor.iter().last();

        let t_in = Instant::now();
        input_main.dispatch().unwrap();
        let t_dispatch = t_in.elapsed();
        let mut n_events = 0u32;
        for event in &mut input_main.clone() {
            n_events += 1;
            backlight.process_event(&event);
            if let Event::Keyboard(KeyboardEvent::Key(key)) = event {
                if key.key() == Key::Fn as u32 {
                    if cfg.double_press_switch_layers > 0 && key.key_state() == KeyState::Pressed {
                        if last.elapsed()
                            < Duration::from_millis(cfg.double_press_switch_layers.into())
                        {
                            store.base_order.swap(0, 1);
                        }
                        last = Instant::now();
                    }
                    let fn_pressed = key.key_state() == KeyState::Pressed;
                    if rstate.fn_pressed != fn_pressed {
                        rstate.fn_pressed = fn_pressed;
                        needs_complete_redraw = true;
                    }
                }
            }
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
                backlight.wake(); // any digitizer touch keeps the bar lit
                let (x, y) = (s.x, s.y);
                match s.phase {
                    TouchPhase::Down => {
                        if backlight.current_bl() == 0 {
                            continue; // bar is dark; the touch just woke it
                        }
                        let active = store.resolve(&rstate);
                        let hit = match store.get(&active) {
                            Layer::Buttons(fl) => fl.hit(width, height, x, y, None),
                            Layer::Slider(_) => None,
                        };
                        eprintln!("[dbg {:.6}] DOWN x={x:.0} y={y:.0} hit={hit:?}", dbg_ts());
                        if let Some(btn) = hit {
                            let trigger = match store.get(&active) {
                                Layer::Buttons(fl) => fl.buttons[btn].1.open_layer.clone(),
                                Layer::Slider(_) => None,
                            };
                            if let Some(target) = trigger {
                                rstate.modal = Some(target.clone());
                                if let Layer::Slider(s) = store.get_mut(&target) {
                                    let level = match s.backend {
                                        SliderBackend::Brightness => backlight.display_level(),
                                    };
                                    s.grab(x, level);
                                }
                                touches.insert(0, TouchTarget::Slider { layer: target });
                                needs_complete_redraw = true;
                            } else {
                                touches.insert(
                                    0,
                                    TouchTarget::Button {
                                        layer: active.clone(),
                                        btn,
                                    },
                                );
                                if let Layer::Buttons(fl) = store.get_mut(&active) {
                                    fl.buttons[btn].1.set_active(&mut uinput, true);
                                }
                            }
                        }
                    }
                    TouchPhase::Motion => match touches.get(&0) {
                        Some(TouchTarget::Button { layer, btn }) => {
                            let (layer, btn) = (layer.clone(), *btn);
                            let active = store.resolve(&rstate);
                            let hit = match store.get(&active) {
                                Layer::Buttons(fl) => {
                                    fl.hit(width, height, x, y, Some(btn)).is_some()
                                }
                                Layer::Slider(_) => false,
                            };
                            if let Layer::Buttons(fl) = store.get_mut(&layer) {
                                fl.buttons[btn].1.set_active(&mut uinput, hit);
                            }
                        }
                        Some(TouchTarget::Slider { layer }) => {
                            let layer = layer.clone();
                            if let Layer::Slider(s) = store.get_mut(&layer) {
                                let level = s.drag(x, width as f64);
                                eprintln!("[dbg {:.6}] DRAG x={x:.0} level={level:.2}", dbg_ts());
                                match s.backend {
                                    SliderBackend::Brightness => backlight.set_display_level(level),
                                }
                            }
                        }
                        None => {}
                    },
                    TouchPhase::Up => {
                        let removed = touches.remove(&0);
                        eprintln!("[dbg {:.6}] UP removed={}", dbg_ts(), removed.is_some());
                        end_touch(
                            removed,
                            &mut store,
                            &mut rstate,
                            &mut uinput,
                            &mut needs_complete_redraw,
                        );
                    }
                }
            }
        }
        backlight.update_backlight(&cfg);
    }
}
