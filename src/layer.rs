use crate::config::Config;
use crate::function_layer::FunctionLayer;
use crate::widgets::Slider;
use cairo::Surface;
use drm::control::ClipRect;
use std::collections::HashMap;

/// A single bar layer: either a row of buttons or a full-bar slider widget.
/// The wrapping enum lets the event loop stay layer-kind agnostic.
pub enum Layer {
    Buttons(FunctionLayer),
    Slider(Slider),
}

impl Layer {
    pub fn faster_refresh(&self) -> bool {
        match self {
            Layer::Buttons(l) => l.faster_refresh(),
            Layer::Slider(_) => false,
        }
    }
    pub fn displays_time(&self) -> bool {
        match self {
            Layer::Buttons(l) => l.displays_time(),
            Layer::Slider(_) => false,
        }
    }
    pub fn displays_battery(&self) -> bool {
        match self {
            Layer::Buttons(l) => l.displays_battery(),
            Layer::Slider(_) => false,
        }
    }
    /// Whether anything in this layer changed since the last draw.
    pub fn needs_redraw(&self) -> bool {
        match self {
            Layer::Buttons(l) => l.any_changed(),
            Layer::Slider(s) => s.changed,
        }
    }
    /// Battery state is polled, not event-driven, so force those buttons to redraw.
    pub fn mark_batteries_changed(&mut self) {
        match self {
            Layer::Buttons(l) => l.mark_batteries_changed(),
            Layer::Slider(_) => {}
        }
    }
    pub fn draw(
        &mut self,
        config: &Config,
        width: i32,
        height: i32,
        surface: &Surface,
        pixel_shift: (f64, f64),
        complete_redraw: bool,
    ) -> Vec<ClipRect> {
        match self {
            Layer::Buttons(l) => {
                l.draw(config, width, height, surface, pixel_shift, complete_redraw)
            }
            Layer::Slider(s) => {
                s.draw(config, width, height, surface, pixel_shift, complete_redraw)
            }
        }
    }
}

/// All layers keyed by name, plus the Fn-selected order of the two base layers.
pub struct LayerStore {
    pub registry: HashMap<String, Layer>,
    pub base_order: [String; 2],
}

impl LayerStore {
    /// Resolve which layer name is currently active, by precedence:
    /// `modal` > `substate` > `app` > base (Fn-selected). Names absent from the
    /// registry are skipped. Only the base layer is ever populated in the initial
    /// refactor — the higher-precedence sources arrive in later phases — so this
    /// reduces to exactly the old `active_layer` (0/1) behaviour for now.
    pub fn resolve(&self, st: &ResolverState) -> String {
        for cand in st
            .modal
            .iter()
            .chain(st.substates.last())
            .chain(st.app.iter())
        {
            if self.registry.contains_key(cand) {
                return cand.clone();
            }
        }
        self.base_order[st.fn_pressed as usize].clone()
    }
    pub fn get(&self, name: &str) -> &Layer {
        self.registry
            .get(name)
            .expect("resolved layer must exist in the registry")
    }
    pub fn get_mut(&mut self, name: &str) -> &mut Layer {
        self.registry
            .get_mut(name)
            .expect("resolved layer must exist in the registry")
    }
}

/// The context sources whose highest-precedence active member decides which
/// layer is shown (see [`LayerStore::resolve`]). In the initial refactor only
/// `fn_pressed` is ever set; the rest are wired up in later phases.
#[derive(Default)]
pub struct ResolverState {
    /// Fn held -> select `base_order[1]` (the secondary base layer).
    pub fn_pressed: bool,
    /// Focused-app layer name, set over the control socket (later phase).
    pub app: Option<String>,
    /// App sub-state layer stack, pushed/popped over the socket (later phase).
    pub substates: Vec<String>,
    /// Transient modal layer, e.g. a touch-held slider (later phase).
    pub modal: Option<String>,
}

/// What an in-flight touch (keyed by libinput seat slot) is currently driving.
pub enum TouchTarget {
    Button { layer: String, btn: usize },
    Slider { layer: String },
}
