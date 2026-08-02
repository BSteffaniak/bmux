//! Runtime presentation contract.

use bmux_tui::geometry::Size;

/// Reason the runtime requests a backend reset before the next presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetReason {
    /// Application state invalidated all retained presentation state.
    Application,
    /// Terminal viewport changed.
    Resize,
}

/// Neutral report from one successfully committed presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PresentReport {
    /// Number of terminal cells changed, when the presenter can report it.
    pub changed_cells: usize,
    /// Whether the presentation repainted the complete surface.
    pub full_repaint: bool,
}

/// Presents the current application state to a terminal backend.
pub trait Presenter<P> {
    /// Presentation error.
    type Error;

    /// Notify the presenter that the terminal viewport changed.
    fn resize(&mut self, _size: Size) {}

    /// Reset retained backend state before a future presentation.
    fn reset(&mut self, reason: ResetReason);

    /// Present the current application state.
    ///
    /// # Errors
    ///
    /// Returns a presenter-defined error when frame construction or terminal output fails. A
    /// failed presentation must not be treated as committed by the presenter.
    fn present(&mut self, program: &mut P) -> Result<PresentReport, Self::Error>;
}

/// Presenter useful for tests and applications with no terminal output.
#[derive(Debug, Default)]
pub struct HeadlessPresenter {
    presentations: u64,
    resets: u64,
}

impl HeadlessPresenter {
    /// Number of successful presentation calls.
    #[must_use]
    pub const fn presentations(&self) -> u64 {
        self.presentations
    }

    /// Number of reset calls.
    #[must_use]
    pub const fn resets(&self) -> u64 {
        self.resets
    }
}

impl<P> Presenter<P> for HeadlessPresenter {
    type Error = std::convert::Infallible;

    fn reset(&mut self, _reason: ResetReason) {
        self.resets = self.resets.saturating_add(1);
    }

    fn present(&mut self, _program: &mut P) -> Result<PresentReport, Self::Error> {
        self.presentations = self.presentations.saturating_add(1);
        Ok(PresentReport::default())
    }
}
