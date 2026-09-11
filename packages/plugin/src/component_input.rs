//! Input lowering for retained component producers.
use crate::AttachInputEvent;
use bmux_tui::{
    event::{Event, MouseButton, MouseEvent, MouseEventKind},
    geometry::Point,
};

/// Convert a routed attach input event without inventing missing coordinates.
#[must_use]
pub fn component_event(input: &AttachInputEvent) -> Option<Event> {
    if input.event_kind == "key" && matches!(input.phase.as_str(), "press" | "repeat") {
        let mut stroke = bmux_keyboard::parse_key_stroke(input.key.as_deref()?).ok()?;
        stroke.modifiers.shift |= input.modifiers.shift;
        stroke.modifiers.ctrl |= input.modifiers.control;
        stroke.modifiers.alt |= input.modifiers.alt;
        stroke.modifiers.super_key |= input.modifiers.super_key;
        stroke.modifiers.hyper |= input.modifiers.hyper;
        stroke.modifiers.meta |= input.modifiers.meta;
        return Some(Event::Key(stroke));
    }
    if input.event_kind != "pointer" {
        return None;
    }
    let button = match input.button.as_deref() {
        Some("left") => Some(MouseButton::Left),
        Some("right") => Some(MouseButton::Right),
        Some("middle") => Some(MouseButton::Middle),
        _ => None,
    };
    let kind = match input.phase.as_str() {
        "enter" | "move" | "leave" => MouseEventKind::Move,
        "down" => MouseEventKind::Down(button?),
        "up" => MouseEventKind::Up(button?),
        "drag" => MouseEventKind::Drag(button?),
        "wheel" if input.wheel_delta > 0 => MouseEventKind::ScrollUp,
        "wheel" if input.wheel_delta < 0 => MouseEventKind::ScrollDown,
        _ => return None,
    };
    Some(Event::Mouse(
        MouseEvent::new(kind, Point::new(input.col?, input.row?)).with_modifiers(
            bmux_tui::event::MouseModifiers {
                shift: input.modifiers.shift,
                alt: input.modifiers.alt,
                ctrl: input.modifiers.control,
            },
        ),
    ))
}
