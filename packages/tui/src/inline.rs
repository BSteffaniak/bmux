//! Inline terminal ownership without an alternate screen.

use crate::{ansi::write_ansi_inline_frame, buffer::Buffer};
use std::io::{self, Write};

/// Owns raw mode and the rows of one inline interaction.
///
/// The cursor is kept immediately below the rendered region. Drop clears that
/// region and restores cooked input and cursor visibility. Exclusive terminal
/// output is required while this guard exists; do not print through other writers.
/// Frames must leave a spare column and row. After a terminal resize, callers
/// should remeasure before drawing; rows outside the resized screen are not erased.
pub struct InlineTerminal<W: Write> {
    writer: W,
    rows: u16,
    restore_raw: bool,
}

impl<W: Write> InlineTerminal<W> {
    /// Enter an inline interaction, preserving an already-enabled raw mode.
    ///
    /// # Errors
    /// Returns terminal mode or writer errors.
    pub fn enter(mut writer: W) -> io::Result<Self> {
        let restore_raw = !crossterm::terminal::is_raw_mode_enabled()?;
        if restore_raw {
            crossterm::terminal::enable_raw_mode()?;
        }
        if let Err(error) = writer.write_all(b"\x1b[?25l") {
            if restore_raw {
                let _ = crossterm::terminal::disable_raw_mode();
            }
            return Err(error);
        }
        Ok(Self {
            writer,
            rows: 0,
            restore_raw,
        })
    }

    /// Paint a new inline frame using BMUX's full style/Unicode renderer.
    ///
    /// # Errors
    /// Returns terminal size, invalid frame size, or output errors.
    pub fn draw(&mut self, buffer: &Buffer) -> io::Result<()> {
        let (width, height) = crossterm::terminal::size()?;
        if buffer.area().width >= width || buffer.area().height >= height {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "inline frame must leave a spare terminal row and column",
            ));
        }
        self.clear()?;
        // Record before writing so cleanup is attempted even after partial output.
        self.rows = buffer.area().height;
        write_ansi_inline_frame(&mut self.writer, buffer)?;
        self.writer.flush()
    }

    /// Clear owned visible rows and leave the cursor at their beginning.
    ///
    /// # Errors
    /// Returns terminal size or output errors.
    pub fn clear(&mut self) -> io::Result<()> {
        if self.rows == 0 {
            return Ok(());
        }
        let height = crossterm::terminal::size()?.1;
        let rows = self.rows.min(height.saturating_sub(1));
        if rows > 0 {
            write!(self.writer, "\r\x1b[{rows}A")?;
            for _ in 0..rows {
                self.writer.write_all(b"\x1b[2K\r\n")?;
            }
            write!(self.writer, "\x1b[{rows}A\r")?;
        }
        self.rows = 0;
        self.writer.flush()
    }
}

impl<W: Write> Drop for InlineTerminal<W> {
    fn drop(&mut self) {
        let _ = self.clear();
        let _ = self.writer.write_all(b"\x1b[0m\x1b[?25h");
        let _ = self.writer.flush();
        if self.restore_raw {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}
