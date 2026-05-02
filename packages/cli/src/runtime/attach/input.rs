use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

/// Terminal input normalized for attach UI reducers.
///
/// Coordinates use the same 0-based row/column convention as crossterm and the
/// existing attach/status rendering code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalInputEvent {
    Key(TerminalKeyEvent),
    Mouse(TerminalMouseEvent),
    Resize { cols: u16, rows: u16 },
    Bytes(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalKeyEvent {
    pub code: TerminalKeyCode,
    pub kind: TerminalKeyPhase,
    pub modifiers: TerminalModifiers,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalKeyCode {
    Backspace,
    Enter,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Tab,
    BackTab,
    Delete,
    Insert,
    Function(u8),
    Char(char),
    Null,
    Esc,
    CapsLock,
    ScrollLock,
    NumLock,
    PrintScreen,
    Pause,
    Menu,
    KeypadBegin,
    Media(String),
    Modifier(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalKeyPhase {
    Press,
    Repeat,
    Release,
}

// Keep modifiers as explicit booleans so simulation inputs are independent of
// crossterm's bitflag representation and easy for playbooks to construct.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TerminalModifiers {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    pub super_key: bool,
    pub hyper: bool,
    pub meta: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalMouseEvent {
    pub phase: TerminalMousePhase,
    pub button: Option<TerminalMouseButton>,
    pub col: u16,
    pub row: u16,
    pub modifiers: TerminalModifiers,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalMousePhase {
    Down,
    Up,
    Drag,
    Move,
    ScrollUp,
    ScrollDown,
    ScrollLeft,
    ScrollRight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalMouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalGeometry {
    pub cols: u16,
    pub rows: u16,
}

impl TerminalInputEvent {
    pub fn from_crossterm_event(event: crossterm::event::Event) -> Option<Self> {
        match event {
            crossterm::event::Event::Key(key) => Some(Self::Key(TerminalKeyEvent::from(key))),
            crossterm::event::Event::Mouse(mouse) => {
                Some(Self::Mouse(TerminalMouseEvent::from(mouse)))
            }
            crossterm::event::Event::Resize(cols, rows) => Some(Self::Resize { cols, rows }),
            crossterm::event::Event::Paste(text) => Some(Self::Bytes(text.into_bytes())),
            crossterm::event::Event::FocusGained | crossterm::event::Event::FocusLost => None,
        }
    }
}

impl From<KeyEvent> for TerminalKeyEvent {
    fn from(event: KeyEvent) -> Self {
        Self {
            code: TerminalKeyCode::from(event.code),
            kind: TerminalKeyPhase::from(event.kind),
            modifiers: TerminalModifiers::from(event.modifiers),
        }
    }
}

impl From<KeyCode> for TerminalKeyCode {
    fn from(code: KeyCode) -> Self {
        match code {
            KeyCode::Backspace => Self::Backspace,
            KeyCode::Enter => Self::Enter,
            KeyCode::Left => Self::Left,
            KeyCode::Right => Self::Right,
            KeyCode::Up => Self::Up,
            KeyCode::Down => Self::Down,
            KeyCode::Home => Self::Home,
            KeyCode::End => Self::End,
            KeyCode::PageUp => Self::PageUp,
            KeyCode::PageDown => Self::PageDown,
            KeyCode::Tab => Self::Tab,
            KeyCode::BackTab => Self::BackTab,
            KeyCode::Delete => Self::Delete,
            KeyCode::Insert => Self::Insert,
            KeyCode::F(index) => Self::Function(index),
            KeyCode::Char(value) => Self::Char(value),
            KeyCode::Null => Self::Null,
            KeyCode::Esc => Self::Esc,
            KeyCode::CapsLock => Self::CapsLock,
            KeyCode::ScrollLock => Self::ScrollLock,
            KeyCode::NumLock => Self::NumLock,
            KeyCode::PrintScreen => Self::PrintScreen,
            KeyCode::Pause => Self::Pause,
            KeyCode::Menu => Self::Menu,
            KeyCode::KeypadBegin => Self::KeypadBegin,
            KeyCode::Media(value) => Self::Media(format!("{value:?}")),
            KeyCode::Modifier(value) => Self::Modifier(format!("{value:?}")),
        }
    }
}

impl From<KeyEventKind> for TerminalKeyPhase {
    fn from(kind: KeyEventKind) -> Self {
        match kind {
            KeyEventKind::Press => Self::Press,
            KeyEventKind::Repeat => Self::Repeat,
            KeyEventKind::Release => Self::Release,
        }
    }
}

impl From<MouseEvent> for TerminalMouseEvent {
    fn from(event: MouseEvent) -> Self {
        let (phase, button) = match event.kind {
            MouseEventKind::Down(button) => (TerminalMousePhase::Down, Some(button.into())),
            MouseEventKind::Up(button) => (TerminalMousePhase::Up, Some(button.into())),
            MouseEventKind::Drag(button) => (TerminalMousePhase::Drag, Some(button.into())),
            MouseEventKind::Moved => (TerminalMousePhase::Move, None),
            MouseEventKind::ScrollUp => (TerminalMousePhase::ScrollUp, None),
            MouseEventKind::ScrollDown => (TerminalMousePhase::ScrollDown, None),
            MouseEventKind::ScrollLeft => (TerminalMousePhase::ScrollLeft, None),
            MouseEventKind::ScrollRight => (TerminalMousePhase::ScrollRight, None),
        };
        Self {
            phase,
            button,
            col: event.column,
            row: event.row,
            modifiers: TerminalModifiers::from(event.modifiers),
        }
    }
}

impl From<MouseButton> for TerminalMouseButton {
    fn from(button: MouseButton) -> Self {
        match button {
            MouseButton::Left => Self::Left,
            MouseButton::Right => Self::Right,
            MouseButton::Middle => Self::Middle,
        }
    }
}

impl From<KeyModifiers> for TerminalModifiers {
    fn from(modifiers: KeyModifiers) -> Self {
        Self {
            shift: modifiers.contains(KeyModifiers::SHIFT),
            control: modifiers.contains(KeyModifiers::CONTROL),
            alt: modifiers.contains(KeyModifiers::ALT),
            super_key: modifiers.contains(KeyModifiers::SUPER),
            hyper: modifiers.contains(KeyModifiers::HYPER),
            meta: modifiers.contains(KeyModifiers::META),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        TerminalInputEvent, TerminalModifiers, TerminalMouseButton, TerminalMouseEvent,
        TerminalMousePhase,
    };
    use crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

    #[test]
    fn maps_crossterm_mouse_down() {
        let event = TerminalMouseEvent::from(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 4,
            row: 9,
            modifiers: KeyModifiers::SHIFT | KeyModifiers::CONTROL,
        });

        assert_eq!(
            event,
            TerminalMouseEvent {
                phase: TerminalMousePhase::Down,
                button: Some(TerminalMouseButton::Left),
                col: 4,
                row: 9,
                modifiers: TerminalModifiers {
                    shift: true,
                    control: true,
                    alt: false,
                    super_key: false,
                    hyper: false,
                    meta: false,
                },
            }
        );
    }

    #[test]
    fn maps_crossterm_mouse_up_and_drag() {
        let up = TerminalMouseEvent::from(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Right),
            column: 5,
            row: 7,
            modifiers: KeyModifiers::empty(),
        });
        assert_eq!(up.phase, TerminalMousePhase::Up);
        assert_eq!(up.button, Some(TerminalMouseButton::Right));

        let drag = TerminalMouseEvent::from(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Middle),
            column: 9,
            row: 3,
            modifiers: KeyModifiers::empty(),
        });
        assert_eq!(drag.phase, TerminalMousePhase::Drag);
        assert_eq!(drag.button, Some(TerminalMouseButton::Middle));
    }

    #[test]
    fn maps_crossterm_mouse_motion_and_scroll() {
        let moved = TerminalMouseEvent::from(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 8,
            row: 2,
            modifiers: KeyModifiers::empty(),
        });
        assert_eq!(moved.phase, TerminalMousePhase::Move);
        assert_eq!(moved.button, None);

        let scroll = TerminalMouseEvent::from(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 8,
            row: 2,
            modifiers: KeyModifiers::ALT,
        });
        assert_eq!(scroll.phase, TerminalMousePhase::ScrollDown);
        assert!(scroll.modifiers.alt);
    }

    #[test]
    fn maps_crossterm_event_resize() {
        assert_eq!(
            TerminalInputEvent::from_crossterm_event(Event::Resize(120, 30)),
            Some(TerminalInputEvent::Resize {
                cols: 120,
                rows: 30
            })
        );
    }
}
