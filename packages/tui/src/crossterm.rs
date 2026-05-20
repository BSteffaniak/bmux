//! Crossterm terminal lifecycle adapter.

use std::io::{self, Write};

use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

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

#[cfg(test)]
mod tests {
    // Lifecycle behavior touches the process terminal mode and is intentionally
    // not exercised in unit tests. The type is compiled by feature validation.
    #[test]
    fn crossterm_guard_module_compiles() {
        let _ = core::mem::size_of::<Option<super::CrosstermTerminalGuard<Vec<u8>>>>();
    }
}
