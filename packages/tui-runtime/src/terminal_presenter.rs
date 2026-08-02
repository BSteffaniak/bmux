//! [`bmux_tui::terminal::Terminal`] presenter adapter.

use std::io::{self, Write};

use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Rect, Size};
use bmux_tui::terminal::Terminal;

use crate::presenter::{PresentReport, Presenter, ResetReason};

/// Presenter that renders application state through a BMUX terminal.
pub struct TerminalPresenter<W, R> {
    terminal: Terminal<W>,
    render: R,
}

impl<W, R> TerminalPresenter<W, R> {
    /// Create a terminal presenter from an owned terminal and render callback.
    #[must_use]
    pub const fn new(terminal: Terminal<W>, render: R) -> Self {
        Self { terminal, render }
    }

    /// Return the underlying terminal.
    #[must_use]
    pub const fn terminal(&self) -> &Terminal<W> {
        &self.terminal
    }

    /// Return the underlying terminal mutably.
    pub const fn terminal_mut(&mut self) -> &mut Terminal<W> {
        &mut self.terminal
    }

    /// Consume the presenter and return its terminal.
    pub fn into_terminal(self) -> Terminal<W> {
        self.terminal
    }
}

impl<P, W, R> Presenter<P> for TerminalPresenter<W, R>
where
    W: Write,
    R: FnMut(&mut P, &mut Frame<'_>),
{
    type Error = io::Error;

    fn resize(&mut self, size: Size) {
        self.terminal
            .resize(Rect::new(0, 0, size.width, size.height));
    }

    fn reset(&mut self, _reason: ResetReason) {
        self.terminal.reset();
    }

    fn present(&mut self, program: &mut P) -> Result<PresentReport, Self::Error> {
        let stats = self.terminal.draw(|frame| (self.render)(program, frame))?;
        Ok(PresentReport {
            changed_cells: stats.changed_cells,
            full_repaint: stats.full_repaint,
        })
    }
}
