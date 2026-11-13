//! Terminal input events, the input reader, and input mode commands.
//!
//! One process-wide reader serves [`poll`], [`read`], and [`EventStream`]. It
//! is created on first use and reads from stdin when stdin is a terminal,
//! otherwise from `/dev/tty` (Unix), or from the console input buffer
//! (Windows).

use std::io::{self, Write};
use std::time::Duration;

use crate::command::Command;

pub(crate) mod reader;
mod stream;
#[cfg(unix)]
pub(crate) mod unix_parse;
#[cfg(unix)]
mod unix_source;
#[cfg(windows)]
mod windows_source;

pub use stream::EventStream;

/// Wait up to `timeout` for an input event to become available.
///
/// Returns `true` when [`read`] will return without blocking. While another
/// caller is blocked reading, this waits for that reader for at most
/// `timeout` and then reports `false`.
///
/// # Errors
///
/// Returns any error from the terminal input source, including failure to
/// open it.
pub fn poll(timeout: Duration) -> io::Result<bool> {
    reader::poll_event(Some(timeout))
}

/// Block until an input event is available and return it.
///
/// # Errors
///
/// Returns any error from the terminal input source, including failure to
/// open it.
pub fn read() -> io::Result<Event> {
    reader::read_event()
}

/// A terminal input event.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Event {
    /// The terminal gained focus.
    FocusGained,
    /// The terminal lost focus.
    FocusLost,
    /// A key event.
    Key(KeyEvent),
    /// A mouse event.
    Mouse(MouseEvent),
    /// Text pasted with bracketed paste.
    Paste(String),
    /// The terminal was resized to `(columns, rows)`.
    Resize(u16, u16),
}

/// Defines a set of bit flags over `u8`.
macro_rules! flags {
    (
        $(#[$meta:meta])*
        pub struct $name:ident {
            $( $(#[$flag_meta:meta])* const $flag:ident = $value:expr; )*
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
        pub struct $name(u8);

        impl $name {
            $( $(#[$flag_meta])* pub const $flag: Self = Self($value); )*

            /// No flags.
            #[must_use]
            pub const fn empty() -> Self {
                Self(0)
            }

            /// The raw bits.
            #[must_use]
            pub const fn bits(self) -> u8 {
                self.0
            }

            /// Flags from raw bits, keeping only known flags.
            #[must_use]
            pub const fn from_bits_truncate(bits: u8) -> Self {
                Self(bits & (0 $(| $value)*))
            }

            /// Whether no flag is set.
            #[must_use]
            pub const fn is_empty(self) -> bool {
                self.0 == 0
            }

            /// Whether every flag in `other` is set.
            #[must_use]
            pub const fn contains(self, other: Self) -> bool {
                self.0 & other.0 == other.0
            }

            /// Whether any flag in `other` is set.
            #[must_use]
            pub const fn intersects(self, other: Self) -> bool {
                self.0 & other.0 != 0
            }

            /// Set every flag in `other`.
            pub const fn insert(&mut self, other: Self) {
                self.0 |= other.0;
            }

            /// Clear every flag in `other`.
            pub const fn remove(&mut self, other: Self) {
                self.0 &= !other.0;
            }

            /// Set or clear every flag in `other`.
            pub const fn set(&mut self, other: Self, value: bool) {
                if value {
                    self.insert(other);
                } else {
                    self.remove(other);
                }
            }
        }

        impl ::std::ops::BitOr for $name {
            type Output = Self;
            fn bitor(self, other: Self) -> Self {
                Self(self.0 | other.0)
            }
        }

        impl ::std::ops::BitOrAssign for $name {
            fn bitor_assign(&mut self, other: Self) {
                self.0 |= other.0;
            }
        }

        impl ::std::ops::BitAnd for $name {
            type Output = Self;
            fn bitand(self, other: Self) -> Self {
                Self(self.0 & other.0)
            }
        }

        impl ::std::ops::BitAndAssign for $name {
            fn bitand_assign(&mut self, other: Self) {
                self.0 &= other.0;
            }
        }

        impl ::std::ops::Sub for $name {
            type Output = Self;
            fn sub(self, other: Self) -> Self {
                Self(self.0 & !other.0)
            }
        }

        impl ::std::fmt::Debug for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                let mut first = true;
                write!(f, "{}(", stringify!($name))?;
                $(
                    if $value != 0 && self.contains(Self::$flag) {
                        if !first {
                            f.write_str(" | ")?;
                        }
                        first = false;
                        f.write_str(stringify!($flag))?;
                    }
                )*
                if first {
                    f.write_str("empty")?;
                }
                f.write_str(")")
            }
        }
    };
}

flags! {
    /// Modifier keys held during a key or mouse event.
    pub struct KeyModifiers {
        /// Shift.
        const SHIFT = 0b0000_0001;
        /// Control.
        const CONTROL = 0b0000_0010;
        /// Alt (Option on macOS).
        const ALT = 0b0000_0100;
        /// Super (Windows or Command key).
        const SUPER = 0b0000_1000;
        /// Hyper.
        const HYPER = 0b0001_0000;
        /// Meta.
        const META = 0b0010_0000;
        /// No modifiers.
        const NONE = 0b0000_0000;
    }
}

flags! {
    /// Extra key state reported by the kitty keyboard protocol.
    pub struct KeyEventState {
        /// The key is on the keypad.
        const KEYPAD = 0b0000_0001;
        /// Caps Lock was on.
        const CAPS_LOCK = 0b0000_0010;
        /// Num Lock was on.
        const NUM_LOCK = 0b0000_0100;
        /// No extra state.
        const NONE = 0b0000_0000;
    }
}

flags! {
    /// Kitty keyboard protocol progressive enhancement flags.
    pub struct KeyboardEnhancementFlags {
        /// Report ambiguous keys as escape codes.
        const DISAMBIGUATE_ESCAPE_CODES = 0b0000_0001;
        /// Report repeat and release events.
        const REPORT_EVENT_TYPES = 0b0000_0010;
        /// Report shifted and base-layout alternate keys.
        const REPORT_ALTERNATE_KEYS = 0b0000_0100;
        /// Report every key, including text keys, as an escape code.
        const REPORT_ALL_KEYS_AS_ESCAPE_CODES = 0b0000_1000;
    }
}

/// Whether a key was pressed, repeated, or released.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum KeyEventKind {
    /// The key went down.
    Press,
    /// The key is auto-repeating.
    Repeat,
    /// The key went up.
    Release,
}

/// A media key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MediaKeyCode {
    /// Play.
    Play,
    /// Pause.
    Pause,
    /// Play/Pause toggle.
    PlayPause,
    /// Reverse.
    Reverse,
    /// Stop.
    Stop,
    /// Fast forward.
    FastForward,
    /// Rewind.
    Rewind,
    /// Next track.
    TrackNext,
    /// Previous track.
    TrackPrevious,
    /// Record.
    Record,
    /// Lower volume.
    LowerVolume,
    /// Raise volume.
    RaiseVolume,
    /// Mute volume.
    MuteVolume,
}

/// A modifier key reported on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ModifierKeyCode {
    /// Left Shift.
    LeftShift,
    /// Left Control.
    LeftControl,
    /// Left Alt.
    LeftAlt,
    /// Left Super.
    LeftSuper,
    /// Left Hyper.
    LeftHyper,
    /// Left Meta.
    LeftMeta,
    /// Right Shift.
    RightShift,
    /// Right Control.
    RightControl,
    /// Right Alt.
    RightAlt,
    /// Right Super.
    RightSuper,
    /// Right Hyper.
    RightHyper,
    /// Right Meta.
    RightMeta,
    /// ISO level 3 shift (`AltGr`).
    IsoLevel3Shift,
    /// ISO level 5 shift.
    IsoLevel5Shift,
}

/// A key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum KeyCode {
    /// Backspace.
    Backspace,
    /// Enter.
    Enter,
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Home.
    Home,
    /// End.
    End,
    /// Page Up.
    PageUp,
    /// Page Down.
    PageDown,
    /// Tab.
    Tab,
    /// Shift+Tab.
    BackTab,
    /// Delete.
    Delete,
    /// Insert.
    Insert,
    /// A function key.
    F(u8),
    /// A character.
    Char(char),
    /// No key.
    Null,
    /// Escape.
    Esc,
    /// Caps Lock.
    CapsLock,
    /// Scroll Lock.
    ScrollLock,
    /// Num Lock.
    NumLock,
    /// Print Screen.
    PrintScreen,
    /// Pause.
    Pause,
    /// Menu.
    Menu,
    /// The keypad's center key.
    KeypadBegin,
    /// A media key.
    Media(MediaKeyCode),
    /// A modifier key on its own.
    Modifier(ModifierKeyCode),
}

/// A key event.
///
/// Equality and hashing treat an uppercase ASCII character as implying
/// Shift, so `Char('A')` equals `Char('a')` with [`KeyModifiers::SHIFT`].
#[derive(Debug, Clone, Copy, Eq)]
pub struct KeyEvent {
    /// The key.
    pub code: KeyCode,
    /// Modifiers held.
    pub modifiers: KeyModifiers,
    /// Press, repeat, or release.
    pub kind: KeyEventKind,
    /// Extra kitty keyboard protocol state.
    pub state: KeyEventState,
}

impl KeyEvent {
    /// A key press.
    #[must_use]
    pub const fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self::new_with_kind_and_state(code, modifiers, KeyEventKind::Press, KeyEventState::NONE)
    }

    /// A key event of the given kind.
    #[must_use]
    pub const fn new_with_kind(code: KeyCode, modifiers: KeyModifiers, kind: KeyEventKind) -> Self {
        Self::new_with_kind_and_state(code, modifiers, kind, KeyEventState::NONE)
    }

    /// A key event of the given kind and state.
    #[must_use]
    pub const fn new_with_kind_and_state(
        code: KeyCode,
        modifiers: KeyModifiers,
        kind: KeyEventKind,
        state: KeyEventState,
    ) -> Self {
        Self {
            code,
            modifiers,
            kind,
            state,
        }
    }

    /// Whether this is a press.
    #[must_use]
    pub const fn is_press(&self) -> bool {
        matches!(self.kind, KeyEventKind::Press)
    }

    /// Whether this is a release.
    #[must_use]
    pub const fn is_release(&self) -> bool {
        matches!(self.kind, KeyEventKind::Release)
    }

    /// Whether this is a repeat.
    #[must_use]
    pub const fn is_repeat(&self) -> bool {
        matches!(self.kind, KeyEventKind::Repeat)
    }

    /// Shift present exactly when the character is uppercase ASCII.
    const fn case_normalized(mut self) -> Self {
        if let KeyCode::Char(character) = self.code {
            if character.is_ascii_uppercase() {
                self.modifiers.insert(KeyModifiers::SHIFT);
            } else if self.modifiers.contains(KeyModifiers::SHIFT) {
                self.code = KeyCode::Char(character.to_ascii_uppercase());
            }
        }
        self
    }
}

impl From<KeyCode> for KeyEvent {
    fn from(code: KeyCode) -> Self {
        Self::new(code, KeyModifiers::NONE)
    }
}

impl PartialEq for KeyEvent {
    fn eq(&self, other: &Self) -> bool {
        let left = self.case_normalized();
        let right = other.case_normalized();
        left.code == right.code
            && left.modifiers == right.modifiers
            && left.kind == right.kind
            && left.state == right.state
    }
}

impl std::hash::Hash for KeyEvent {
    fn hash<H: std::hash::Hasher>(&self, hasher: &mut H) {
        let normalized = self.case_normalized();
        normalized.code.hash(hasher);
        normalized.modifiers.hash(hasher);
        normalized.kind.hash(hasher);
        normalized.state.hash(hasher);
    }
}

/// A mouse event at a zero-based cell position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MouseEvent {
    /// What happened.
    pub kind: MouseEventKind,
    /// Zero-based column.
    pub column: u16,
    /// Zero-based row.
    pub row: u16,
    /// Modifiers held.
    pub modifiers: KeyModifiers,
}

/// What a mouse event reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseEventKind {
    /// A button went down.
    Down(MouseButton),
    /// A button went up.
    Up(MouseButton),
    /// The pointer moved with a button held.
    Drag(MouseButton),
    /// The pointer moved with no button held.
    Moved,
    /// The wheel scrolled down.
    ScrollDown,
    /// The wheel scrolled up.
    ScrollUp,
    /// The wheel scrolled left.
    ScrollLeft,
    /// The wheel scrolled right.
    ScrollRight,
}

/// A mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseButton {
    /// Left button.
    Left,
    /// Right button.
    Right,
    /// Middle button.
    Middle,
}

/// Report mouse button, drag, and motion events (SGR encoding preferred).
///
/// On Windows this sets the console input mode to report mouse and window
/// events, which also turns off quick-edit selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnableMouseCapture;

impl Command for EnableMouseCapture {
    fn write_ansi(&self, out: &mut (impl Write + ?Sized)) -> io::Result<()> {
        // Normal, button-motion, and any-motion tracking; then the RXVT and
        // SGR coordinate encodings, the last one supported taking effect.
        out.write_all(b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1015h\x1b[?1006h")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        crate::sys::windows::enable_mouse_capture()
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        false
    }
}

/// Stop reporting mouse events.
///
/// On Windows this restores the console input mode saved by the first
/// [`EnableMouseCapture`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisableMouseCapture;

impl Command for DisableMouseCapture {
    fn write_ansi(&self, out: &mut (impl Write + ?Sized)) -> io::Result<()> {
        out.write_all(b"\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        crate::sys::windows::disable_mouse_capture()
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        false
    }
}

/// Report focus changes (`CSI ?1004h`).
///
/// Windows consoles always report focus changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnableFocusChange;

impl Command for EnableFocusChange {
    fn write_ansi(&self, out: &mut (impl Write + ?Sized)) -> io::Result<()> {
        out.write_all(b"\x1b[?1004h")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Ok(())
    }
}

/// Stop reporting focus changes (`CSI ?1004l`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisableFocusChange;

impl Command for DisableFocusChange {
    fn write_ansi(&self, out: &mut (impl Write + ?Sized)) -> io::Result<()> {
        out.write_all(b"\x1b[?1004l")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Ok(())
    }
}

/// Deliver pastes as [`Event::Paste`] (`CSI ?2004h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnableBracketedPaste;

impl Command for EnableBracketedPaste {
    fn write_ansi(&self, out: &mut (impl Write + ?Sized)) -> io::Result<()> {
        out.write_all(b"\x1b[?2004h")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "bracketed paste requires virtual terminal processing",
        ))
    }
}

/// Stop bracketed paste (`CSI ?2004l`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisableBracketedPaste;

impl Command for DisableBracketedPaste {
    fn write_ansi(&self, out: &mut (impl Write + ?Sized)) -> io::Result<()> {
        out.write_all(b"\x1b[?2004l")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Ok(())
    }
}

/// Push kitty keyboard protocol flags (`CSI > flags u`).
///
/// Not available on Windows consoles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PushKeyboardEnhancementFlags(pub KeyboardEnhancementFlags);

impl Command for PushKeyboardEnhancementFlags {
    fn write_ansi(&self, out: &mut (impl Write + ?Sized)) -> io::Result<()> {
        out.write_all(b"\x1b[>")?;
        crate::command::write_u16(out, u16::from(self.0.bits()))?;
        out.write_all(b"u")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "keyboard enhancement is not available on Windows consoles",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        false
    }
}

/// Pop one level of kitty keyboard protocol flags (`CSI < 1 u`).
///
/// Not available on Windows consoles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PopKeyboardEnhancementFlags;

impl Command for PopKeyboardEnhancementFlags {
    fn write_ansi(&self, out: &mut (impl Write + ?Sized)) -> io::Result<()> {
        out.write_all(b"\x1b[<1u")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "keyboard enhancement is not available on Windows consoles",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue;

    #[test]
    fn mode_commands_write_expected_sequences() {
        let mut out = Vec::new();
        queue!(
            out,
            EnableFocusChange,
            DisableFocusChange,
            EnableBracketedPaste,
            DisableBracketedPaste,
        )
        .unwrap();
        assert_eq!(
            out,
            b"\x1b[?1004h\x1b[?1004l\x1b[?2004h\x1b[?2004l".as_slice()
        );
    }

    #[cfg(unix)]
    #[test]
    fn mouse_and_keyboard_commands_write_expected_sequences() {
        let mut out = Vec::new();
        queue!(
            out,
            EnableMouseCapture,
            DisableMouseCapture,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
            ),
            PopKeyboardEnhancementFlags,
        )
        .unwrap();
        assert_eq!(
            out,
            b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1015h\x1b[?1006h\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[>7u\x1b[<1u"
                .as_slice()
        );
    }

    #[test]
    fn key_event_equality_normalizes_ascii_case() {
        assert_eq!(
            KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::SHIFT)
        );
        assert_ne!(
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::SHIFT)
        );
        let hash = |event: KeyEvent| {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            event.hash(&mut hasher);
            hasher.finish()
        };
        assert_eq!(
            hash(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE)),
            hash(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::SHIFT))
        );
    }

    #[test]
    fn flags_support_set_operations() {
        let mut modifiers = KeyModifiers::CONTROL | KeyModifiers::ALT;
        assert!(modifiers.contains(KeyModifiers::ALT));
        modifiers.set(KeyModifiers::ALT, false);
        assert_eq!(modifiers, KeyModifiers::CONTROL);
        assert!(KeyModifiers::empty().is_empty());
        assert_eq!(format!("{modifiers:?}"), "KeyModifiers(CONTROL)");
    }
}
