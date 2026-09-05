//! Inline terminal ownership without an alternate screen.

use crate::{
    ansi::{write_ansi_inline_frame, write_ansi_inline_frame_diff},
    buffer::Buffer,
};
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
    previous: Option<Buffer>,
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
            previous: None,
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
        let mut update = Vec::new();
        if let Some(previous) = &self.previous
            && previous.area() == buffer.area()
        {
            write_ansi_inline_frame_diff(&mut update, previous, buffer)?;
        } else {
            // Resize/first paint: clear and repaint in the same buffered update,
            // never flush a blank intermediate frame.
            let rows = self.rows.min(height.saturating_sub(1));
            if rows > 0 {
                write!(update, "\r\x1b[{rows}A")?;
                for _ in 0..rows {
                    update.write_all(b"\x1b[2K\r\n")?;
                }
                write!(update, "\x1b[{rows}A\r")?;
            }
            write_ansi_inline_frame(&mut update, buffer)?;
        }
        if update.is_empty() {
            return Ok(());
        }
        // Synchronized output is ignored by terminals which do not support it;
        // those terminals still benefit from changed-span writes with no clear.
        let mut transaction = Vec::with_capacity(update.len() + 16);
        transaction.extend_from_slice(b"\x1b[?2026h");
        transaction.extend_from_slice(&update);
        transaction.extend_from_slice(b"\x1b[?2026l");
        if let Err(error) = self
            .writer
            .write_all(&transaction)
            .and_then(|()| self.writer.flush())
        {
            self.previous = None;
            let _ = self.writer.write_all(b"\x1b[?2026l");
            let _ = self.writer.flush();
            return Err(error);
        }
        self.rows = buffer.area().height;
        self.previous = Some(buffer.clone());
        Ok(())
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
        self.previous = None;
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
