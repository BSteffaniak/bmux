//! Output-only inline presentation with bounded frames and explicit terminal sizing.
//!
//! Does not enable raw mode, hide the cursor, or enter the alternate screen.
//! The caller must exclusively own terminal output and supply fresh dimensions.

use crate::{
    ansi::{write_ansi_inline_frame, write_ansi_inline_frame_diff},
    buffer::Buffer,
    geometry::{Rect, Size},
    text::Line,
};
use std::{
    io::{self, Write},
    time::{Duration, Instant},
};

/// An output-only inline region. No terminal modes are changed.
///
/// Call `clear` with fresh dimensions to finish; Drop retains the last frame
/// because it cannot safely discover whether the terminal has resized.
///
/// Terminal resize/reflow makes old cursor anchors unreliable. On a size change
/// this presenter abandons the old region rather than risking erasure of permanent
/// output. Old transient rows may consequently remain after resize. Callers must
/// not write through other writers while a region is active.
pub struct InlineOutput<W: Write> {
    writer: W,
    previous: Option<Buffer>,
    size: Option<Size>,
    interval: Duration,
    last_draw: Option<Instant>,
    failed: bool,
}

impl<W: Write> InlineOutput<W> {
    /// Create an output region with a minimum interval between paints.
    /// A zero interval disables throttling. Call `draw` at a deadline after a
    /// deferred update: this type does not spawn threads or schedule timers.
    #[must_use]
    pub const fn new(writer: W, interval: Duration) -> Self {
        Self {
            writer,
            previous: None,
            size: None,
            interval,
            last_draw: None,
            failed: false,
        }
    }

    /// Paint the latest lines, clipped to a bounded tail of the terminal.
    /// Returns false if rate-limited; the caller retains pending content.
    /// Pass `force` for the final update. Frames leave a spare row and column.
    ///
    /// # Errors
    /// Returns output errors. Following an output error the presenter is poisoned
    /// and rejects further drawing to avoid using an uncertain cursor anchor.
    pub fn draw(
        &mut self,
        lines: &[Line],
        size: Size,
        now: Instant,
        force: bool,
    ) -> io::Result<bool> {
        self.ensure_usable()?;
        if !force
            && self.size == Some(size)
            && self
                .last_draw
                .is_some_and(|last| now.saturating_duration_since(last) < self.interval)
        {
            return Ok(false);
        }
        if self.size.is_some_and(|old| old != size) {
            // Never move upwards using an anchor invalidated by terminal reflow.
            self.previous = None;
        }
        self.size = Some(size);
        let width = size.width.saturating_sub(1);
        let height = size
            .height
            .saturating_sub(1)
            .min(u16::try_from(lines.len()).unwrap_or(u16::MAX));
        let mut update = Vec::new();
        if width == 0 || height == 0 {
            self.clear_into(&mut update)?;
        } else {
            let mut frame = Buffer::empty(Rect::new(0, 0, width, height));
            let start = lines.len() - usize::from(height);
            for (row, line) in lines[start..].iter().enumerate() {
                frame.write_line(
                    Rect::new(0, u16::try_from(row).unwrap_or(0), width, 1),
                    line,
                );
            }
            if let Some(previous) = &self.previous
                && previous.area() == frame.area()
            {
                write_ansi_inline_frame_diff(&mut update, previous, &frame)?;
            } else {
                self.clear_into(&mut update)?;
                write_ansi_inline_frame(&mut update, &frame)?;
            }
            self.previous = Some(frame);
        }
        self.write_update(&update)?;
        self.last_draw = Some(now);
        Ok(true)
    }

    fn ensure_usable(&self) -> io::Result<()> {
        if self.failed {
            Err(io::Error::other(
                "inline output cursor state is unknown after an I/O error",
            ))
        } else {
            Ok(())
        }
    }

    fn clear_into(&mut self, update: &mut Vec<u8>) -> io::Result<()> {
        if let Some(previous) = self.previous.take() {
            let rows = previous.area().height;
            if rows > 0 {
                write!(update, "\r\x1b[{rows}A")?;
                for _ in 0..rows {
                    update.write_all(b"\x1b[2K\r\n")?;
                }
                write!(update, "\x1b[{rows}A\r")?;
            }
        }
        Ok(())
    }

    fn write_update(&mut self, update: &[u8]) -> io::Result<()> {
        if update.is_empty() {
            return Ok(());
        }
        if let Err(error) = self
            .writer
            .write_all(update)
            .and_then(|()| self.writer.flush())
        {
            self.failed = true;
            self.previous = None;
            return Err(error);
        }
        Ok(())
    }

    /// Clear transient content without touching previously committed output.
    /// Supply current terminal dimensions; resize invalidates the old anchor.
    ///
    /// # Errors
    /// Returns writer errors or an error if a previous write failed.
    pub fn clear(&mut self, size: Size) -> io::Result<()> {
        self.ensure_usable()?;
        if self.size != Some(size) {
            self.previous = None;
        }
        self.size = Some(size);
        let mut update = Vec::new();
        self.clear_into(&mut update)?;
        self.write_update(&update)?;
        self.last_draw = None;
        Ok(())
    }

    /// Clear the transient region and append permanent styled lines.
    /// Applications can use this for a success marker or captured failure output.
    /// Lines wrap explicitly at a spare column; terminal controls are never replayed.
    ///
    /// # Errors
    /// Returns writer errors or an error if a previous write failed.
    pub fn commit(&mut self, lines: &[Line], size: Size) -> io::Result<()> {
        self.clear(size)?;
        let width = size.width.saturating_sub(1).max(1);
        let mut update = Vec::new();
        for line in lines {
            for row in line.wrap_character(usize::from(width)) {
                let mut frame = Buffer::empty(Rect::new(0, 0, width, 1));
                frame.write_line(frame.area(), &row);
                write_ansi_inline_frame(&mut update, &frame)?;
                if update.len() >= 16 * 1024 {
                    self.write_update(&update)?;
                    update.clear();
                }
            }
        }
        self.write_update(&update)
    }

    /// Release the anchor without erasing it (for example before an external writer).
    /// The previously rendered region becomes permanent output.
    pub fn retain(&mut self) {
        self.previous = None;
        self.last_draw = None;
    }
}
