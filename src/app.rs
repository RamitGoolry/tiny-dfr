//! The application hub: owns the bar's state, the redraw/dispatch bookkeeping, and
//! the single effectful site (`apply`). `real_main` opens the root resources, drops
//! privileges, builds an `App`, and then just feeds it I/O events from the epoll
//! loop. Everything here is a verbatim lift of the old `real_main` loop bodies, with
//! the moved locals turned into `self.` fields — behaviour is unchanged.
use cairo::{Format, ImageSurface};
use chrono::{Local, Timelike};
use ::input::{
    event::{
        keyboard::{KeyState, KeyboardEvent, KeyboardEventTrait},
        Event,
    },
};
use input_linux::{uinput::UInputHandle, EventKind, Key};
use input_linux_sys::{input_id, uinput_setup};
use libc::c_char;
use std::{
    cmp::min,
    collections::HashMap,
    fs::File,
    time::{Duration, Instant},
};

use crate::action::{Action, Edge};
use crate::backlight::BacklightManager;
use crate::kbd_backlight::KbdBacklight;
use crate::volume::VolumeMixer;
use crate::config::{Config, ConfigManager};
use crate::display::DrmBackend;
use crate::input::toggle_keys;
use crate::layer::{LayerStore, ResolverState, TouchTarget};
use crate::pixel_shift::PixelShiftManager;
use crate::state::State;
use crate::touch::{TouchPhase, TouchSample};
use crate::{dbg_ts, TIMEOUT_MS, TOUCH_ACTIVE_POLL_MS};

/// Owns the application state, the render target, and the event dispatch. The I/O
/// that drives it (drm, epoll, libinput, udev, the digitizer reader, the config
/// manager) stays in `real_main`.
pub(crate) struct App {
    store: LayerStore,
    rstate: ResolverState,
    touches: HashMap<i32, TouchTarget>,
    backlight: BacklightManager,
    /// Keyboard backlight LED, driven by the keyboard-illumination slider.
    kbd: KbdBacklight,
    /// ALSA Master mixer, driven by the volume slider.
    volume: VolumeMixer,
    uinput: UInputHandle<File>,
    cfg: Config,
    pixel_shift: PixelShiftManager,
    /// The render target the active layer draws into; copied into the drm fb.
    surface: ImageSurface,
    needs_complete_redraw: bool,
    /// Last resolved layer name, for the `LAYER {} -> {}` transition log.
    prev_active: String,
    /// The time-display redraw throttle: the last second/minute we redrew at.
    last_redraw_ts: u32,
    /// Timestamp of the last Fn press, for the double-press layer-swap timing.
    last: Instant,
}

impl App {
    /// Build the App after the privilege drop. `uinput` and `backlight` are opened
    /// as root in `real_main` (so their fds survive as `nobody`) and handed in here;
    /// the uinput *device setup* below intentionally runs post-PrivDrop.
    pub(crate) fn new(
        cfg_mgr: &ConfigManager,
        width: u16,
        db_width: u32,
        db_height: u32,
        uinput: UInputHandle<File>,
        backlight: BacklightManager,
        kbd: KbdBacklight,
    ) -> App {
        // Fatal at startup: an unloadable config means the daemon cannot run. This
        // runs under `real_main`'s catch_unwind, so the panic paints the crash bar.
        let (cfg, store) = cfg_mgr
            .load_config(width)
            .unwrap_or_else(|e| panic!("failed to load configuration: {e:#}"));
        let pixel_shift = PixelShiftManager::new();
        let last = Instant::now();
        // The mixer opens /dev/snd/controlC0, reachable now that the daemon has
        // dropped into the `audio` group (see real_main's PrivDrop group list).
        let volume = VolumeMixer::new();

        // uinput virtual-device setup — must stay AFTER the privilege drop.
        uinput
            .set_evbit(EventKind::Key)
            .expect("failed to enable key events on the uinput device");
        for k in Key::iter() {
            uinput
                .set_keybit(k)
                .expect("failed to register a key on the uinput device");
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
            .expect("failed to configure the uinput device");
        uinput
            .dev_create()
            .expect("failed to create the uinput device");

        let surface = ImageSurface::create(Format::ARgb32, db_width as i32, db_height as i32)
            .expect("failed to create the render surface");
        let rstate = ResolverState::default();
        let touches: HashMap<i32, TouchTarget> = HashMap::new();
        let last_redraw_ts = {
            let active = store.resolve(&rstate);
            if store.get(&active).faster_refresh() {
                Local::now().second()
            } else {
                Local::now().minute()
            }
        };
        App {
            store,
            rstate,
            touches,
            backlight,
            kbd,
            volume,
            uinput,
            cfg,
            pixel_shift,
            surface,
            needs_complete_redraw: true,
            prev_active: String::new(),
            last_redraw_ts,
            last,
        }
    }

    /// The world the widgets render from, snapshotted each iteration.
    pub(crate) fn state(&self) -> State {
        State {
            brightness: self.backlight.display_level(),
            kbd_illum: self.kbd.level(),
            volume: self.volume.level(),
        }
    }

    /// Pick up an inotify config reload: reset the resolver, drop in-flight touches,
    /// and force a full redraw. Returns whether a reload happened.
    pub(crate) fn reload_config(&mut self, cfg_mgr: &mut ConfigManager, width: u16) -> bool {
        if cfg_mgr.update_config(&mut self.cfg, &mut self.store, width) {
            self.rstate = ResolverState::default();
            self.touches.clear();
            self.needs_complete_redraw = true;
            true
        } else {
            false
        }
    }

    /// Resolve the active layer and log the transition when it changes.
    pub(crate) fn resolve_and_log(&mut self) {
        let active = self.store.resolve(&self.rstate);
        if active != self.prev_active {
            eprintln!("[dbg {:.6}] LAYER {} -> {}", dbg_ts(), self.prev_active, active);
            self.prev_active = active;
        }
    }

    /// Compute the epoll wait: the time-to-next-minute, the pixel-shift cadence (whose
    /// `update` may also flag a redraw), and the fast poll while a touch is in flight.
    pub(crate) fn next_timeout(&mut self, touch_down: bool) -> i32 {
        let now = Local::now();
        let ms_left = ((60 - now.second()) * 1000) as i32;
        let mut next_timeout_ms = min(ms_left, TIMEOUT_MS);

        if self.cfg.enable_pixel_shift {
            let (pixel_shift_needs_redraw, pixel_shift_next_timeout_ms) = self.pixel_shift.update();
            if pixel_shift_needs_redraw {
                self.needs_complete_redraw = true;
            }
            next_timeout_ms = min(next_timeout_ms, pixel_shift_next_timeout_ms);
        }

        // A touch awaiting release is on a timer, not an fd event — poll fast
        // enough to catch it, else the loop sleeps up to TIMEOUT_MS and the
        // release (and the key-up it sends) lags by seconds.
        if touch_down {
            next_timeout_ms = min(next_timeout_ms, TOUCH_ACTIVE_POLL_MS);
        }
        next_timeout_ms
    }

    /// Per-iteration time trigger: when the active layer shows the clock, force a full
    /// redraw on each second/minute boundary it cares about.
    pub(crate) fn tick(&mut self) {
        let active = self.store.resolve(&self.rstate);
        let current_ts = if self.store.get(&active).faster_refresh() {
            Local::now().second()
        } else {
            Local::now().minute()
        };
        if self.store.get(&active).displays_time() && (current_ts != self.last_redraw_ts) {
            self.needs_complete_redraw = true;
            self.last_redraw_ts = current_ts;
        }
    }

    /// Draw the active layer into `surface`, copy it into the drm fb, and push the
    /// damage. `state` is the per-iteration snapshot (see `state`).
    pub(crate) fn render(&mut self, drm: &mut DrmBackend, state: &State) {
        let (height, width) = drm.mode().size();
        let active = self.store.resolve(&self.rstate);
        if self.needs_complete_redraw || self.store.get(&active).needs_redraw(state) {
            let shift = if self.cfg.enable_pixel_shift {
                self.pixel_shift.get()
            } else {
                (0.0, 0.0)
            };
            let t_r = Instant::now();
            let clips = self.store.get_mut(&active).draw(
                &self.cfg,
                width as i32,
                height as i32,
                &self.surface,
                shift,
                state,
                self.needs_complete_redraw,
            );
            let t_draw = t_r.elapsed();
            let data = self
                .surface
                .data()
                .expect("failed to access the render surface pixels");
            drm.map()
                .expect("failed to map the DRM framebuffer for the frame copy")
                .as_mut()[..data.len()]
                .copy_from_slice(&data);
            // Partial (per-button) damage, as before; the probe times the push.
            let t_d = Instant::now();
            drm.dirty(&clips)
                .expect("failed to flush the DRM framebuffer damage");
            let t_dirty = t_d.elapsed();
            eprintln!(
                "[dbg {:.6}] REDRAW draw={}ms dirty={}ms clips={} complete={}",
                dbg_ts(),
                t_draw.as_millis(),
                t_dirty.as_millis(),
                clips.len(),
                self.needs_complete_redraw
            );
            self.needs_complete_redraw = false;
        }
    }

    /// Handle one libinput event: feed the backlight idle timer, and on the Fn key do
    /// the double-press layer swap and the Fn-held layer toggle.
    pub(crate) fn on_libinput(&mut self, event: Event) {
        self.backlight.process_event(&event);
        if let Event::Keyboard(KeyboardEvent::Key(key)) = event {
            if key.key() == Key::Fn as u32 {
                if self.cfg.double_press_switch_layers > 0 && key.key_state() == KeyState::Pressed {
                    if self.last.elapsed()
                        < Duration::from_millis(self.cfg.double_press_switch_layers.into())
                    {
                        self.store.base_order.swap(0, 1);
                    }
                    self.last = Instant::now();
                }
                let fn_pressed = key.key_state() == KeyState::Pressed;
                if self.rstate.fn_pressed != fn_pressed {
                    self.rstate.fn_pressed = fn_pressed;
                    self.needs_complete_redraw = true;
                }
            }
        }
    }

    /// Dispatch one raw digitizer sample (Down/Motion/Up). `state` is the per-iteration
    /// snapshot, shared across every sample handled this iteration.
    pub(crate) fn on_touch(&mut self, s: TouchSample, width: u16, height: u16, state: &State) {
        self.backlight.wake(); // any digitizer touch keeps the bar lit
        let (x, y) = (s.x, s.y);
        match s.phase {
            TouchPhase::Down => {
                if self.backlight.current_bl() == 0 {
                    return; // bar is dark; the touch just woke it
                }
                let active = self.store.resolve(&self.rstate);
                let hit = self.store.get(&active).hit(width, height, x, y, None);
                eprintln!("[dbg {:.6}] DOWN x={x:.0} y={y:.0} hit={hit:?}", dbg_ts());
                if let Some(btn) = hit {
                    let action = self.store.get_mut(&active).on_press(btn, state);
                    // A modal hand-off (OpenModal) records its own Slider
                    // target inside `apply`; everything else is a button.
                    if !matches!(action, Some(Action::OpenModal(_))) {
                        self.touches.insert(
                            0,
                            TouchTarget::Button {
                                layer: active.clone(),
                                btn,
                            },
                        );
                    }
                    self.apply(action, x, state);
                }
            }
            TouchPhase::Motion => match self.touches.get(&0) {
                Some(TouchTarget::Button { layer, btn }) => {
                    let (layer, btn) = (layer.clone(), *btn);
                    let active = self.store.resolve(&self.rstate);
                    // Follow the finger: re-pressing re-lights + re-emits,
                    // leaving releases — both no-ops if already in that
                    // state, so we just call the matching edge each move.
                    let hit = self
                        .store
                        .get(&active)
                        .hit(width, height, x, y, Some(btn))
                        .is_some();
                    let action = if hit {
                        self.store.get_mut(&layer).on_press(btn, state)
                    } else {
                        self.store.get_mut(&layer).on_release(btn, state)
                    };
                    self.apply(action, x, state);
                }
                Some(TouchTarget::Slider { layer }) => {
                    let layer = layer.clone();
                    let action = self.store.get_mut(&layer).drag_slider(x, width as f64);
                    self.apply(action, x, state);
                }
                None => {}
            },
            TouchPhase::Up => {
                let removed = self.touches.remove(&0);
                eprintln!("[dbg {:.6}] UP removed={}", dbg_ts(), removed.is_some());
                let action = match removed {
                    Some(TouchTarget::Button { layer, btn }) => {
                        self.store.get_mut(&layer).on_release(btn, state)
                    }
                    Some(TouchTarget::Slider { .. }) => Some(Action::CloseModal),
                    None => None,
                };
                self.apply(action, x, state);
            }
        }
    }

    /// Drive the backlight idle/dim state machine — called at the end of each loop
    /// iteration, after touch handling has had its chance to `wake` it.
    pub(crate) fn update_backlight(&mut self) {
        self.backlight.update_backlight(&self.cfg);
    }

    /// The single effectful site: turn an `Action` returned by a widget into a real
    /// effect (emit a key, set brightness, enter/leave a modal). `x` is the touch's
    /// long-axis position, used to anchor a slider grab on `OpenModal`.
    pub(crate) fn apply(&mut self, action: Option<Action>, x: f64, state: &State) {
        let Some(action) = action else { return };
        match action {
            Action::Key(keys, edge) => {
                toggle_keys(&mut self.uinput, &keys, (edge == Edge::Press) as i32);
            }
            Action::SetBrightness(level) => {
                self.backlight.set_display_level(level);
            }
            Action::SetKbdIllum(level) => {
                self.kbd.set_level(level);
            }
            Action::SetVolume(level) => {
                self.volume.set_level(level);
            }
            Action::OpenModal(target) => {
                self.rstate.modal = Some(target.clone());
                self.store.get_mut(&target).grab_slider(state, x);
                self.touches.insert(0, TouchTarget::Slider { layer: target });
                self.needs_complete_redraw = true;
            }
            Action::CloseModal => {
                if let Some(layer) = self.rstate.modal.take() {
                    self.store.get_mut(&layer).release_slider();
                }
                self.needs_complete_redraw = true;
            }
        }
    }
}
