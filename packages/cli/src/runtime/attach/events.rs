use super::state::AttachExitReason;
use bmux_plugin_sdk::ActionDispatchRequest;
use crossterm::event::Event;

pub enum AttachLoopEvent {
    Server(bmux_client::ServerEvent),
    Terminal(Event),
    /// An action dispatch request from async plugin code.
    ActionDispatch(ActionDispatchRequest),
}

pub enum AttachLoopControl {
    Continue,
    Break(AttachExitReason),
}
