//! Domain-neutral terminal UI event primitives.

use bmux_keyboard::KeyStroke;

use crate::geometry::{Point, Size};

/// A terminal UI input or lifecycle event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Keyboard input.
    Key(KeyStroke),
    /// Mouse input.
    Mouse(MouseEvent),
    /// Terminal viewport was resized.
    Resize(Size),
    /// Bracketed paste payload.
    Paste(String),
    /// Terminal focus changed.
    Focus(FocusEvent),
    /// Periodic tick for animations, polling, or debounced redraws.
    Tick,
    /// Caller-defined event tag for app/plugin integration.
    User(String),
}

impl Event {
    /// Return true when the event should normally trigger a redraw.
    #[must_use]
    pub const fn requests_redraw(&self) -> bool {
        matches!(
            self,
            Self::Key(_)
                | Self::Mouse(_)
                | Self::Resize(_)
                | Self::Paste(_)
                | Self::Focus(_)
                | Self::Tick
                | Self::User(_)
        )
    }

    /// Return the resize size when this is a resize event.
    #[must_use]
    pub const fn resize_size(&self) -> Option<Size> {
        match self {
            Self::Resize(size) => Some(*size),
            Self::Key(_)
            | Self::Mouse(_)
            | Self::Paste(_)
            | Self::Focus(_)
            | Self::Tick
            | Self::User(_) => None,
        }
    }

    /// Return the key stroke when this is a key event.
    #[must_use]
    pub const fn key(&self) -> Option<KeyStroke> {
        match self {
            Self::Key(stroke) => Some(*stroke),
            Self::Mouse(_)
            | Self::Resize(_)
            | Self::Paste(_)
            | Self::Focus(_)
            | Self::Tick
            | Self::User(_) => None,
        }
    }
}

/// Terminal focus event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusEvent {
    /// Terminal gained focus.
    Gained,
    /// Terminal lost focus.
    Lost,
}

/// Mouse event with terminal-cell coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    /// Mouse event kind.
    pub kind: MouseEventKind,
    /// Cell position where the event occurred.
    pub position: Point,
    /// Modifier state held during the event.
    pub modifiers: MouseModifiers,
}

impl MouseEvent {
    /// Create a mouse event without modifiers.
    #[must_use]
    pub const fn new(kind: MouseEventKind, position: Point) -> Self {
        Self {
            kind,
            position,
            modifiers: MouseModifiers::NONE,
        }
    }

    /// Set mouse modifiers.
    #[must_use]
    pub const fn with_modifiers(mut self, modifiers: MouseModifiers) -> Self {
        self.modifiers = modifiers;
        self
    }
}

/// Mouse event kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEventKind {
    /// Button press.
    Down(MouseButton),
    /// Button release.
    Up(MouseButton),
    /// Drag with a button held.
    Drag(MouseButton),
    /// Mouse moved without a pressed button.
    Move,
    /// Scroll wheel up.
    ScrollUp,
    /// Scroll wheel down.
    ScrollDown,
    /// Scroll wheel left.
    ScrollLeft,
    /// Scroll wheel right.
    ScrollRight,
}

/// Mouse button identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MouseButton {
    /// Primary button.
    Left,
    /// Secondary button.
    Right,
    /// Middle button.
    Middle,
    /// Additional terminal-reported button.
    Other(u8),
}

/// Mouse modifier flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct MouseModifiers {
    /// Shift key held.
    pub shift: bool,
    /// Alt/option key held.
    pub alt: bool,
    /// Control key held.
    pub ctrl: bool,
}

impl MouseModifiers {
    /// Empty mouse modifiers.
    pub const NONE: Self = Self {
        shift: false,
        alt: false,
        ctrl: false,
    };

    /// Return true when no mouse modifiers are set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        !self.shift && !self.alt && !self.ctrl
    }
}

/// Outcome from dispatching an event to a UI component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventOutcome {
    /// Event was not handled.
    Ignored,
    /// Event was handled and no redraw is needed.
    Handled,
    /// Event was handled and a redraw is requested.
    Redraw,
}

impl EventOutcome {
    /// Combine two outcomes, preserving the strongest redraw request.
    #[must_use]
    pub const fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Redraw, _) | (_, Self::Redraw) => Self::Redraw,
            (Self::Handled, _) | (_, Self::Handled) => Self::Handled,
            (Self::Ignored, Self::Ignored) => Self::Ignored,
        }
    }

    /// Return true when the event was handled.
    #[must_use]
    pub const fn is_handled(self) -> bool {
        matches!(self, Self::Handled | Self::Redraw)
    }

    /// Return true when a redraw is requested.
    #[must_use]
    pub const fn needs_redraw(self) -> bool {
        matches!(self, Self::Redraw)
    }
}

/// Trait for stateful event handlers.
pub trait EventHandler {
    /// Handle a TUI event.
    fn handle_event(&mut self, event: &Event) -> EventOutcome;
}

#[cfg(test)]
mod tests {
    use super::{Event, EventOutcome, MouseButton, MouseEvent, MouseEventKind, MouseModifiers};
    use bmux_keyboard::{KeyCode, KeyStroke};

    use crate::geometry::{Point, Size};

    #[test]
    fn event_accessors_return_matching_payloads() {
        let key = KeyStroke::simple(KeyCode::Enter);
        assert_eq!(Event::Key(key).key(), Some(key));
        assert_eq!(
            Event::Resize(Size::new(80, 24)).resize_size(),
            Some(Size::new(80, 24))
        );
        assert_eq!(Event::Tick.key(), None);
    }

    #[test]
    fn mouse_event_tracks_position_kind_and_modifiers() {
        let modifiers = MouseModifiers {
            ctrl: true,
            ..MouseModifiers::NONE
        };
        let event = MouseEvent::new(MouseEventKind::Down(MouseButton::Left), Point::new(3, 4))
            .with_modifiers(modifiers);

        assert_eq!(event.kind, MouseEventKind::Down(MouseButton::Left));
        assert_eq!(event.position, Point::new(3, 4));
        assert_eq!(event.modifiers, modifiers);
        assert!(!event.modifiers.is_empty());
    }

    #[test]
    fn event_outcomes_merge_to_strongest_value() {
        assert_eq!(
            EventOutcome::Ignored.merge(EventOutcome::Handled),
            EventOutcome::Handled
        );
        assert_eq!(
            EventOutcome::Handled.merge(EventOutcome::Redraw),
            EventOutcome::Redraw
        );
        assert!(EventOutcome::Handled.is_handled());
        assert!(EventOutcome::Redraw.needs_redraw());
    }
}
