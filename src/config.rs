use crate::fonts::{FontConfig, Pattern};
use crate::function_layer::FunctionLayer;
use crate::layer::LayerStore;
use crate::widgets::{BrightnessSlider, KbdIllumSlider, VolumeSlider};
use anyhow::{anyhow, Context, Result};
use cairo::FontFace;
use freetype::Library as FtLibrary;
use input_linux::Key;
use nix::{
    errno::Errno,
    sys::inotify::{AddWatchFlags, InitFlags, Inotify, InotifyEvent, WatchDescriptor},
};
use serde::{
    de::{self, Visitor},
    Deserialize, Deserializer,
};
use std::{
    collections::HashMap, env, fmt, fs::read_to_string, io::ErrorKind, os::fd::AsFd, path::PathBuf,
};

const USER_CFG_REL_PATH: &str = ".config/tiny-dfr/config.toml";

fn user_cfg_path() -> Option<PathBuf> {
    env::var_os("HOME").map(|home| PathBuf::from(home).join(USER_CFG_REL_PATH))
}

pub struct Config {
    pub show_button_outlines: bool,
    pub enable_pixel_shift: bool,
    pub font_face: FontFace,
    pub adaptive_brightness: bool,
    pub active_brightness: u32,
    pub double_press_switch_layers: u32,
    /// Focused-window class -> layer name. When the focused app matches, that layer
    /// takes precedence over the base layers (see `app.rs::on_focus`).
    pub app_layers: HashMap<String, String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ConfigProxy {
    media_layer_default: Option<bool>,
    show_button_outlines: Option<bool>,
    enable_pixel_shift: Option<bool>,
    font_template: Option<String>,
    adaptive_brightness: Option<bool>,
    active_brightness: Option<u32>,
    double_press_switch_layers: Option<u32>,
    primary_layer_keys: Option<Vec<ButtonConfig>>,
    media_layer_keys: Option<Vec<ButtonConfig>>,
    app_layers: Option<HashMap<String, String>>,
    layers: Option<HashMap<String, LayerConfig>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LayerConfig {
    keys: Vec<ButtonConfig>,
}

fn array_or_single<'de, D>(deserializer: D) -> Result<Vec<Key>, D::Error>
where
    D: Deserializer<'de>,
{
    struct ArrayOrSingle;

    impl<'de> Visitor<'de> for ArrayOrSingle {
        type Value = Vec<Key>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("string or array of strings")
        }

        fn visit_str<E: de::Error>(self, value: &str) -> Result<Vec<Key>, E> {
            Ok(vec![Deserialize::deserialize(
                de::value::BorrowedStrDeserializer::new(value),
            )?])
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, seq: A) -> Result<Vec<Key>, A::Error> {
            Deserialize::deserialize(de::value::SeqAccessDeserializer::new(seq))
        }
    }

    deserializer.deserialize_any(ArrayOrSingle)
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ButtonConfig {
    #[serde(alias = "Svg")]
    pub icon: Option<String>,
    pub text: Option<String>,
    pub theme: Option<String>,
    pub time: Option<String>,
    pub battery: Option<String>,
    pub media: Option<String>,
    pub chromium_tabs: Option<String>,
    pub locale: Option<String>,
    #[serde(deserialize_with = "array_or_single", default)]
    pub action: Vec<Key>,
    pub stretch: Option<usize>,
    pub icon_width: Option<i32>,
    pub icon_height: Option<i32>,
    /// Touching this button opens the named layer (e.g. a slider) as a
    /// momentary modal instead of sending a key.
    pub open_layer: Option<String>,
    /// Touching this button opens a named full-bar transient overlay.
    pub push_layer: Option<String>,
    /// Touching this button closes the current transient overlay.
    pub pop_layer: Option<bool>,
}

/// Unwrap a config field the merged base config is expected to define. A missing
/// one means the shipped `/usr/share/tiny-dfr/config.toml` is incomplete (an
/// install error), surfaced as an error rather than a panic.
fn require<T>(value: Option<T>, key: &str) -> Result<T> {
    value.ok_or_else(|| anyhow!("config is missing required key `{key}`"))
}

fn load_font(name: &str) -> Result<FontFace> {
    let fontconfig = FontConfig::new();
    let mut pattern = Pattern::new(name);
    fontconfig.perform_substitutions(&mut pattern);
    let pat_match = fontconfig.match_pattern(&pattern).map_err(|_| {
        anyhow!("no font matches template '{name}'; ensure at least one font is installed")
    })?;
    let file_name = pat_match.get_file_name();
    let file_idx = pat_match.get_font_index();
    let ft_library = FtLibrary::init().map_err(|e| anyhow!("FreeType init failed: {e}"))?;
    let face = ft_library
        .new_face(file_name, file_idx)
        .map_err(|e| anyhow!("loading font face '{file_name}': {e}"))?;
    FontFace::create_from_ft(&face).map_err(|e| anyhow!("creating cairo font face: {e}"))
}

fn load_config(width: u16) -> Result<(Config, LayerStore)> {
    let base_str = read_to_string("/usr/share/tiny-dfr/config.toml")
        .context("reading /usr/share/tiny-dfr/config.toml")?;
    let mut base = toml::from_str::<ConfigProxy>(&base_str)
        .context("parsing /usr/share/tiny-dfr/config.toml")?;

    // A user config is optional (absence is normal), but if one exists it must
    // parse — surfacing the error rejects the reload and keeps the running config,
    // rather than silently reverting to the shipped defaults.
    if let Some(user_cfg_path) = user_cfg_path() {
        match read_to_string(&user_cfg_path) {
            Ok(user_str) => {
                let user = toml::from_str::<ConfigProxy>(&user_str)
                    .with_context(|| format!("parsing {}", user_cfg_path.display()))?;
                base.media_layer_default = user.media_layer_default.or(base.media_layer_default);
                base.show_button_outlines = user.show_button_outlines.or(base.show_button_outlines);
                base.enable_pixel_shift = user.enable_pixel_shift.or(base.enable_pixel_shift);
                base.font_template = user.font_template.or(base.font_template);
                base.adaptive_brightness = user.adaptive_brightness.or(base.adaptive_brightness);
                base.media_layer_keys = user.media_layer_keys.or(base.media_layer_keys);
                base.primary_layer_keys = user.primary_layer_keys.or(base.primary_layer_keys);
                base.active_brightness = user.active_brightness.or(base.active_brightness);
                base.double_press_switch_layers = user
                    .double_press_switch_layers
                    .or(base.double_press_switch_layers);
                base.app_layers = user.app_layers.or(base.app_layers);
                if let Some(user_layers) = user.layers {
                    base.layers
                        .get_or_insert_with(HashMap::new)
                        .extend(user_layers);
                }
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => {
                return Err(
                    anyhow::Error::from(e).context(format!("reading {}", user_cfg_path.display()))
                )
            }
        }
    }
    let mut media_layer_keys = require(base.media_layer_keys.take(), "MediaLayerKeys")?;
    let mut primary_layer_keys = require(base.primary_layer_keys.take(), "PrimaryLayerKeys")?;
    if width >= 2170 {
        for layer in [&mut media_layer_keys, &mut primary_layer_keys] {
            layer.insert(
                0,
                ButtonConfig {
                    icon: None,
                    text: Some("esc".into()),
                    theme: None,
                    action: vec![Key::Esc],
                    stretch: None,
                    time: None,
                    locale: None,
                    battery: None,
                    media: None,
                    chromium_tabs: None,
                    icon_width: None,
                    icon_height: None,
                    open_layer: None,
                    push_layer: None,
                    pop_layer: None,
                },
            );
        }
    }
    let media_layer =
        FunctionLayer::with_config(media_layer_keys).context("building the media layer")?;
    let fkey_layer =
        FunctionLayer::with_config(primary_layer_keys).context("building the primary layer")?;
    let global_left_layer = FunctionLayer::with_config(vec![ButtonConfig {
        icon: None,
        text: Some("esc".into()),
        theme: None,
        action: vec![Key::Esc],
        stretch: None,
        time: None,
        locale: None,
        battery: None,
        media: None,
        chromium_tabs: None,
        icon_width: None,
        icon_height: None,
        open_layer: None,
        push_layer: None,
        pop_layer: None,
    }])
    .context("building the global-left layer")?;
    let volume_button = ButtonConfig {
        icon: Some("volume_up".into()),
        text: None,
        theme: None,
        action: Vec::new(),
        stretch: None,
        time: None,
        locale: None,
        battery: None,
        media: None,
        chromium_tabs: None,
        icon_width: None,
        icon_height: None,
        open_layer: Some("volume".into()),
        push_layer: None,
        pop_layer: None,
    };
    let brightness_button = ButtonConfig {
        icon: Some("brightness_high".into()),
        text: None,
        theme: None,
        action: Vec::new(),
        stretch: None,
        time: None,
        locale: None,
        battery: None,
        media: None,
        chromium_tabs: None,
        icon_width: None,
        icon_height: None,
        open_layer: Some("brightness".into()),
        push_layer: None,
        pop_layer: None,
    };
    let global_right_layer = FunctionLayer::with_config(vec![volume_button, brightness_button])
        .context("building the global-right layer")?;
    let global_right_media_layer = FunctionLayer::with_config(vec![
        ButtonConfig {
            icon: Some("play_pause".into()),
            text: None,
            theme: None,
            action: Vec::new(),
            stretch: None,
            time: None,
            locale: None,
            battery: None,
            media: None,
            chromium_tabs: None,
            icon_width: None,
            icon_height: None,
            open_layer: None,
            push_layer: Some("media-overlay".into()),
            pop_layer: None,
        },
        ButtonConfig {
            icon: Some("volume_up".into()),
            text: None,
            theme: None,
            action: Vec::new(),
            stretch: None,
            time: None,
            locale: None,
            battery: None,
            media: None,
            chromium_tabs: None,
            icon_width: None,
            icon_height: None,
            open_layer: Some("volume".into()),
            push_layer: None,
            pop_layer: None,
        },
        ButtonConfig {
            icon: Some("brightness_high".into()),
            text: None,
            theme: None,
            action: Vec::new(),
            stretch: None,
            time: None,
            locale: None,
            battery: None,
            media: None,
            chromium_tabs: None,
            icon_width: None,
            icon_height: None,
            open_layer: Some("brightness".into()),
            push_layer: None,
            pop_layer: None,
        },
    ])
    .context("building the global-right-media layer")?;
    let media_active_layer = FunctionLayer::with_config(vec![ButtonConfig {
        icon: None,
        text: None,
        theme: None,
        action: Vec::new(),
        stretch: Some(8),
        time: None,
        locale: None,
        battery: None,
        media: Some("active".into()),
        chromium_tabs: None,
        icon_width: None,
        icon_height: None,
        open_layer: None,
        push_layer: None,
        pop_layer: None,
    }])
    .context("building the media-active layer")?;
    let media_overlay_layer = FunctionLayer::with_config(vec![
        ButtonConfig {
            icon: None,
            text: None,
            theme: None,
            action: Vec::new(),
            stretch: Some(16),
            time: None,
            locale: None,
            battery: None,
            media: Some("active".into()),
            chromium_tabs: None,
            icon_width: None,
            icon_height: None,
            open_layer: None,
            push_layer: None,
            pop_layer: None,
        },
        ButtonConfig {
            icon: None,
            text: Some("×".into()),
            theme: None,
            action: Vec::new(),
            stretch: Some(1),
            time: None,
            locale: None,
            battery: None,
            media: None,
            chromium_tabs: None,
            icon_width: None,
            icon_height: None,
            open_layer: None,
            push_layer: None,
            pop_layer: Some(true),
        },
    ])
    .context("building the media-overlay layer")?;
    let chromium_tabs_layer = FunctionLayer::with_config(vec![ButtonConfig {
        icon: None,
        text: None,
        theme: None,
        action: Vec::new(),
        stretch: Some(8),
        time: None,
        locale: None,
        battery: None,
        media: None,
        chromium_tabs: Some("active".into()),
        icon_width: None,
        icon_height: None,
        open_layer: None,
        push_layer: None,
        pop_layer: None,
    }])
    .context("building the chromium-tabs layer")?;
    let chromium_tabs_overlay_layer = FunctionLayer::with_config(vec![
        ButtonConfig {
            icon: None,
            text: None,
            theme: None,
            action: Vec::new(),
            stretch: Some(16),
            time: None,
            locale: None,
            battery: None,
            media: None,
            chromium_tabs: Some("active".into()),
            icon_width: None,
            icon_height: None,
            open_layer: None,
            push_layer: None,
            pop_layer: None,
        },
        ButtonConfig {
            icon: None,
            text: Some("×".into()),
            theme: None,
            action: Vec::new(),
            stretch: Some(1),
            time: None,
            locale: None,
            battery: None,
            media: None,
            chromium_tabs: None,
            icon_width: None,
            icon_height: None,
            open_layer: None,
            push_layer: None,
            pop_layer: Some(true),
        },
    ])
    .context("building the chromium-tabs-overlay layer")?;
    let global_right_tabs_layer = FunctionLayer::with_config(vec![
        ButtonConfig {
            icon: None,
            text: Some("Tabs".into()),
            theme: None,
            action: Vec::new(),
            stretch: None,
            time: None,
            locale: None,
            battery: None,
            media: None,
            chromium_tabs: None,
            icon_width: None,
            icon_height: None,
            open_layer: None,
            push_layer: Some("chromium-tabs-overlay".into()),
            pop_layer: None,
        },
        ButtonConfig {
            icon: Some("volume_up".into()),
            text: None,
            theme: None,
            action: Vec::new(),
            stretch: None,
            time: None,
            locale: None,
            battery: None,
            media: None,
            chromium_tabs: None,
            icon_width: None,
            icon_height: None,
            open_layer: Some("volume".into()),
            push_layer: None,
            pop_layer: None,
        },
        ButtonConfig {
            icon: Some("brightness_high".into()),
            text: None,
            theme: None,
            action: Vec::new(),
            stretch: None,
            time: None,
            locale: None,
            battery: None,
            media: None,
            chromium_tabs: None,
            icon_width: None,
            icon_height: None,
            open_layer: Some("brightness".into()),
            push_layer: None,
            pop_layer: None,
        },
    ])
    .context("building the global-right-tabs layer")?;
    let mut registry = HashMap::new();
    registry.insert("media".to_string(), media_layer);
    registry.insert("fkeys".to_string(), fkey_layer);
    registry.insert("global-left".to_string(), global_left_layer);
    registry.insert("global-right".to_string(), global_right_layer);
    registry.insert("global-right-media".to_string(), global_right_media_layer);
    registry.insert("global-right-tabs".to_string(), global_right_tabs_layer);
    registry.insert("media-active".to_string(), media_active_layer);
    registry.insert("media-overlay".to_string(), media_overlay_layer);
    registry.insert("chromium-tabs".to_string(), chromium_tabs_layer);
    registry.insert(
        "chromium-tabs-overlay".to_string(),
        chromium_tabs_overlay_layer,
    );
    for (name, layer) in base.layers.take().unwrap_or_default() {
        registry.insert(
            name.clone(),
            FunctionLayer::with_config(layer.keys)
                .with_context(|| format!("building configured layer `{name}`"))?,
        );
    }
    // Built-in modal slider layers, entered via a button's `OpenLayer = "..."`.
    registry.insert(
        "brightness".to_string(),
        FunctionLayer::slider_layer(Box::new(BrightnessSlider)),
    );
    registry.insert(
        "kbd_illum".to_string(),
        FunctionLayer::slider_layer(Box::new(KbdIllumSlider)),
    );
    registry.insert(
        "volume".to_string(),
        FunctionLayer::slider_layer(Box::new(VolumeSlider)),
    );
    // base_order[0] = shown when Fn is not held; base_order[1] = shown while Fn held.
    let base_order = if require(base.media_layer_default, "MediaLayerDefault")? {
        ["media".to_string(), "fkeys".to_string()]
    } else {
        ["fkeys".to_string(), "media".to_string()]
    };
    for (class, layer) in base.app_layers.as_ref().into_iter().flatten() {
        if !registry.contains_key(layer) {
            return Err(anyhow!(
                "AppLayers maps class `{class}` to unknown layer `{layer}`"
            ));
        }
    }

    let cfg = Config {
        show_button_outlines: require(base.show_button_outlines, "ShowButtonOutlines")?,
        enable_pixel_shift: require(base.enable_pixel_shift, "EnablePixelShift")?,
        adaptive_brightness: require(base.adaptive_brightness, "AdaptiveBrightness")?,
        font_face: load_font(&require(base.font_template, "FontTemplate")?)?,
        active_brightness: require(base.active_brightness, "ActiveBrightness")?,
        double_press_switch_layers: require(
            base.double_press_switch_layers,
            "DoublePressSwitchLayers",
        )?,
        app_layers: base.app_layers.unwrap_or_default(),
    };
    Ok((
        cfg,
        LayerStore {
            registry,
            base_order,
        },
    ))
}

pub struct ConfigManager {
    inotify_fd: Inotify,
    watch_desc: Option<WatchDescriptor>,
}

fn arm_inotify(inotify_fd: &Inotify) -> Option<WatchDescriptor> {
    let path = user_cfg_path()?;
    let flags = AddWatchFlags::IN_MOVED_TO | AddWatchFlags::IN_CLOSE | AddWatchFlags::IN_ONESHOT;
    match inotify_fd.add_watch(&path, flags) {
        Ok(wd) => Some(wd),
        // The config file not existing yet is normal — there's just nothing to
        // watch. Any other error disables live reload rather than crashing.
        Err(Errno::ENOENT) => None,
        Err(e) => {
            eprintln!(
                "failed to watch {}, live config reload disabled: {e}",
                path.display()
            );
            None
        }
    }
}

impl ConfigManager {
    pub fn new() -> ConfigManager {
        let inotify_fd = Inotify::init(InitFlags::IN_NONBLOCK)
            .expect("failed to initialize inotify for config-file watching");
        let watch_desc = arm_inotify(&inotify_fd);
        ConfigManager {
            inotify_fd,
            watch_desc,
        }
    }
    pub fn load_config(&self, width: u16) -> Result<(Config, LayerStore)> {
        load_config(width)
    }
    pub fn update_config(&mut self, cfg: &mut Config, store: &mut LayerStore, width: u16) -> bool {
        if self.watch_desc.is_none() {
            self.watch_desc = arm_inotify(&self.inotify_fd);
            return false;
        }
        match self.inotify_fd.read_events() {
            Err(Errno::EAGAIN) => false,
            r => self.handle_events(cfg, store, width, r),
        }
    }
    #[cold]
    fn handle_events(
        &mut self,
        cfg: &mut Config,
        store: &mut LayerStore,
        width: u16,
        evts: Result<Vec<InotifyEvent>, Errno>,
    ) -> bool {
        let evts = match evts {
            Ok(evts) => evts,
            Err(e) => {
                eprintln!("inotify read failed, skipping config reload: {e}");
                return false;
            }
        };
        let mut ret = false;
        for evt in evts {
            if Some(evt.wd) != self.watch_desc {
                continue;
            }
            match load_config(width) {
                Ok((new_cfg, new_store)) => {
                    *cfg = new_cfg;
                    *store = new_store;
                    ret = true;
                }
                Err(e) => {
                    eprintln!("config reload failed, keeping the previous config: {e:#}");
                }
            }
            // Re-arm AFTER the load: load_config reads (opens+closes) the watched
            // file, and the watch includes IN_CLOSE — re-arming beforehand would
            // catch that close and spin an endless reload loop. IN_ONESHOT already
            // consumed the firing watch, so re-arm unconditionally (even on a failed
            // load) so a later fixing edit still triggers a reload.
            self.watch_desc = arm_inotify(&self.inotify_fd);
        }
        ret
    }
    pub fn fd(&self) -> &impl AsFd {
        &self.inotify_fd
    }
}
