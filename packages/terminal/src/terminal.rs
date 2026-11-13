//! Terminal modes, size, screen commands, and capability probes.

use std::io::{self, Write};

use crate::command::Command;
use crate::sys;

/// Put the controlling terminal into raw mode.
///
/// On Unix the terminal attributes in effect before the first call are saved
/// and restored by [`disable_raw_mode`]; calling this again while raw mode is
/// enabled does nothing. On Windows line input, echo, and processed input are
/// turned off on the console input buffer.
///
/// # Errors
///
/// Returns any error from opening the terminal or changing its attributes.
pub fn enable_raw_mode() -> io::Result<()> {
    sys::enable_raw_mode()
}

/// Restore the terminal attributes saved by [`enable_raw_mode`].
///
/// Does nothing on Unix when raw mode is not enabled.
///
/// # Errors
///
/// Returns any error from opening the terminal or changing its attributes.
pub fn disable_raw_mode() -> io::Result<()> {
    sys::disable_raw_mode()
}

/// Whether raw mode is currently enabled.
///
/// # Errors
///
/// On Windows, returns any error reading the console input mode.
pub fn is_raw_mode_enabled() -> io::Result<bool> {
    sys::is_raw_mode_enabled()
}

/// The terminal size as `(columns, rows)`.
///
/// # Errors
///
/// Returns an error when no terminal size can be determined.
pub fn size() -> io::Result<(u16, u16)> {
    sys::size()
}

/// Whether the terminal supports the kitty keyboard protocol.
///
/// On Unix this writes a progressive-enhancement query followed by a
/// primary device attributes query and waits up to two seconds for the
/// replies; terminals that answer only the second query do not support the
/// protocol. Input that arrives meanwhile stays queued for
/// [`crate::event::read`]. Windows consoles always report `false`.
///
/// # Errors
///
/// Returns an error when the query cannot be written or neither reply arrives
/// within two seconds.
pub fn supports_keyboard_enhancement() -> io::Result<bool> {
    sys::supports_keyboard_enhancement()
}

/// Switch to the alternate screen buffer (`CSI ?1049h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnterAlternateScreen;

impl Command for EnterAlternateScreen {
    fn write_ansi(&self, out: &mut (impl Write + ?Sized)) -> io::Result<()> {
        out.write_all(b"\x1b[?1049h")
    }
}

/// Switch back to the main screen buffer (`CSI ?1049l`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaveAlternateScreen;

impl Command for LeaveAlternateScreen {
    fn write_ansi(&self, out: &mut (impl Write + ?Sized)) -> io::Result<()> {
        out.write_all(b"\x1b[?1049l")
    }
}

/// Which part of the screen [`Clear`] erases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClearType {
    /// The whole visible screen (`CSI 2J`).
    All,
    /// The whole screen and its scrollback (`CSI 3J`).
    Purge,
    /// From the cursor to the end of the screen (`CSI J`).
    FromCursorDown,
    /// From the start of the screen to the cursor (`CSI 1J`).
    FromCursorUp,
    /// The cursor's line (`CSI 2K`).
    CurrentLine,
    /// From the cursor to the end of its line (`CSI K`).
    UntilNewLine,
}

/// Erase part of the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clear(pub ClearType);

impl Command for Clear {
    fn write_ansi(&self, out: &mut (impl Write + ?Sized)) -> io::Result<()> {
        out.write_all(match self.0 {
            ClearType::All => b"\x1b[2J",
            ClearType::Purge => b"\x1b[3J",
            ClearType::FromCursorDown => b"\x1b[J",
            ClearType::FromCursorUp => b"\x1b[1J",
            ClearType::CurrentLine => b"\x1b[2K",
            ClearType::UntilNewLine => b"\x1b[K",
        })
    }
}

/// Begin a synchronized update (`CSI ?2026h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BeginSynchronizedUpdate;

impl Command for BeginSynchronizedUpdate {
    fn write_ansi(&self, out: &mut (impl Write + ?Sized)) -> io::Result<()> {
        out.write_all(b"\x1b[?2026h")
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        // Terminals that do not know the mode ignore it.
        true
    }
}

/// End a synchronized update (`CSI ?2026l`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndSynchronizedUpdate;

impl Command for EndSynchronizedUpdate {
    fn write_ansi(&self, out: &mut (impl Write + ?Sized)) -> io::Result<()> {
        out.write_all(b"\x1b[?2026l")
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        // Terminals that do not know the mode ignore it.
        true
    }
}

/// Stop wrapping at the right margin (`CSI ?7l`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisableLineWrap;

impl Command for DisableLineWrap {
    fn write_ansi(&self, out: &mut (impl Write + ?Sized)) -> io::Result<()> {
        out.write_all(b"\x1b[?7l")
    }
}

/// Wrap at the right margin (`CSI ?7h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnableLineWrap;

impl Command for EnableLineWrap {
    fn write_ansi(&self, out: &mut (impl Write + ?Sized)) -> io::Result<()> {
        out.write_all(b"\x1b[?7h")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue;

    #[test]
    fn screen_commands_write_expected_sequences() {
        let mut out = Vec::new();
        queue!(
            out,
            EnterAlternateScreen,
            LeaveAlternateScreen,
            Clear(ClearType::All),
            Clear(ClearType::Purge),
            Clear(ClearType::FromCursorDown),
            Clear(ClearType::FromCursorUp),
            Clear(ClearType::CurrentLine),
            Clear(ClearType::UntilNewLine),
            BeginSynchronizedUpdate,
            EndSynchronizedUpdate,
            DisableLineWrap,
            EnableLineWrap,
        )
        .unwrap();
        assert_eq!(
            out,
            b"\x1b[?1049h\x1b[?1049l\x1b[2J\x1b[3J\x1b[J\x1b[1J\x1b[2K\x1b[K\x1b[?2026h\x1b[?2026l\x1b[?7l\x1b[?7h"
                .as_slice()
        );
    }
}
