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

pub fn collect_attach_loop_events(
    server_events: Vec<bmux_client::ServerEvent>,
    terminal_event: Option<Event>,
) -> Vec<AttachLoopEvent> {
    let mut events = server_events
        .into_iter()
        .map(AttachLoopEvent::Server)
        .collect::<Vec<_>>();
    if let Some(event) = terminal_event {
        events.push(AttachLoopEvent::Terminal(event));
    }
    events
}
