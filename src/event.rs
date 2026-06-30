use crate::touch::TouchSample;

pub(crate) enum AppEvent {
    Libinput(::input::event::Event),
    Touch(TouchSample),
    FocusChanged { class: String, title: String },
    ConfigReload,
    Tick,
}
