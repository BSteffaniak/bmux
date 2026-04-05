use super::state::AttachExitReason;
use crossterm::event::Event;

pub enum AttachLoopEvent {
    Server(bmux_client::ServerEvent),
    Terminal(Event),
}

pub enum AttachLoopControl {
    Continue,
    Break(AttachExitReason),
}
