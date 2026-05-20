//! Crossterm terminal lifecycle and event adapter.

use std::io::{self, Write};

use crossterm::event::{
    Event as CrosstermEvent, KeyCode as CrosstermKeyCode, KeyEvent as CrosstermKeyEvent,
    KeyModifiers as CrosstermKeyModifiers, MouseButton as CrosstermMouseButton,
    MouseEvent as CrosstermMouseEvent, MouseEventKind as CrosstermMouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use crate::event::{Event, FocusEvent, MouseButton, MouseEvent, MouseEventKind, MouseModifiers};
use crate::geometry::{Point, Size};

/// RAII guard for crossterm raw mode and alternate-screen lifecycle.
///
/// The guard enables raw mode and enters the alternate screen on creation. On
/// drop it attempts to leave the alternate screen and disable raw mode.
pub struct CrosstermTerminalGuard<W: Write> {
    writer: Option<W>,
    active: bool,
}

impl<W: Write> CrosstermTerminalGuard<W> {
    /// Enter raw-mode alternate-screen terminal lifecycle.
    ///
    /// # Errors
    ///
    /// Returns any error reported by crossterm while enabling raw mode or
    /// entering the alternate screen.
    pub fn enter(mut writer: W) -> io::Result<Self> {
        enable_raw_mode()?;
        if let Err(error) = execute!(writer, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        Ok(Self {
            writer: Some(writer),
            active: true,
        })
    }

    /// Return the wrapped writer, if the guard has not been consumed by `leave`.
    #[must_use]
    pub const fn writer(&self) -> Option<&W> {
        self.writer.as_ref()
    }

    /// Return the wrapped writer mutably, if the guard has not been consumed by `leave`.
    pub const fn writer_mut(&mut self) -> Option<&mut W> {
        self.writer.as_mut()
    }

    /// Leave alternate screen and raw mode, returning the wrapped writer.
    ///
    /// # Errors
    ///
    /// Returns any error reported by crossterm while leaving the alternate
    /// screen or disabling raw mode.
    pub fn leave(mut self) -> io::Result<W> {
        self.leave_inner()?;
        self.active = false;
        let Some(writer) = self.writer.take() else {
            return Err(io::Error::other("crossterm guard writer already taken"));
        };
        Ok(writer)
    }

    fn leave_inner(&mut self) -> io::Result<()> {
        if let Some(writer) = &mut self.writer {
            execute!(writer, LeaveAlternateScreen)?;
        }
        disable_raw_mode()
    }
}

impl<W: Write> Drop for CrosstermTerminalGuard<W> {
    fn drop(&mut self) {
        if self.active {
            if let Some(writer) = &mut self.writer {
                let _ = execute!(writer, LeaveAlternateScreen);
            }
            let _ = disable_raw_mode();
        }
    }
}

/// Convert a crossterm event into a BMUX TUI event.
#[must_use]
pub fn event_from_crossterm(event: CrosstermEvent) -> Option<Event> {
    match event {
        CrosstermEvent::Key(key) => key_from_crossterm(key).map(Event::Key),
        CrosstermEvent::Mouse(mouse) => Some(Event::Mouse(mouse_from_crossterm(mouse))),
        CrosstermEvent::Resize(width, height) => Some(Event::Resize(Size::new(width, height))),
        CrosstermEvent::FocusGained => Some(Event::Focus(FocusEvent::Gained)),
        CrosstermEvent::FocusLost => Some(Event::Focus(FocusEvent::Lost)),
        CrosstermEvent::Paste(text) => Some(Event::Paste(text)),
    }
}

/// Convert a crossterm key event into a BMUX keyboard stroke.
#[must_use]
pub fn key_from_crossterm(key: CrosstermKeyEvent) -> Option<bmux_keyboard::KeyStroke> {
    let code = match key.code {
        CrosstermKeyCode::Backspace => bmux_keyboard::KeyCode::Backspace,
        CrosstermKeyCode::Enter => bmux_keyboard::KeyCode::Enter,
        CrosstermKeyCode::Left => bmux_keyboard::KeyCode::Left,
        CrosstermKeyCode::Right => bmux_keyboard::KeyCode::Right,
        CrosstermKeyCode::Up => bmux_keyboard::KeyCode::Up,
        CrosstermKeyCode::Down => bmux_keyboard::KeyCode::Down,
        CrosstermKeyCode::Home => bmux_keyboard::KeyCode::Home,
        CrosstermKeyCode::End => bmux_keyboard::KeyCode::End,
        CrosstermKeyCode::PageUp => bmux_keyboard::KeyCode::PageUp,
        CrosstermKeyCode::PageDown => bmux_keyboard::KeyCode::PageDown,
        CrosstermKeyCode::Tab | CrosstermKeyCode::BackTab => bmux_keyboard::KeyCode::Tab,
        CrosstermKeyCode::Delete => bmux_keyboard::KeyCode::Delete,
        CrosstermKeyCode::Insert => bmux_keyboard::KeyCode::Insert,
        CrosstermKeyCode::F(number) => bmux_keyboard::KeyCode::F(number),
        CrosstermKeyCode::Char(' ') => bmux_keyboard::KeyCode::Space,
        CrosstermKeyCode::Char(value) => bmux_keyboard::KeyCode::Char(value),
        CrosstermKeyCode::Esc => bmux_keyboard::KeyCode::Escape,
        CrosstermKeyCode::Null
        | CrosstermKeyCode::CapsLock
        | CrosstermKeyCode::ScrollLock
        | CrosstermKeyCode::NumLock
        | CrosstermKeyCode::PrintScreen
        | CrosstermKeyCode::Pause
        | CrosstermKeyCode::Menu
        | CrosstermKeyCode::KeypadBegin
        | CrosstermKeyCode::Media(_)
        | CrosstermKeyCode::Modifier(_) => return None,
    };
    Some(bmux_keyboard::KeyStroke::with_modifiers(
        code,
        modifiers_from_crossterm(key.modifiers),
    ))
}

/// Convert crossterm mouse event data.
#[must_use]
pub fn mouse_from_crossterm(mouse: CrosstermMouseEvent) -> MouseEvent {
    MouseEvent::new(
        mouse_kind_from_crossterm(mouse.kind),
        Point::new(mouse.column, mouse.row),
    )
    .with_modifiers(mouse_modifiers_from_crossterm(mouse.modifiers))
}

fn modifiers_from_crossterm(modifiers: CrosstermKeyModifiers) -> bmux_keyboard::Modifiers {
    bmux_keyboard::Modifiers {
        ctrl: modifiers.contains(CrosstermKeyModifiers::CONTROL),
        alt: modifiers.contains(CrosstermKeyModifiers::ALT),
        shift: modifiers.contains(CrosstermKeyModifiers::SHIFT),
        super_key: modifiers.contains(CrosstermKeyModifiers::SUPER),
        hyper: modifiers.contains(CrosstermKeyModifiers::HYPER),
        meta: modifiers.contains(CrosstermKeyModifiers::META),
    }
}

const fn mouse_button_from_crossterm(button: CrosstermMouseButton) -> MouseButton {
    match button {
        CrosstermMouseButton::Left => MouseButton::Left,
        CrosstermMouseButton::Right => MouseButton::Right,
        CrosstermMouseButton::Middle => MouseButton::Middle,
    }
}

const fn mouse_kind_from_crossterm(kind: CrosstermMouseEventKind) -> MouseEventKind {
    match kind {
        CrosstermMouseEventKind::Down(button) => {
            MouseEventKind::Down(mouse_button_from_crossterm(button))
        }
        CrosstermMouseEventKind::Up(button) => {
            MouseEventKind::Up(mouse_button_from_crossterm(button))
        }
        CrosstermMouseEventKind::Drag(button) => {
            MouseEventKind::Drag(mouse_button_from_crossterm(button))
        }
        CrosstermMouseEventKind::Moved => MouseEventKind::Move,
        CrosstermMouseEventKind::ScrollUp => MouseEventKind::ScrollUp,
        CrosstermMouseEventKind::ScrollDown => MouseEventKind::ScrollDown,
        CrosstermMouseEventKind::ScrollLeft => MouseEventKind::ScrollLeft,
        CrosstermMouseEventKind::ScrollRight => MouseEventKind::ScrollRight,
    }
}

fn mouse_modifiers_from_crossterm(modifiers: CrosstermKeyModifiers) -> MouseModifiers {
    MouseModifiers {
        shift: modifiers.contains(CrosstermKeyModifiers::SHIFT),
        alt: modifiers.contains(CrosstermKeyModifiers::ALT),
        ctrl: modifiers.contains(CrosstermKeyModifiers::CONTROL),
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyEvent, KeyEventKind, KeyEventState};

    use super::{event_from_crossterm, key_from_crossterm};
    use crate::event::{Event, FocusEvent, MouseButton, MouseEventKind};

    #[test]
    fn crossterm_guard_module_compiles() {
        let _ = core::mem::size_of::<Option<super::CrosstermTerminalGuard<Vec<u8>>>>();
    }

    #[test]
    fn converts_key_events() {
        let key = KeyEvent {
            code: crossterm::event::KeyCode::Char('x'),
            modifiers: crossterm::event::KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };

        let stroke = key_from_crossterm(key).expect("key should convert");

        assert_eq!(stroke.key, bmux_keyboard::KeyCode::Char('x'));
        assert!(stroke.modifiers.ctrl);
    }

    #[test]
    fn converts_resize_focus_and_paste_events() {
        assert_eq!(
            event_from_crossterm(crossterm::event::Event::Resize(80, 24)),
            Some(Event::Resize(crate::geometry::Size::new(80, 24)))
        );
        assert_eq!(
            event_from_crossterm(crossterm::event::Event::FocusGained),
            Some(Event::Focus(FocusEvent::Gained))
        );
        assert_eq!(
            event_from_crossterm(crossterm::event::Event::Paste("hi".to_owned())),
            Some(Event::Paste("hi".to_owned()))
        );
    }

    #[test]
    fn converts_mouse_events() {
        let event = event_from_crossterm(crossterm::event::Event::Mouse(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: 4,
                row: 7,
                modifiers: crossterm::event::KeyModifiers::ALT,
            },
        ));

        let Some(Event::Mouse(mouse)) = event else {
            panic!("expected mouse event");
        };
        assert_eq!(mouse.kind, MouseEventKind::Down(MouseButton::Left));
        assert_eq!(mouse.position, crate::geometry::Point::new(4, 7));
        assert!(mouse.modifiers.alt);
    }
}
