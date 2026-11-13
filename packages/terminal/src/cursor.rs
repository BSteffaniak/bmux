//! Cursor commands.

use std::io::{self, Write};

use crate::command::{Command, write_u16};

/// Move the cursor to a zero-based `(column, row)` position (`CSI row;col H`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoveTo(pub u16, pub u16);

impl Command for MoveTo {
    #[inline]
    fn write_ansi(&self, out: &mut (impl Write + ?Sized)) -> io::Result<()> {
        out.write_all(b"\x1b[")?;
        write_u16(out, self.1.saturating_add(1))?;
        out.write_all(b";")?;
        write_u16(out, self.0.saturating_add(1))?;
        out.write_all(b"H")
    }
}

/// Save the cursor position and attributes (`ESC 7`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SavePosition;

impl Command for SavePosition {
    fn write_ansi(&self, out: &mut (impl Write + ?Sized)) -> io::Result<()> {
        out.write_all(b"\x1b7")
    }
}

/// Restore the position saved by [`SavePosition`] (`ESC 8`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestorePosition;

impl Command for RestorePosition {
    fn write_ansi(&self, out: &mut (impl Write + ?Sized)) -> io::Result<()> {
        out.write_all(b"\x1b8")
    }
}

/// Hide the cursor (`CSI ?25l`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hide;

impl Command for Hide {
    fn write_ansi(&self, out: &mut (impl Write + ?Sized)) -> io::Result<()> {
        out.write_all(b"\x1b[?25l")
    }
}

/// Show the cursor (`CSI ?25h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Show;

impl Command for Show {
    fn write_ansi(&self, out: &mut (impl Write + ?Sized)) -> io::Result<()> {
        out.write_all(b"\x1b[?25h")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue;

    #[test]
    fn cursor_commands_write_expected_sequences() {
        let mut out = Vec::new();
        queue!(out, MoveTo(0, 0), MoveTo(79, 23), Hide, Show, SavePosition, RestorePosition)
            .unwrap();
        assert_eq!(
            out,
            b"\x1b[1;1H\x1b[24;80H\x1b[?25l\x1b[?25h\x1b7\x1b8".as_slice()
        );
    }

    #[test]
    fn move_to_saturates_at_the_largest_position() {
        let mut out = Vec::new();
        queue!(out, MoveTo(u16::MAX, u16::MAX)).unwrap();
        assert_eq!(out, b"\x1b[65535;65535H".as_slice());
    }
}
