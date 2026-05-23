//! Crossterm terminal lifecycle and event adapter.

use std::io::{self, Write};
use std::time::Duration;

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as CrosstermEvent, KeyEvent as CrosstermKeyEvent, KeyModifiers as CrosstermKeyModifiers,
    MouseButton as CrosstermMouseButton, MouseEvent as CrosstermMouseEvent,
    MouseEventKind as CrosstermMouseEventKind, poll as crossterm_poll, read as crossterm_read,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{execute, queue};

use crate::event::{Event, FocusEvent, MouseButton, MouseEvent, MouseEventKind, MouseModifiers};
use crate::geometry::{Point, Size};
use bmux_keyboard::crossterm::{crossterm_key_event_is_release, crossterm_key_event_to_stroke};

/// RAII guard for crossterm raw mode and alternate-screen lifecycle.
///
/// The guard enables raw mode and enters the alternate screen on creation. On
/// drop it attempts to leave the alternate screen and disable raw mode.
pub struct CrosstermTerminalGuard<W: Write> {
    writer: Option<W>,
    active: bool,
    keyboard_enhanced: bool,
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
        if let Err(error) = execute!(
            writer,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        ) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        let keyboard_enhanced = push_keyboard_enhancement_flags(&mut writer).is_ok();
        Ok(Self {
            writer: Some(writer),
            active: true,
            keyboard_enhanced,
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
            if self.keyboard_enhanced {
                pop_keyboard_enhancement_flags(writer)?;
                self.keyboard_enhanced = false;
            }
            execute!(
                writer,
                DisableBracketedPaste,
                DisableMouseCapture,
                LeaveAlternateScreen
            )?;
        }
        disable_raw_mode()
    }
}

impl<W: Write> Drop for CrosstermTerminalGuard<W> {
    fn drop(&mut self) {
        if self.active {
            if let Some(writer) = &mut self.writer {
                if self.keyboard_enhanced {
                    let _ = pop_keyboard_enhancement_flags(writer);
                    self.keyboard_enhanced = false;
                }
                let _ = execute!(
                    writer,
                    DisableBracketedPaste,
                    DisableMouseCapture,
                    LeaveAlternateScreen
                );
            }
            let _ = disable_raw_mode();
        }
    }
}

fn push_keyboard_enhancement_flags<W: Write>(writer: &mut W) -> io::Result<()> {
    use crossterm::event::{KeyboardEnhancementFlags, PushKeyboardEnhancementFlags};

    queue!(
        writer,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
        )
    )?;
    writer.flush()
}

fn pop_keyboard_enhancement_flags<W: Write>(writer: &mut W) -> io::Result<()> {
    queue!(writer, crossterm::event::PopKeyboardEnhancementFlags)?;
    writer.flush()
}

/// Poll for a terminal event and convert it to a BMUX TUI event.
///
/// Returns `Ok(None)` when no terminal event is ready before `timeout`, or
/// when crossterm reports an event kind that has no BMUX TUI representation.
///
/// # Errors
///
/// Returns any error reported by crossterm while polling or reading events.
pub fn poll_event(timeout: Duration) -> io::Result<Option<Event>> {
    if !crossterm_poll(timeout)? {
        return Ok(None);
    }
    read_event()
}

/// Read one terminal event and convert it to a BMUX TUI event.
///
/// # Errors
///
/// Returns any error reported by crossterm while reading events.
pub fn read_event() -> io::Result<Option<Event>> {
    Ok(event_from_crossterm(crossterm_read()?))
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
    if crossterm_key_event_is_release(&key) {
        return None;
    }

    crossterm_key_event_to_stroke(&key)
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

    use super::{event_from_crossterm, key_from_crossterm, poll_event, read_event};
    use crate::event::{Event, FocusEvent, MouseButton, MouseEventKind};

    #[test]
    fn crossterm_guard_module_compiles() {
        let _ = core::mem::size_of::<Option<super::CrosstermTerminalGuard<Vec<u8>>>>();
        let _ = poll_event as fn(std::time::Duration) -> std::io::Result<Option<Event>>;
        let _ = read_event as fn() -> std::io::Result<Option<Event>>;
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
    fn ignores_key_release_events() {
        let key = KeyEvent {
            code: crossterm::event::KeyCode::Char('x'),
            modifiers: crossterm::event::KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        };

        assert_eq!(key_from_crossterm(key), None);
    }

    #[test]
    fn preserves_super_and_meta_char_modifiers() {
        let key = KeyEvent {
            code: crossterm::event::KeyCode::Char('c'),
            modifiers: crossterm::event::KeyModifiers::SUPER | crossterm::event::KeyModifiers::META,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };

        let stroke = key_from_crossterm(key).expect("key should convert");

        assert_eq!(stroke.key, bmux_keyboard::KeyCode::Char('c'));
        assert!(stroke.modifiers.super_key);
        assert!(stroke.modifiers.meta);
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
