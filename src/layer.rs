use crate::function_layer::FunctionLayer;
use std::collections::HashMap;

/// All layers keyed by name, plus the Fn-selected order of the two base layers.
/// A layer is just a `FunctionLayer` (a row of cells); a modal slider is a
/// one-cell `FunctionLayer`, so the event loop stays layer-kind agnostic.
pub struct LayerStore {
    pub registry: HashMap<String, FunctionLayer>,
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
    pub fn get(&self, name: &str) -> &FunctionLayer {
        self.registry
            .get(name)
            .expect("resolved layer must exist in the registry")
    }
    pub fn get_mut(&mut self, name: &str) -> &mut FunctionLayer {
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
    Media { layer: String, btn: usize },
    Slider { layer: String },
}
