use super::input::{TerminalGeometry, TerminalInputEvent};
use super::state::AttachExitReason;
use bmux_plugin_sdk::ActionDispatchRequest;
use crossterm::event::Event;
use std::time::Instant;

pub struct AttachTerminalEvent {
    pub raw: Event,
    pub normalized: Option<TerminalInputEvent>,
    pub received_at: Instant,
    pub geometry: TerminalGeometry,
}

impl AttachTerminalEvent {
    #[must_use]
    pub fn from_crossterm(raw: Event, received_at: Instant, geometry: TerminalGeometry) -> Self {
        let normalized = TerminalInputEvent::from_crossterm_event(raw.clone());
        Self {
            raw,
            normalized,
            received_at,
            geometry,
        }
    }
}

pub enum AttachLoopEvent {
    Server(bmux_client::ServerEvent),
    Terminal(AttachTerminalEvent),
    /// An action dispatch request from async plugin code.
    ActionDispatch(ActionDispatchRequest),
}

pub enum AttachLoopControl {
    Continue,
    Break(AttachExitReason),
}
