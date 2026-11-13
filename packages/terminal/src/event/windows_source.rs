//! Windows console input source.

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Console::{
    CAPSLOCK_ON, DOUBLE_CLICK, FOCUS_EVENT, FROM_LEFT_1ST_BUTTON_PRESSED,
    FROM_LEFT_2ND_BUTTON_PRESSED, FROM_LEFT_3RD_BUTTON_PRESSED, FROM_LEFT_4TH_BUTTON_PRESSED,
    GetNumberOfConsoleInputEvents, INPUT_RECORD, KEY_EVENT, KEY_EVENT_RECORD, LEFT_ALT_PRESSED,
    LEFT_CTRL_PRESSED, MOUSE_EVENT, MOUSE_EVENT_RECORD, MOUSE_HWHEELED, MOUSE_MOVED,
    MOUSE_WHEELED, RIGHT_ALT_PRESSED, RIGHT_CTRL_PRESSED, RIGHTMOST_BUTTON_PRESSED,
    ReadConsoleInputW, SHIFT_PRESSED, WINDOW_BUFFER_SIZE_EVENT,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, INFINITE, ResetEvent, SetEvent, WaitForMultipleObjects,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyboardLayout, ToUnicodeEx, VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE,
    VK_F1, VK_F24, VK_HOME, VK_INSERT, VK_LEFT, VK_MENU, VK_NEXT, VK_NUMPAD0, VK_NUMPAD9,
    VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_TAB, VK_UP,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use super::reader::{InternalEvent, Source, Waker};
use super::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind};
use crate::sys::windows::{ConsoleHandle, window_dimensions};

/// An owned manual-reset event used to interrupt waits.
struct WakeEvent(HANDLE);

// SAFETY: event handles may be signaled and waited on from any thread.
unsafe impl Send for WakeEvent {}
// SAFETY: as above.
unsafe impl Sync for WakeEvent {}

impl WakeEvent {
    fn new() -> io::Result<Self> {
        // SAFETY: default security, manual reset, initially unsignaled.
        let handle = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(handle))
    }
}

impl Drop for WakeEvent {
    fn drop(&mut self) {
        // SAFETY: the handle is owned and closed exactly once.
        unsafe { CloseHandle(self.0) };
    }
}

// SAFETY: console handles may be used from any thread.
unsafe impl Send for ConsoleHandle {}

/// Mouse buttons held as of the previous mouse record.
#[derive(Default, Clone, Copy)]
struct ButtonsHeld {
    left: bool,
    right: bool,
    middle: bool,
}

/// Reads console input records.
pub struct WindowsSource {
    input: ConsoleHandle,
    output: Option<ConsoleHandle>,
    wake: Arc<WakeEvent>,
    surrogate: Option<u16>,
    buttons: ButtonsHeld,
}

impl WindowsSource {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            input: ConsoleHandle::input()?,
            output: ConsoleHandle::output().ok(),
            wake: Arc::new(WakeEvent::new()?),
            surrogate: None,
            buttons: ButtonsHeld::default(),
        })
    }

    /// Wait for input or a wake; `Ok(true)` when input is ready.
    fn wait(&self, deadline: Option<Instant>) -> io::Result<Option<bool>> {
        let millis = deadline.map_or(INFINITE, |deadline| {
            let remaining = deadline.saturating_duration_since(Instant::now());
            u32::try_from(remaining.as_millis()).unwrap_or(INFINITE - 1)
        });
        let handles = [self.input.raw(), self.wake.0];
        // SAFETY: both handles stay open for the call.
        let result = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, millis) };
        match result {
            WAIT_OBJECT_0 => Ok(Some(true)),
            value if value == WAIT_OBJECT_0 + 1 => {
                // SAFETY: owned event handle.
                unsafe { ResetEvent(self.wake.0) };
                Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "terminal input wait was woken",
                ))
            }
            WAIT_TIMEOUT => Ok(None),
            WAIT_FAILED => Err(io::Error::last_os_error()),
            _ => Err(io::Error::other("unexpected console wait result")),
        }
    }

    fn pending_records(&self) -> io::Result<u32> {
        let mut count = 0;
        // SAFETY: valid console handle and output pointer.
        if unsafe { GetNumberOfConsoleInputEvents(self.input.raw(), &raw mut count) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(count)
    }

    fn read_record(&self) -> io::Result<INPUT_RECORD> {
        // SAFETY: plain data, fully written on success.
        let mut record: INPUT_RECORD = unsafe { std::mem::zeroed() };
        let mut read = 0;
        // SAFETY: valid handle, one-record buffer, and output pointer.
        if unsafe { ReadConsoleInputW(self.input.raw(), &raw mut record, 1, &raw mut read) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(record)
    }

    fn translate(&mut self, record: &INPUT_RECORD) -> io::Result<Option<Event>> {
        // The event type selects which union member is initialized.
        match u32::from(record.EventType) {
            KEY_EVENT => {
                // SAFETY: KEY_EVENT records carry a KEY_EVENT_RECORD.
                let key = unsafe { record.Event.KeyEvent };
                Ok(translate_key(&mut self.surrogate, &key))
            }
            MOUSE_EVENT => {
                // SAFETY: MOUSE_EVENT records carry a MOUSE_EVENT_RECORD.
                let mouse = unsafe { record.Event.MouseEvent };
                let event = self.translate_mouse(&mouse);
                self.buttons = ButtonsHeld {
                    left: mouse.dwButtonState & FROM_LEFT_1ST_BUTTON_PRESSED != 0,
                    right: right_button(mouse.dwButtonState),
                    middle: mouse.dwButtonState & FROM_LEFT_2ND_BUTTON_PRESSED != 0,
                };
                Ok(event.map(Event::Mouse))
            }
            WINDOW_BUFFER_SIZE_EVENT => self.resize().map(Some),
            FOCUS_EVENT => {
                // SAFETY: FOCUS_EVENT records carry a FOCUS_EVENT_RECORD.
                let focus = unsafe { record.Event.FocusEvent };
                Ok(Some(if focus.bSetFocus != 0 {
                    Event::FocusGained
                } else {
                    Event::FocusLost
                }))
            }
            _ => Ok(None),
        }
    }

    /// The visible window size after a screen buffer change.
    fn resize(&self) -> io::Result<Event> {
        let info = match &self.output {
            Some(output) => output.screen_buffer_info()?,
            None => ConsoleHandle::output()?.screen_buffer_info()?,
        };
        let (columns, rows) = window_dimensions(&info);
        Ok(Event::Resize(columns, rows))
    }

    fn translate_mouse(&self, record: &MOUSE_EVENT_RECORD) -> Option<MouseEvent> {
        let modifiers = modifiers(record.dwControlKeyState);
        // Positions are buffer coordinates; rows are made window-relative.
        let window_top = self
            .output
            .as_ref()
            .and_then(|output| output.screen_buffer_info().ok())
            .map_or(0, |info| info.srWindow.Top);
        // Console coordinates are non-negative within the buffer.
        #[allow(clippy::cast_sign_loss)]
        let column = record.dwMousePosition.X as u16;
        #[allow(clippy::cast_sign_loss)]
        let row = record.dwMousePosition.Y.wrapping_sub(window_top) as u16;
        let state = record.dwButtonState;
        // The wheel direction is the sign of the high word.
        #[allow(clippy::cast_possible_wrap)]
        let signed_state = state as i32;
        let left = state & FROM_LEFT_1ST_BUTTON_PRESSED != 0;
        let right = right_button(state);
        let middle = state & FROM_LEFT_2ND_BUTTON_PRESSED != 0;
        let held = self.buttons;
        let kind = match record.dwEventFlags {
            0 | DOUBLE_CLICK => {
                if left && !held.left {
                    MouseEventKind::Down(MouseButton::Left)
                } else if !left && held.left {
                    MouseEventKind::Up(MouseButton::Left)
                } else if right && !held.right {
                    MouseEventKind::Down(MouseButton::Right)
                } else if !right && held.right {
                    MouseEventKind::Up(MouseButton::Right)
                } else if middle && !held.middle {
                    MouseEventKind::Down(MouseButton::Middle)
                } else if !middle && held.middle {
                    MouseEventKind::Up(MouseButton::Middle)
                } else {
                    return None;
                }
            }
            MOUSE_MOVED => {
                if state == 0 {
                    MouseEventKind::Moved
                } else if right {
                    MouseEventKind::Drag(MouseButton::Right)
                } else if middle {
                    MouseEventKind::Drag(MouseButton::Middle)
                } else {
                    MouseEventKind::Drag(MouseButton::Left)
                }
            }
            MOUSE_WHEELED => match signed_state.signum() {
                -1 => MouseEventKind::ScrollDown,
                1 => MouseEventKind::ScrollUp,
                _ => return None,
            },
            MOUSE_HWHEELED => match signed_state.signum() {
                -1 => MouseEventKind::ScrollLeft,
                1 => MouseEventKind::ScrollRight,
                _ => return None,
            },
            _ => return None,
        };
        Some(MouseEvent {
            kind,
            column,
            row,
            modifiers,
        })
    }
}

/// Translate a key record, pairing UTF-16 surrogates across records.
fn translate_key(surrogate: &mut Option<u16>, record: &KEY_EVENT_RECORD) -> Option<Event> {
    match key_record(record)? {
        DecodedKey::Key(event) => {
            *surrogate = None;
            Some(Event::Key(event))
        }
        DecodedKey::Surrogate(unit) => {
            let Some(high) = surrogate.take() else {
                *surrogate = Some(unit);
                return None;
            };
            let character = char::decode_utf16([high, unit]).next()?.ok()?;
            // Consoles split a pair across a key-down and a key-up record, so
            // the completed character is always a press.
            Some(Event::Key(KeyEvent::new(
                KeyCode::Char(character),
                modifiers(record.dwControlKeyState),
            )))
        }
    }
}

const fn right_button(state: u32) -> bool {
    state & (RIGHTMOST_BUTTON_PRESSED | FROM_LEFT_3RD_BUTTON_PRESSED | FROM_LEFT_4TH_BUTTON_PRESSED)
        != 0
}

const fn modifiers(state: u32) -> KeyModifiers {
    let mut modifiers = KeyModifiers::NONE;
    if state & SHIFT_PRESSED != 0 {
        modifiers.insert(KeyModifiers::SHIFT);
    }
    if state & (LEFT_CTRL_PRESSED | RIGHT_CTRL_PRESSED) != 0 {
        modifiers.insert(KeyModifiers::CONTROL);
    }
    if state & (LEFT_ALT_PRESSED | RIGHT_ALT_PRESSED) != 0 {
        modifiers.insert(KeyModifiers::ALT);
    }
    modifiers
}

const fn kind(record: &KEY_EVENT_RECORD) -> KeyEventKind {
    if record.bKeyDown != 0 {
        KeyEventKind::Press
    } else {
        KeyEventKind::Release
    }
}

enum DecodedKey {
    Key(KeyEvent),
    Surrogate(u16),
}

/// The character a key produces with no modifiers held, in the foreground
/// window's keyboard layout, cased for Shift and Caps Lock.
fn layout_character(record: &KEY_EVENT_RECORD) -> Option<char> {
    // Leave the kernel's keyboard state (dead keys) untouched.
    const KEEP_KERNEL_STATE: u32 = 0x4;
    let key_state = [0u8; 256];
    let mut produced = [0u16; 2];
    // SAFETY: querying the foreground thread's layout has no preconditions.
    let layout = unsafe {
        let window = GetForegroundWindow();
        let thread = GetWindowThreadProcessId(window, std::ptr::null_mut());
        GetKeyboardLayout(thread)
    };
    // SAFETY: valid key state table and output buffer of the stated length.
    let written = unsafe {
        ToUnicodeEx(
            u32::from(record.wVirtualKeyCode),
            u32::from(record.wVirtualScanCode),
            key_state.as_ptr(),
            produced.as_mut_ptr(),
            2,
            KEEP_KERNEL_STATE,
            layout,
        )
    };
    let written = usize::try_from(written).ok().filter(|count| *count > 0)?;
    let mut characters = char::decode_utf16(produced.into_iter().take(written));
    let character = characters.next()?.ok()?;
    if characters.next().is_some() {
        return None;
    }
    let shift = record.dwControlKeyState & SHIFT_PRESSED != 0;
    let caps_lock = record.dwControlKeyState & CAPSLOCK_ON != 0;
    Some(if shift ^ caps_lock {
        single_case(character.to_uppercase()).unwrap_or(character)
    } else {
        single_case(character.to_lowercase()).unwrap_or(character)
    })
}

/// The one character of a case mapping, if it maps to exactly one.
fn single_case(mut mapping: impl Iterator<Item = char>) -> Option<char> {
    let character = mapping.next()?;
    mapping.next().is_none().then_some(character)
}

fn key_record(record: &KEY_EVENT_RECORD) -> Option<DecodedKey> {
    let modifiers = modifiers(record.dwControlKeyState);
    let virtual_key = record.wVirtualKeyCode;
    // SAFETY: input records are read in their wide-character form.
    let unit = unsafe { record.uChar.UnicodeChar };
    let kind = kind(record);

    // Alt+numpad character codes arrive as the Alt key's release.
    if virtual_key == VK_MENU && record.bKeyDown == 0 && unit != 0 {
        return Some(match unit {
            0xd800..=0xdfff => DecodedKey::Surrogate(unit),
            unit => DecodedKey::Key(KeyEvent::new_with_kind(
                KeyCode::Char(char::from_u32(u32::from(unit))?),
                modifiers,
                kind,
            )),
        });
    }

    // Numpad digits typed with Alt alone compose a character code.
    let numpad_digit = (VK_NUMPAD0..=VK_NUMPAD9).contains(&virtual_key);
    let alt_only = modifiers.contains(KeyModifiers::ALT)
        && !modifiers.contains(KeyModifiers::SHIFT | KeyModifiers::CONTROL);
    if alt_only && numpad_digit {
        return None;
    }

    let code = match virtual_key {
        VK_SHIFT | VK_CONTROL | VK_MENU => return None,
        VK_BACK => KeyCode::Backspace,
        VK_ESCAPE => KeyCode::Esc,
        VK_RETURN => KeyCode::Enter,
        VK_F1..=VK_F24 => {
            // F1 is 0x70; the offset fits in a byte.
            #[allow(clippy::cast_possible_truncation)]
            let number = (virtual_key - VK_F1 + 1) as u8;
            KeyCode::F(number)
        }
        VK_LEFT => KeyCode::Left,
        VK_UP => KeyCode::Up,
        VK_RIGHT => KeyCode::Right,
        VK_DOWN => KeyCode::Down,
        VK_PRIOR => KeyCode::PageUp,
        VK_NEXT => KeyCode::PageDown,
        VK_HOME => KeyCode::Home,
        VK_END => KeyCode::End,
        VK_DELETE => KeyCode::Delete,
        VK_INSERT => KeyCode::Insert,
        VK_TAB if modifiers.contains(KeyModifiers::SHIFT) => KeyCode::BackTab,
        VK_TAB => KeyCode::Tab,
        _ => match unit {
            // Control characters: report the key's layout character instead.
            0x00..=0x1f => KeyCode::Char(layout_character(record)?),
            0xd800..=0xdfff => return Some(DecodedKey::Surrogate(unit)),
            unit => KeyCode::Char(char::from_u32(u32::from(unit))?),
        },
    };
    Some(DecodedKey::Key(KeyEvent::new_with_kind(code, modifiers, kind)))
}

impl Source for WindowsSource {
    fn try_read(&mut self, timeout: Option<Duration>) -> io::Result<Option<InternalEvent>> {
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        loop {
            if self.wait(deadline)? == Some(true) && self.pending_records()? != 0 {
                let record = self.read_record()?;
                if let Some(event) = self.translate(&record)? {
                    return Ok(Some(InternalEvent::Event(event)));
                }
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok(None);
            }
        }
    }

    fn waker(&self) -> Waker {
        let wake = Arc::clone(&self.wake);
        Waker::new(move || {
            // SAFETY: owned event handle kept alive by the Arc.
            unsafe { SetEvent(wake.0) };
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Console::KEY_EVENT_RECORD_0;

    fn record(virtual_key: u16, unit: u16, down: bool, state: u32) -> KEY_EVENT_RECORD {
        KEY_EVENT_RECORD {
            bKeyDown: i32::from(down),
            wRepeatCount: 1,
            wVirtualKeyCode: virtual_key,
            wVirtualScanCode: 0,
            uChar: KEY_EVENT_RECORD_0 { UnicodeChar: unit },
            dwControlKeyState: state,
        }
    }

    fn decoded(record: &KEY_EVENT_RECORD) -> Option<KeyEvent> {
        match key_record(record)? {
            DecodedKey::Key(event) => Some(event),
            DecodedKey::Surrogate(_) => None,
        }
    }

    #[test]
    fn named_keys_and_characters() {
        assert_eq!(
            decoded(&record(VK_RETURN, 13, true, 0)),
            Some(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        );
        assert_eq!(
            decoded(&record(VK_F1 + 11, 0, true, 0)),
            Some(KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE))
        );
        assert_eq!(
            decoded(&record(VK_TAB, 9, true, SHIFT_PRESSED)),
            Some(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT))
        );
        assert_eq!(
            decoded(&record(0x41, u16::from(b'a'), false, 0)),
            Some(KeyEvent::new_with_kind(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
                KeyEventKind::Release
            ))
        );
        assert_eq!(
            decoded(&record(0x41, u16::from(b'A'), true, SHIFT_PRESSED | LEFT_ALT_PRESSED)),
            Some(KeyEvent::new(
                KeyCode::Char('A'),
                KeyModifiers::SHIFT | KeyModifiers::ALT
            ))
        );
        assert!(decoded(&record(VK_SHIFT, 0, true, SHIFT_PRESSED)).is_none());
        assert!(decoded(&record(VK_NUMPAD0 + 5, 0, true, LEFT_ALT_PRESSED)).is_none());
    }

    #[test]
    fn surrogate_pairs_combine() {
        let mut surrogate = None;
        assert!(translate_key(&mut surrogate, &record(0xe7, 0xd83d, true, 0)).is_none());
        assert_eq!(
            translate_key(&mut surrogate, &record(0xe7, 0xde00, true, 0)),
            Some(Event::Key(KeyEvent::new(KeyCode::Char('😀'), KeyModifiers::NONE)))
        );
        // A pair split across key-down and key-up records is one press.
        assert!(translate_key(&mut surrogate, &record(0xe7, 0xd83d, true, 0)).is_none());
        assert_eq!(
            translate_key(&mut surrogate, &record(0xe7, 0xde00, false, 0)),
            Some(Event::Key(KeyEvent::new(KeyCode::Char('😀'), KeyModifiers::NONE)))
        );
    }
}
