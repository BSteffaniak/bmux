use anyhow::{Context, Result};
use crossterm::{
    cursor, execute,
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::io::{self, Write};

pub(crate) struct TerminalGuard {
    alt_screen: bool,
    reserve_top_row: bool,
}

impl TerminalGuard {
    pub(crate) fn activate(alt_screen: bool, reserve_top_row: bool) -> Result<Self> {
        terminal::enable_raw_mode().context("failed to enable terminal raw mode")?;

        if alt_screen {
            execute!(
                io::stdout(),
                EnterAlternateScreen,
                Clear(ClearType::All),
                cursor::MoveTo(0, 0)
            )
            .context("failed to enter alternate screen")?;
        }

        let guard = Self {
            alt_screen,
            reserve_top_row,
        };

        if reserve_top_row {
            let (_, rows) = terminal::size().context("failed to read terminal size")?;
            guard.refresh_layout(rows)?;
        }

        Ok(guard)
    }

    pub(crate) fn refresh_layout(&self, rows: u16) -> Result<()> {
        if !self.reserve_top_row || rows <= 1 {
            return Ok(());
        }

        write!(io::stdout(), "\x1b[2;{rows}r\x1b[2;1H")
            .context("failed to apply terminal scroll region")?;
        io::stdout()
            .flush()
            .context("failed to flush terminal layout update")?;
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.reserve_top_row {
            let _ = write!(io::stdout(), "\x1b[r");
            let _ = io::stdout().flush();
        }

        if self.alt_screen {
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
        }

        let _ = terminal::disable_raw_mode();
    }
}
