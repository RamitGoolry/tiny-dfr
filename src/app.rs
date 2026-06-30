//! The application hub: owns the reactive Store, layer resolution, redraw/dispatch
//! bookkeeping, and the single effectful site (`apply`). `real_main` owns epoll and
//! source-specific polling, then feeds normalized [`AppEvent`]s into `App`.
use ::input::event::{
    keyboard::{KeyState, KeyboardEvent, KeyboardEventTrait},
    Event,
};
use anyhow::{anyhow, Result};
use cairo::{Context, Format, ImageSurface};
use chrono::{Local, Timelike};
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
use crate::chromium::{ChromiumClient, ChromiumTabsState};
use crate::config::{Config, ConfigManager};
use crate::display::DrmBackend;
use crate::event::AppEvent;
use crate::function_layer::LayerArea;
use crate::input::toggle_keys;
use crate::kbd_backlight::KbdBacklight;
use crate::layer::{LayerStore, ResolverState, TouchTarget};
use crate::mpris::{MediaState, MprisClient};
use crate::pixel_shift::PixelShiftManager;
use crate::store::{key, StateKey, Store, Value};
use crate::touch::{TouchPhase, TouchSample};
use crate::volume::VolumeMixer;
use crate::{dbg_ts, BUTTON_SPACING_PX, TIMEOUT_MS, TOUCH_ACTIVE_POLL_MS};

const GLOBAL_LEFT_LAYER: &str = "global-left";
const GLOBAL_RIGHT_LAYER: &str = "global-right";
const GLOBAL_RIGHT_MEDIA_LAYER: &str = "global-right-media";
const GLOBAL_RIGHT_TABS_LAYER: &str = "global-right-tabs";
const MEDIA_ACTIVE_LAYER: &str = "media-active";
const MEDIA_OVERLAY_LAYER: &str = "media-overlay";
const CHROMIUM_TABS_LAYER: &str = "chromium-tabs";

pub(crate) struct AppGeometry {
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) db_width: u32,
    pub(crate) db_height: u32,
}

/// Owns the application state, the render target, and the event dispatch. The I/O
/// that drives it (drm, epoll, libinput, udev, the digitizer reader, the config
/// manager) stays in `real_main`.
pub(crate) struct App {
    store: LayerStore,
    runtime: Store,
    rstate: ResolverState,
    touches: HashMap<i32, TouchTarget>,
    backlight: BacklightManager,
    /// Keyboard backlight LED, driven by the keyboard-illumination slider.
    kbd: KbdBacklight,
    /// PipeWire default sink volume, driven by the volume slider.
    volume: VolumeMixer,
    mpris: MprisClient,
    chromium: ChromiumClient,
    uinput: UInputHandle<File>,
    cfg: Config,
    width: u16,
    height: u16,
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

fn media_is_chromium(player: &str) -> bool {
    let player = player.to_ascii_lowercase();
    player.contains("chromium") || player.contains("chrome")
}

fn titles_probably_match(media_title: &str, focused_title: &str) -> bool {
    let media = normalize_media_title(media_title);
    let focused = normalize_media_title(focused_title);
    !media.is_empty()
        && !focused.is_empty()
        && (focused.contains(&media) || media.contains(&focused))
}

fn normalize_media_title(title: &str) -> String {
    let mut title = title.trim();
    // Chromium titles often start with notification counts like "(695) YouTube".
    if let Some(rest) = title.strip_prefix('(') {
        if let Some((digits, after)) = rest.split_once(')') {
            if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                title = after.trim_start();
            }
        }
    }
    title
        .trim_end_matches(" - YouTube")
        .trim_end_matches(" – YouTube")
        .trim_end_matches(" — YouTube")
        .to_ascii_lowercase()
}

impl App {
    /// Build the App. `uinput`, `backlight`, and `kbd` are opened in `real_main`
    /// and handed in; the daemon runs in the user session, so no privilege drop is
    /// involved (device access comes from group/udev rules — see the README).
    pub(crate) fn new(
        cfg_mgr: &ConfigManager,
        geometry: AppGeometry,
        uinput: UInputHandle<File>,
        backlight: BacklightManager,
        kbd: KbdBacklight,
    ) -> App {
        // Fatal at startup: an unloadable config means the daemon cannot run. This
        // runs under `real_main`'s catch_unwind, so the panic paints the crash bar.
        let (cfg, store) = cfg_mgr
            .load_config(geometry.width)
            .unwrap_or_else(|e| panic!("failed to load configuration: {e:#}"));
        let pixel_shift = PixelShiftManager::new();
        let last = Instant::now();
        // In-process PipeWire volume via wpctl (see volume.rs); spawns its apply thread.
        let volume = VolumeMixer::new();
        let mpris = MprisClient::new();
        let chromium = ChromiumClient::new(None);
        let mut runtime = Store::new();
        runtime
            .set(
                key::HARDWARE_BRIGHTNESS,
                Value::Number(backlight.display_level()),
            )
            .expect("built-in Store key must be valid");
        runtime
            .set(key::HARDWARE_KBD_ILLUM, Value::Number(kbd.level()))
            .expect("built-in Store key must be valid");
        runtime
            .set(key::CONTEXT_FOCUS_CLASS, Value::Text(String::new()))
            .expect("built-in Store key must be valid");
        runtime
            .set(key::CONTEXT_FOCUS_TITLE, Value::Text(String::new()))
            .expect("built-in Store key must be valid");
        runtime
            .set(key::MEDIA_ACTIVE_PLAYER, Value::Text(String::new()))
            .expect("built-in Store key must be valid");
        runtime
            .set(key::MEDIA_ACTIVE_TRACK_ID, Value::Text(String::new()))
            .expect("built-in Store key must be valid");
        runtime
            .set(key::MEDIA_ACTIVE_STATUS, Value::Text(String::new()))
            .expect("built-in Store key must be valid");
        runtime
            .set(key::MEDIA_ACTIVE_TITLE, Value::Text(String::new()))
            .expect("built-in Store key must be valid");
        runtime
            .set(key::MEDIA_ACTIVE_ART_URL, Value::Text(String::new()))
            .expect("built-in Store key must be valid");
        runtime
            .set(key::MEDIA_ACTIVE_LENGTH, Value::Number(0.0))
            .expect("built-in Store key must be valid");
        runtime
            .set(key::MEDIA_ACTIVE_POSITION, Value::Number(0.0))
            .expect("built-in Store key must be valid");
        runtime
            .set(key::CHROMIUM_MEDIA_ACTIVE, Value::Bool(false))
            .expect("built-in Store key must be valid");
        runtime
            .set(key::CHROMIUM_TABS_AVAILABLE, Value::Bool(false))
            .expect("built-in Store key must be valid");
        runtime
            .set(key::CHROMIUM_TABS_JSON, Value::Text("[]".to_string()))
            .expect("built-in Store key must be valid");
        runtime
            .set(key::CHROMIUM_TABS_COUNT, Value::Number(0.0))
            .expect("built-in Store key must be valid");
        runtime
            .set(key::CHROMIUM_TABS_ACTIVE_INDEX, Value::Number(-1.0))
            .expect("built-in Store key must be valid");
        runtime.clear_dirty();

        // uinput virtual-device setup.
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

        let surface = ImageSurface::create(
            Format::ARgb32,
            geometry.db_width as i32,
            geometry.db_height as i32,
        )
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
            runtime,
            rstate,
            touches,
            backlight,
            kbd,
            volume,
            mpris,
            chromium,
            uinput,
            cfg,
            width: geometry.width,
            height: geometry.height,
            pixel_shift,
            surface,
            needs_complete_redraw: true,
            prev_active: String::new(),
            last_redraw_ts,
            last,
        }
    }

    /// Refresh cheap hardware sources into the Store before render.
    pub(crate) fn refresh_sources(&mut self) -> Result<()> {
        self.runtime.set(
            key::HARDWARE_BRIGHTNESS,
            Value::Number(self.backlight.display_level()),
        )?;
        self.runtime
            .set(key::HARDWARE_KBD_ILLUM, Value::Number(self.kbd.level()))?;
        let old_center = self.center_layer().map(str::to_string);
        let old_right = self.global_right_layer();
        self.refresh_media()?;
        self.refresh_chromium_tabs(false)?;
        if old_center.as_deref() != self.center_layer() || old_right != self.global_right_layer() {
            self.needs_complete_redraw = true;
        }
        Ok(())
    }

    fn refresh_media(&mut self) -> Result<()> {
        if let Some(media) = self.mpris.refresh()? {
            self.store_media(media)?;
        } else {
            self.runtime
                .set(key::MEDIA_ACTIVE_PLAYER, Value::Text(String::new()))?;
            self.runtime
                .set(key::MEDIA_ACTIVE_TRACK_ID, Value::Text(String::new()))?;
            self.runtime
                .set(key::MEDIA_ACTIVE_STATUS, Value::Text(String::new()))?;
            self.runtime
                .set(key::MEDIA_ACTIVE_TITLE, Value::Text(String::new()))?;
            self.runtime
                .set(key::MEDIA_ACTIVE_ART_URL, Value::Text(String::new()))?;
            self.runtime
                .set(key::MEDIA_ACTIVE_LENGTH, Value::Number(0.0))?;
            self.runtime
                .set(key::MEDIA_ACTIVE_POSITION, Value::Number(0.0))?;
            self.runtime
                .set(key::CHROMIUM_MEDIA_ACTIVE, Value::Bool(false))?;
        }
        Ok(())
    }

    fn refresh_chromium_tabs(&mut self, force: bool) -> Result<()> {
        let focused_title = self
            .runtime
            .text(key::CONTEXT_FOCUS_TITLE)
            .unwrap_or("")
            .to_string();
        let state = match self.chromium.refresh_tabs(&focused_title, force) {
            Ok(state) => state,
            Err(err) => {
                eprintln!("chromium: {err:#}");
                self.chromium.unavailable_state()
            }
        };
        self.store_chromium_tabs(state)
    }

    fn store_chromium_tabs(&mut self, state: ChromiumTabsState) -> Result<()> {
        self.runtime
            .set(key::CHROMIUM_TABS_AVAILABLE, Value::Bool(state.available))?;
        self.runtime
            .set(key::CHROMIUM_TABS_JSON, Value::Text(state.tabs_json))?;
        self.runtime
            .set(key::CHROMIUM_TABS_COUNT, Value::Number(state.count as f64))?;
        self.runtime.set(
            key::CHROMIUM_TABS_ACTIVE_INDEX,
            Value::Number(state.active_index.map(|idx| idx as f64).unwrap_or(-1.0)),
        )?;
        Ok(())
    }

    fn store_media(&mut self, media: MediaState) -> Result<()> {
        let chromium_media_active = media_is_chromium(&media.player);
        self.runtime
            .set(key::MEDIA_ACTIVE_PLAYER, Value::Text(media.player))?;
        self.runtime
            .set(key::MEDIA_ACTIVE_TRACK_ID, Value::Text(media.track_id))?;
        self.runtime
            .set(key::MEDIA_ACTIVE_STATUS, Value::Text(media.status))?;
        self.runtime
            .set(key::MEDIA_ACTIVE_TITLE, Value::Text(media.title))?;
        self.runtime
            .set(key::MEDIA_ACTIVE_ART_URL, Value::Text(media.art_url))?;
        self.runtime
            .set(key::MEDIA_ACTIVE_LENGTH, Value::Number(media.length_us))?;
        self.runtime
            .set(key::MEDIA_ACTIVE_POSITION, Value::Number(media.position_us))?;
        self.runtime.set(
            key::CHROMIUM_MEDIA_ACTIVE,
            Value::Bool(chromium_media_active),
        )?;
        Ok(())
    }

    fn refresh_key(&mut self, key: &StateKey) -> Result<()> {
        match key.as_str() {
            key::HARDWARE_BRIGHTNESS => self.runtime.set(
                key::HARDWARE_BRIGHTNESS,
                Value::Number(self.backlight.display_level()),
            )?,
            key::HARDWARE_KBD_ILLUM => self
                .runtime
                .set(key::HARDWARE_KBD_ILLUM, Value::Number(self.kbd.level()))?,
            key::AUDIO_VOLUME => self.runtime.set(
                key::AUDIO_VOLUME,
                Value::Number(crate::volume::current_level()),
            )?,
            other => {
                return Err(anyhow!(
                    "no source refresh registered for Store key `{other}`"
                ))
            }
        }
        Ok(())
    }

    /// A Hyprland focus change: map the focused window class to a layer (via
    /// `app_layers` config) and let it take precedence over the base layers. An
    /// unmapped class clears `app`, falling back to the base layer.
    pub(crate) fn on_focus(&mut self, class: &str, title: &str) {
        let old_center = self.center_layer().map(str::to_string);
        let old_right = self.global_right_layer();
        self.runtime
            .set(key::CONTEXT_FOCUS_CLASS, Value::Text(class.to_string()))
            .expect("built-in Store key must be valid");
        self.runtime
            .set(key::CONTEXT_FOCUS_TITLE, Value::Text(title.to_string()))
            .expect("built-in Store key must be valid");
        let new_app = self.cfg.app_layers.get(class).cloned();
        if new_app != self.rstate.app {
            eprintln!(
                "[dbg {:.6}] FOCUS class={class:?} -> app_layer={new_app:?}",
                dbg_ts()
            );
            self.rstate.app = new_app;
        }
        if old_center.as_deref() != self.center_layer() || old_right != self.global_right_layer() {
            self.needs_complete_redraw = true;
        }
    }

    pub(crate) fn handle(&mut self, event: AppEvent, cfg_mgr: &mut ConfigManager) -> Result<()> {
        match event {
            AppEvent::Libinput(event) => self.on_libinput(event),
            AppEvent::Touch(sample) => self.on_touch(sample)?,
            AppEvent::FocusChanged { class, title } => self.on_focus(&class, &title),
            AppEvent::ConfigReload => {
                self.reload_config(cfg_mgr, self.width)?;
            }
            AppEvent::Tick => self.tick(),
        }
        Ok(())
    }

    /// Pick up an inotify config reload: reset the resolver, drop in-flight touches,
    /// and force a full redraw. Returns whether a reload happened.
    pub(crate) fn reload_config(
        &mut self,
        cfg_mgr: &mut ConfigManager,
        width: u16,
    ) -> Result<bool> {
        if cfg_mgr.update_config(&mut self.cfg, &mut self.store, width) {
            self.rstate = ResolverState::default();
            let class = self.runtime.text(key::CONTEXT_FOCUS_CLASS)?.to_string();
            self.rstate.app = self.cfg.app_layers.get(&class).cloned();
            self.touches.clear();
            self.needs_complete_redraw = true;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Resolve the active layer and log the transition when it changes.
    pub(crate) fn resolve_and_log(&mut self) {
        let active = self.store.resolve(&self.rstate);
        if active != self.prev_active {
            eprintln!(
                "[dbg {:.6}] LAYER {} -> {}",
                dbg_ts(),
                self.prev_active,
                active
            );
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

        if self.frame_faster_refresh() {
            next_timeout_ms = min(next_timeout_ms, 1000);
        }

        // A touch awaiting release is on a timer, not an fd event — poll fast
        // enough to catch it, else the loop sleeps up to TIMEOUT_MS and the
        // release (and the key-up it sends) lags by seconds.
        if touch_down {
            next_timeout_ms = min(next_timeout_ms, TOUCH_ACTIVE_POLL_MS);
        }
        next_timeout_ms
    }

    fn frame_faster_refresh(&self) -> bool {
        self.rstate
            .modal
            .as_deref()
            .is_some_and(|layer| self.store.get(layer).faster_refresh())
            || self.store.get(GLOBAL_LEFT_LAYER).faster_refresh()
            || self
                .center_layer()
                .is_some_and(|layer| self.store.get(layer).faster_refresh())
            || self.store.get(self.global_right_layer()).faster_refresh()
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
    /// damage.
    pub(crate) fn render(&mut self, drm: &mut DrmBackend) -> Result<()> {
        if let Err(err) = self.render_active(drm) {
            eprintln!("render failed: {err:#}");
            self.render_error(drm, &err)?;
        }
        self.runtime.clear_dirty();
        Ok(())
    }

    fn render_active(&mut self, drm: &mut DrmBackend) -> Result<()> {
        let (height, width) = drm.mode().size();
        let (height_i, width_i) = (height as i32, width as i32);
        let modal = self.rstate.modal.clone();
        let needs_redraw = if let Some(layer) = &modal {
            self.needs_complete_redraw || self.store.get(layer).needs_redraw(&self.runtime)
        } else {
            self.frame_needs_redraw(width_i, height_i)
        };
        if !needs_redraw {
            return Ok(());
        }

        let shift = if self.cfg.enable_pixel_shift {
            self.pixel_shift.get()
        } else {
            (0.0, 0.0)
        };
        let t_r = Instant::now();
        let clips = if let Some(layer) = modal {
            self.store.get_mut(&layer).draw(
                &self.cfg,
                width_i,
                height_i,
                &self.surface,
                shift,
                &self.runtime,
                self.needs_complete_redraw,
            )?
        } else {
            self.draw_frame(width_i, height_i, shift)?
        };
        let t_draw = t_r.elapsed();
        let data = self.surface.data()?;
        drm.map()?.as_mut()[..data.len()].copy_from_slice(&data);
        // Partial (per-button) damage, as before; the probe times the push.
        let t_d = Instant::now();
        drm.dirty(&clips)?;
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
        Ok(())
    }

    fn bar_areas(&self, width: i32, height: i32) -> (LayerArea, LayerArea, LayerArea) {
        // Compact always-global edge controls. The app-control center gets the
        // remaining width instead of making Esc/Volume/Brightness full row cells.
        let global_button_width = ((height as f64) * 2.5).round() as i32;
        let global_button_width = global_button_width.clamp(120, 190);
        let right_count = if self.global_right_layer() == GLOBAL_RIGHT_LAYER {
            2
        } else {
            3
        };
        let left_width = global_button_width;
        let right_width = global_button_width * right_count + BUTTON_SPACING_PX * (right_count - 1);
        let center_left = left_width as f64 + BUTTON_SPACING_PX as f64;
        let center_width = (width - left_width - right_width - BUTTON_SPACING_PX * 2).max(1);
        let right_left = (width - right_width) as f64;
        (
            LayerArea {
                left: 0.0,
                width: left_width,
                height,
            },
            LayerArea {
                left: center_left,
                width: center_width,
                height,
            },
            LayerArea {
                left: right_left,
                width: right_width,
                height,
            },
        )
    }

    fn center_layer(&self) -> Option<&str> {
        self.rstate
            .substates
            .last()
            .map(String::as_str)
            .filter(|layer| self.store.registry.contains_key(*layer))
            .or_else(|| {
                if self.is_chromium_focused() {
                    if self.chromium_media_on_focused_tab() {
                        self.store
                            .registry
                            .contains_key(MEDIA_ACTIVE_LAYER)
                            .then_some(MEDIA_ACTIVE_LAYER)
                    } else {
                        self.store
                            .registry
                            .contains_key(CHROMIUM_TABS_LAYER)
                            .then_some(CHROMIUM_TABS_LAYER)
                    }
                } else {
                    self.rstate
                        .app
                        .as_deref()
                        .filter(|layer| self.store.registry.contains_key(*layer))
                }
            })
    }

    fn is_chromium_focused(&self) -> bool {
        let class = self
            .runtime
            .text(key::CONTEXT_FOCUS_CLASS)
            .unwrap_or("")
            .to_ascii_lowercase();
        class.contains("chromium") || class.contains("chrome")
    }

    fn chromium_media_active(&self) -> bool {
        self.runtime
            .bool(key::CHROMIUM_MEDIA_ACTIVE)
            .unwrap_or(false)
    }

    fn chromium_media_on_focused_tab(&self) -> bool {
        if !self.is_chromium_focused() || !self.chromium_media_active() {
            return false;
        }
        let media_title = self.runtime.text(key::MEDIA_ACTIVE_TITLE).unwrap_or("");
        let focused_title = self.runtime.text(key::CONTEXT_FOCUS_TITLE).unwrap_or("");
        titles_probably_match(media_title, focused_title)
    }

    fn media_available(&self) -> bool {
        !self
            .runtime
            .text(key::MEDIA_ACTIVE_PLAYER)
            .unwrap_or("")
            .is_empty()
    }

    fn should_show_global_media_button(&self) -> bool {
        self.media_available()
            && !self.chromium_media_on_focused_tab()
            && !matches!(
                self.center_layer(),
                Some(MEDIA_ACTIVE_LAYER) | Some(MEDIA_OVERLAY_LAYER)
            )
    }

    fn global_right_layer(&self) -> &'static str {
        if self.is_chromium_focused() && self.chromium_media_on_focused_tab() {
            GLOBAL_RIGHT_TABS_LAYER
        } else if self.should_show_global_media_button() {
            GLOBAL_RIGHT_MEDIA_LAYER
        } else {
            GLOBAL_RIGHT_LAYER
        }
    }

    fn frame_needs_redraw(&self, width: i32, height: i32) -> bool {
        if self.needs_complete_redraw {
            return true;
        }
        let (_left, _center, _right) = self.bar_areas(width, height);
        self.store
            .get(GLOBAL_LEFT_LAYER)
            .needs_redraw(&self.runtime)
            || self
                .center_layer()
                .is_some_and(|layer| self.store.get(layer).needs_redraw(&self.runtime))
            || self
                .runtime
                .is_dirty(key::MEDIA_ACTIVE_PLAYER)
                .unwrap_or(false)
            || self
                .runtime
                .is_dirty(key::MEDIA_ACTIVE_TITLE)
                .unwrap_or(false)
            || self
                .runtime
                .is_dirty(key::CONTEXT_FOCUS_TITLE)
                .unwrap_or(false)
            || self
                .runtime
                .is_dirty(key::CHROMIUM_MEDIA_ACTIVE)
                .unwrap_or(false)
            || self
                .store
                .get(self.global_right_layer())
                .needs_redraw(&self.runtime)
    }

    fn draw_frame(
        &mut self,
        width: i32,
        height: i32,
        shift: (f64, f64),
    ) -> Result<Vec<drm::control::ClipRect>> {
        let (left, center, right) = self.bar_areas(width, height);
        let mut clips = if self.needs_complete_redraw {
            let c = Context::new(&self.surface)?;
            c.set_source_rgb(0.0, 0.0, 0.0);
            c.paint()?;
            vec![drm::control::ClipRect::new(
                0,
                0,
                height as u16,
                width as u16,
            )]
        } else {
            Vec::new()
        };

        clips.extend(self.store.get_mut(GLOBAL_LEFT_LAYER).draw_in(
            &self.cfg,
            left,
            &self.surface,
            shift,
            &self.runtime,
            self.needs_complete_redraw,
        )?);
        if let Some(center_layer) = self.center_layer().map(str::to_string) {
            clips.extend(self.store.get_mut(&center_layer).draw_in(
                &self.cfg,
                center,
                &self.surface,
                shift,
                &self.runtime,
                self.needs_complete_redraw,
            )?);
        }
        let right_layer = self.global_right_layer();
        clips.extend(self.store.get_mut(right_layer).draw_in(
            &self.cfg,
            right,
            &self.surface,
            shift,
            &self.runtime,
            self.needs_complete_redraw,
        )?);
        Ok(clips)
    }

    pub(crate) fn render_error(&mut self, drm: &mut DrmBackend, err: &anyhow::Error) -> Result<()> {
        let (height, width) = drm.mode().size();
        let c = Context::new(&self.surface)?;
        c.set_source_rgb(0.0, 0.0, 0.0);
        c.paint()?;
        c.translate(height as f64, 0.0);
        c.rotate((90.0f64).to_radians());
        c.set_font_face(&self.cfg.font_face);
        c.set_source_rgb(0.95, 0.1, 0.1);
        c.set_font_size(30.0);
        c.move_to(24.0, height as f64 * 0.42);
        c.show_text("tiny-dfr error")?;
        c.set_source_rgb(1.0, 1.0, 1.0);
        c.set_font_size(22.0);
        let mut line = format!("{err:#}").replace('\n', " | ");
        line.truncate(160);
        c.move_to(24.0, height as f64 * 0.68);
        c.show_text(&line)?;

        let data = self.surface.data()?;
        drm.map()?.as_mut()[..data.len()].copy_from_slice(&data);
        drm.dirty(&[drm::control::ClipRect::new(0, 0, height, width)])?;
        self.needs_complete_redraw = true;
        Ok(())
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

    fn area_for_layer(&self, layer: &str) -> Option<LayerArea> {
        if self.rstate.modal.as_deref() == Some(layer) {
            return Some(LayerArea::full(self.width as i32, self.height as i32));
        }
        let (left, center, right) = self.bar_areas(self.width as i32, self.height as i32);
        if layer == GLOBAL_LEFT_LAYER {
            Some(left)
        } else if layer == self.global_right_layer() {
            Some(right)
        } else if self.center_layer() == Some(layer) {
            Some(center)
        } else {
            None
        }
    }

    fn hit_layer(&self, layer: &str, x: f64, y: f64, i: Option<usize>) -> Option<usize> {
        let area = self.area_for_layer(layer)?;
        self.store.get(layer).hit_in(area, x, y, i)
    }

    fn hit_bar(&self, x: f64, y: f64) -> Option<(String, usize)> {
        if let Some(modal) = &self.rstate.modal {
            return self
                .hit_layer(modal, x, y, None)
                .map(|i| (modal.clone(), i));
        }
        for layer in [
            Some(GLOBAL_LEFT_LAYER),
            self.center_layer(),
            Some(self.global_right_layer()),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(i) = self.hit_layer(layer, x, y, None) {
                return Some((layer.to_string(), i));
            }
        }
        None
    }

    /// Dispatch one raw digitizer sample (Down/Motion/Up).
    pub(crate) fn on_touch(&mut self, s: TouchSample) -> Result<()> {
        self.backlight.wake(); // any digitizer touch keeps the bar lit
        let width = self.width;
        let (x, y) = (s.x, s.y);
        match s.phase {
            TouchPhase::Down => {
                if self.backlight.current_bl() == 0 {
                    return Ok(()); // bar is dark; the touch just woke it
                }
                let hit = self.hit_bar(x, y);
                eprintln!("[dbg {:.6}] DOWN x={x:.0} y={y:.0} hit={hit:?}", dbg_ts());
                if let Some((active, btn)) = hit {
                    let area = self
                        .area_for_layer(&active)
                        .ok_or_else(|| anyhow!("no area for active layer `{active}`"))?;
                    let action =
                        self.store
                            .get_mut(&active)
                            .on_press_at(btn, &self.runtime, area, x);
                    // Layer hand-offs and media scrub hand-offs manage their own
                    // lifecycle; everything else is a normal button press/release sequence.
                    if matches!(action, Some(Action::MediaSeek(_))) {
                        self.touches.insert(
                            0,
                            TouchTarget::Media {
                                layer: active.clone(),
                                btn,
                            },
                        );
                    } else if !matches!(
                        action,
                        Some(
                            Action::OpenModal(_)
                                | Action::PushLayer(_)
                                | Action::PopLayer
                                | Action::ChromiumActivateTab(_),
                        )
                    ) {
                        self.touches.insert(
                            0,
                            TouchTarget::Button {
                                layer: active.clone(),
                                btn,
                            },
                        );
                    }
                    self.apply(action, x)?;
                }
            }
            TouchPhase::Motion => match self.touches.get(&0) {
                Some(TouchTarget::Button { layer, btn }) => {
                    let (layer, btn) = (layer.clone(), *btn);
                    // Follow the finger: re-pressing re-lights + re-emits,
                    // leaving releases — both no-ops if already in that
                    // state, so we just call the matching edge each move.
                    let hit = self.hit_layer(&layer, x, y, Some(btn)).is_some();
                    let area = self
                        .area_for_layer(&layer)
                        .ok_or_else(|| anyhow!("no area for active layer `{layer}`"))?;
                    let action = if hit {
                        self.store
                            .get_mut(&layer)
                            .on_press_at(btn, &self.runtime, area, x)
                    } else {
                        self.store
                            .get_mut(&layer)
                            .on_release_at(btn, &self.runtime, area)
                    };
                    self.apply(action, x)?;
                }
                Some(TouchTarget::Media { layer, btn }) => {
                    let (layer, btn) = (layer.clone(), *btn);
                    let area = self
                        .area_for_layer(&layer)
                        .ok_or_else(|| anyhow!("no area for active layer `{layer}`"))?;
                    let action = self
                        .store
                        .get_mut(&layer)
                        .on_drag_at(btn, &self.runtime, area, x);
                    self.apply(action, x)?;
                }
                Some(TouchTarget::Slider { layer }) => {
                    let layer = layer.clone();
                    let action = self.store.get_mut(&layer).drag_slider(x, width as f64);
                    self.apply(action, x)?;
                }
                None => {}
            },
            TouchPhase::Up => {
                let removed = self.touches.remove(&0);
                eprintln!("[dbg {:.6}] UP removed={}", dbg_ts(), removed.is_some());
                let action = match removed {
                    Some(TouchTarget::Button { layer, btn }) => {
                        let area = self
                            .area_for_layer(&layer)
                            .ok_or_else(|| anyhow!("no area for active layer `{layer}`"))?;
                        self.store
                            .get_mut(&layer)
                            .on_release_at(btn, &self.runtime, area)
                    }
                    Some(TouchTarget::Media { layer, btn }) => {
                        let area = self
                            .area_for_layer(&layer)
                            .ok_or_else(|| anyhow!("no area for active layer `{layer}`"))?;
                        self.store
                            .get_mut(&layer)
                            .on_release_at(btn, &self.runtime, area)
                    }
                    Some(TouchTarget::Slider { .. }) => Some(Action::CloseModal),
                    None => None,
                };
                self.apply(action, x)?;
            }
        }
        Ok(())
    }

    /// Drive the backlight idle/dim state machine — called at the end of each loop
    /// iteration, after touch handling has had its chance to `wake` it.
    pub(crate) fn update_backlight(&mut self) {
        self.backlight.update_backlight(&self.cfg);
    }

    /// The single effectful site: turn an `Action` returned by a widget into a real
    /// effect (emit a key, set brightness, enter/leave a modal). `x` is the touch's
    /// long-axis position, used to anchor a slider grab on `OpenModal`.
    pub(crate) fn apply(&mut self, action: Option<Action>, x: f64) -> Result<()> {
        let Some(action) = action else { return Ok(()) };
        match action {
            Action::Key(keys, edge) => {
                toggle_keys(&mut self.uinput, &keys, (edge == Edge::Press) as i32);
            }
            Action::SetBrightness(level) => {
                self.backlight.set_display_level(level);
                let key = StateKey::new(key::HARDWARE_BRIGHTNESS)?;
                self.refresh_key(&key)?;
            }
            Action::SetKbdIllum(level) => {
                self.kbd.set_level(level);
                let key = StateKey::new(key::HARDWARE_KBD_ILLUM)?;
                self.refresh_key(&key)?;
            }
            Action::SetVolume(level) => {
                self.volume.set_level(level);
                self.runtime.set(key::AUDIO_VOLUME, Value::Number(level))?;
            }
            Action::MediaPrevious => {
                self.mpris.previous()?;
                self.refresh_media()?;
            }
            Action::MediaPlayPause => {
                self.mpris.play_pause()?;
                self.refresh_media()?;
            }
            Action::MediaNext => {
                self.mpris.next()?;
                self.refresh_media()?;
            }
            Action::MediaSeek(position_us) => {
                self.mpris.seek(position_us)?;
                self.runtime
                    .set(key::MEDIA_ACTIVE_POSITION, Value::Number(position_us))?;
            }
            Action::ChromiumActivateTab(id) => {
                self.chromium.activate_tab(&id)?;
                self.refresh_chromium_tabs(true)?;
                self.needs_complete_redraw = true;
            }
            Action::PushLayer(layer) => {
                self.rstate.modal = Some(layer);
                self.needs_complete_redraw = true;
            }
            Action::PopLayer => {
                if self.rstate.modal.is_some() {
                    self.rstate.modal = None;
                } else {
                    self.rstate.substates.pop();
                }
                self.needs_complete_redraw = true;
            }
            Action::OpenModal(target) => {
                let slider_key = self.store.get(&target).slider_key()?;
                self.refresh_key(&slider_key)?;
                let level = self.runtime.number(slider_key.as_str())?;
                self.rstate.modal = Some(target.clone());
                self.store.get_mut(&target).grab_slider(level, x);
                self.touches
                    .insert(0, TouchTarget::Slider { layer: target });
                self.needs_complete_redraw = true;
            }
            Action::CloseModal => {
                if let Some(layer) = self.rstate.modal.take() {
                    self.store.get_mut(&layer).release_slider();
                }
                self.needs_complete_redraw = true;
            }
        }
        Ok(())
    }
}
