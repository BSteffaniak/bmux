//! [`bmux_tui::terminal::Terminal`] presenter adapter.
//!
//! [`TerminalPresenter::with_commit`] publishes the exact interaction metadata
//! from the last successfully flushed frame. Applications should update their
//! [`bmux_tui::interaction::InteractionRouter`] from this callback and route
//! subsequent input through that committed scene instead of recomputing layout.

use std::io::{self, Write};

use bmux_tui::focus::FocusTrap;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Rect, Size};
use bmux_tui::hit::HitMap;
use bmux_tui::terminal::Terminal;

use crate::presenter::{PresentReport, Presenter, ResetReason};

/// No-op committed-presentation observer.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopPresentationCommit;

/// Presenter that renders application state through a BMUX terminal.
pub struct TerminalPresenter<W, R, C = NoopPresentationCommit> {
    terminal: Terminal<W>,
    render: R,
    commit: C,
}

impl<W: Write, R> TerminalPresenter<W, R, NoopPresentationCommit> {
    /// Create a terminal presenter from an owned terminal and render callback.
    #[must_use]
    pub const fn new(terminal: Terminal<W>, render: R) -> Self {
        Self {
            terminal,
            render,
            commit: NoopPresentationCommit,
        }
    }
}

impl<W: Write, R, C> TerminalPresenter<W, R, C> {
    /// Create a presenter with a callback invoked only after a frame commits.
    ///
    /// The observer receives the exact interaction scene and reconciled focus
    /// state belonging to terminal output that successfully flushed. Consumers
    /// can therefore route later input without maintaining speculative layout.
    #[must_use]
    pub const fn with_commit(terminal: Terminal<W>, render: R, commit: C) -> Self {
        Self {
            terminal,
            render,
            commit,
        }
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

    /// Return the interaction scene from the last successfully committed frame.
    #[must_use]
    pub const fn interactions(&self) -> &HitMap {
        self.terminal.hits()
    }

    /// Return ordered focus state from the last successfully committed frame.
    #[must_use]
    pub const fn focus(&self) -> &FocusTrap {
        self.terminal.focus()
    }

    /// Consume the presenter and return its terminal.
    pub fn into_terminal(self) -> Terminal<W> {
        self.terminal
    }
}

impl<P, W, R> Presenter<P> for TerminalPresenter<W, R, NoopPresentationCommit>
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

impl<P, W, R, C> Presenter<P> for TerminalPresenter<W, R, C>
where
    W: Write,
    R: FnMut(&mut P, &mut Frame<'_>),
    C: FnMut(&mut P, &HitMap, &FocusTrap),
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
        let interactions = self.terminal.hits().clone();
        let focus = self.terminal.focus().clone();
        (self.commit)(program, &interactions, &focus);
        Ok(PresentReport {
            changed_cells: stats.changed_cells,
            full_repaint: stats.full_repaint,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io::{self, Write};
    use std::rc::Rc;

    use bmux_tui::geometry::Rect;
    use bmux_tui::hit::HitRegion;

    use super::TerminalPresenter;
    use crate::presenter::Presenter;

    #[derive(Debug)]
    struct FailOnFlush {
        fail: Rc<RefCell<bool>>,
    }

    impl Write for FailOnFlush {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            if *self.fail.borrow() {
                Err(io::Error::other("injected flush failure"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn commit_observer_only_receives_successfully_flushed_scene() {
        let fail = Rc::new(RefCell::new(false));
        let terminal = bmux_tui::terminal::Terminal::new(
            FailOnFlush {
                fail: Rc::clone(&fail),
            },
            Rect::new(0, 0, 4, 1),
        );
        let committed = Rc::new(RefCell::new(Vec::new()));
        let observed = Rc::clone(&committed);
        let mut presenter = TerminalPresenter::with_commit(
            terminal,
            |program: &mut &'static str, frame: &mut bmux_tui::frame::Frame<'_>| {
                frame.push_hit(HitRegion::new(*program, frame.area()).focusable(true));
            },
            move |_: &mut &'static str,
                  hits: &bmux_tui::hit::HitMap,
                  _: &bmux_tui::focus::FocusTrap| {
                observed
                    .borrow_mut()
                    .push(hits.regions()[0].id.as_str().to_owned());
            },
        );
        let mut program = "committed";
        presenter
            .present(&mut program)
            .expect("initial frame commits");

        *fail.borrow_mut() = true;
        program = "speculative";
        assert!(presenter.present(&mut program).is_err());
        assert_eq!(committed.borrow().as_slice(), ["committed"]);
        assert_eq!(
            presenter.interactions().regions()[0].id.as_str(),
            "committed"
        );
    }
}
