//! [`bmux_tui::terminal::Terminal`] presenter adapter.
//!
//! The render callback receives the application and a root
//! [`bmux_tui::paint::PaintCx`] covering the terminal area. Applications resolve
//! their component tree under the terminal constraints and paint it through
//! that scoped context; the raw frame and buffer are never exposed.
//!
//! [`TerminalPresenter::with_commit`] publishes the exact interaction metadata
//! from the last successfully flushed frame. Applications should update their
//! [`bmux_tui::interaction::InteractionRouter`] from this callback and route
//! subsequent input through that committed scene instead of recomputing layout.
//! The committed selection scene is available through [`TerminalPresenter::selection`]
//! without changing the existing callback signature.

use std::io::{self, Write};

use bmux_tui::focus::FocusTrap;
use bmux_tui::geometry::{Rect, Size};
use bmux_tui::hit::HitMap;
use bmux_tui::paint::PaintCx;
use bmux_tui::selection::SelectionScene;
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

    /// Return the selection scene from the last successfully committed frame.
    #[must_use]
    pub const fn selection(&self) -> &SelectionScene {
        self.terminal.selection()
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
    R: FnMut(&mut P, &mut PaintCx<'_, '_>),
{
    type Error = io::Error;

    fn observe_terminal_event(&mut self, event: &bmux_tui::event::Event) -> bool {
        self.terminal.observe_event(event)
    }
    fn set_pointer_context(&mut self, context: bmux_tui::terminal::PointerContext) {
        self.terminal.set_pointer_context(context);
    }
    fn restore(&mut self) -> impl std::future::Future<Output = io::Result<()>> {
        futures_util::future::lazy(move |_| self.terminal.restore_pointer())
    }

    fn resize(&mut self, size: Size) {
        self.terminal
            .resize(Rect::new(0, 0, size.width, size.height));
    }

    fn reset(&mut self, _reason: ResetReason) {
        self.terminal.reset();
    }

    fn present(&mut self, program: &mut P) -> Result<PresentReport, Self::Error> {
        let stats = self.terminal.draw(|cx| (self.render)(program, cx))?;
        Ok(PresentReport {
            changed_cells: stats.changed_cells,
            full_repaint: stats.full_repaint,
        })
    }
}

impl<P, W, R, C> Presenter<P> for TerminalPresenter<W, R, C>
where
    W: Write,
    R: FnMut(&mut P, &mut PaintCx<'_, '_>),
    C: FnMut(&mut P, &HitMap, &FocusTrap),
{
    type Error = io::Error;

    fn observe_terminal_event(&mut self, event: &bmux_tui::event::Event) -> bool {
        self.terminal.observe_event(event)
    }
    fn set_pointer_context(&mut self, context: bmux_tui::terminal::PointerContext) {
        self.terminal.set_pointer_context(context);
    }
    fn restore(&mut self) -> impl std::future::Future<Output = io::Result<()>> {
        futures_util::future::lazy(move |_| self.terminal.restore_pointer())
    }

    fn resize(&mut self, size: Size) {
        self.terminal
            .resize(Rect::new(0, 0, size.width, size.height));
    }

    fn reset(&mut self, _reason: ResetReason) {
        self.terminal.reset();
    }

    fn present(&mut self, program: &mut P) -> Result<PresentReport, Self::Error> {
        let stats = self.terminal.draw(|cx| (self.render)(program, cx))?;
        let interactions = self.terminal.hits().clone();
        let focus = self.terminal.focus().clone();
        (self.commit)(program, &interactions, &focus);
        Ok(PresentReport {
            changed_cells: stats.changed_cells,
            full_repaint: stats.full_repaint,
        })
    }
}

/// Standard text presenter with one cancellable, exclusive host output owner.
/// No application OSC handling or additional input reader is required.
pub struct AsyncTerminalPresenter<O: crate::presenter::CancellableOutput, R, C> {
    terminal: Terminal<Vec<u8>>,
    output: Option<O>,
    render: R,
    commit: C,
    pending: Option<(
        crate::presenter::Delivery<O>,
        Terminal<Vec<u8>>,
        PresentReport,
    )>,
}

impl<O: crate::presenter::CancellableOutput, R, C> AsyncTerminalPresenter<O, R, C> {
    /// Construct with a staging terminal, exclusive output and successful-commit observer.
    pub const fn new(terminal: Terminal<Vec<u8>>, output: O, render: R, commit: C) -> Self {
        Self {
            terminal,
            output: Some(output),
            render,
            commit,
            pending: None,
        }
    }

    /// Last successfully delivered terminal, never an in-flight candidate.
    pub const fn terminal(&self) -> &Terminal<Vec<u8>> {
        &self.terminal
    }

    fn cancel_pending(&mut self) {
        if let Some((mut delivery, candidate, _)) = self.pending.take() {
            if let Ok(output) = delivery.reclaim() {
                self.output = Some(output);
            }
            self.terminal.invalidate_after_failed_delivery(&candidate);
        }
    }

    /// Cancel and join pending delivery, then release the pointer through the same owner.
    /// # Errors
    /// Reports uncertain ownership, output failure, or the bounded cleanup deadline.
    pub async fn restore_pointer(&mut self, deadline: std::time::Duration) -> io::Result<()> {
        self.cancel_pending();
        let Some(transaction) = self.terminal.prepare_pointer_restore()? else {
            return Ok(());
        };
        let mut bytes = b"\x18\x1b\\\x1b[?2026l".to_vec();
        bytes.extend_from_slice(transaction.bytes());
        let result = match crate::presenter::Delivery::start(&mut self.output, bytes) {
            Ok(mut delivery) => {
                let result =
                    tokio::time::timeout(deadline, std::future::poll_fn(|cx| delivery.poll(cx)))
                        .await
                        .unwrap_or_else(|_| {
                            Err(io::Error::new(
                                io::ErrorKind::TimedOut,
                                "pointer restoration deadline",
                            ))
                        });
                let reclaimed = delivery.reclaim();
                match reclaimed {
                    Ok(output) => {
                        self.output = Some(output);
                        result
                    }
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        };
        self.terminal
            .complete_pointer_restore(transaction, result.is_ok());
        result
    }
}

impl<P, O, R, C> Presenter<P> for AsyncTerminalPresenter<O, R, C>
where
    O: crate::presenter::CancellableOutput,
    R: FnMut(&mut P, &mut PaintCx<'_, '_>),
    C: FnMut(&mut P, &HitMap, &FocusTrap),
{
    type Error = io::Error;
    fn resize(&mut self, size: Size) {
        self.terminal
            .resize(Rect::new(0, 0, size.width, size.height));
    }
    fn reset(&mut self, _: ResetReason) {
        self.terminal.reset();
    }
    fn observe_terminal_event(&mut self, event: &bmux_tui::event::Event) -> bool {
        self.terminal.observe_event(event)
    }
    fn set_pointer_context(&mut self, context: bmux_tui::terminal::PointerContext) {
        self.terminal.set_pointer_context(context);
    }
    fn present(&mut self, _: &mut P) -> io::Result<PresentReport> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "use start_present for asynchronous output",
        ))
    }
    fn start_present(&mut self, program: &mut P) -> io::Result<Option<PresentReport>> {
        if self.pending.is_some() {
            return Err(io::Error::other("presentation already in flight"));
        }
        let mut candidate = self.terminal.fork(Vec::new());
        let stats = candidate.draw(|cx| (self.render)(program, cx))?;
        let mut bytes = b"\x18\x1b\\\x1b[?2026l".to_vec();
        bytes.extend(std::mem::take(candidate.writer_mut()));
        let delivery = crate::presenter::Delivery::start(&mut self.output, bytes)?;
        self.pending = Some((
            delivery,
            candidate,
            PresentReport {
                changed_cells: stats.changed_cells,
                full_repaint: stats.full_repaint,
            },
        ));
        Ok(None)
    }
    fn poll_present(
        &mut self,
        cx: &mut std::task::Context<'_>,
        program: &mut P,
    ) -> std::task::Poll<io::Result<PresentReport>> {
        let Some((delivery, _, _)) = &mut self.pending else {
            return std::task::Poll::Pending;
        };
        let result = std::task::ready!(delivery.poll(cx));
        let (mut delivery, candidate, report) = self.pending.take().expect("pending delivery");
        let output = match delivery.reclaim() {
            Ok(output) => output,
            Err(error) => {
                self.terminal.invalidate_after_failed_delivery(&candidate);
                return std::task::Poll::Ready(Err(error));
            }
        };
        self.output = Some(output);
        if let Err(error) = result {
            self.terminal.invalidate_after_failed_delivery(&candidate);
            return std::task::Poll::Ready(Err(error));
        }
        self.terminal.acknowledge(candidate);
        (self.commit)(program, self.terminal.hits(), self.terminal.focus());
        std::task::Poll::Ready(Ok(report))
    }
    fn cancel_present(&mut self) {
        self.cancel_pending();
    }
    async fn restore(&mut self) -> io::Result<()> {
        self.restore_pointer(std::time::Duration::from_millis(250))
            .await
    }
}

impl<O: crate::presenter::CancellableOutput, R, C> Drop for AsyncTerminalPresenter<O, R, C> {
    fn drop(&mut self) {
        self.cancel_pending();
        let Ok(Some(transaction)) = self.terminal.prepare_pointer_restore() else {
            return;
        };
        let mut bytes = b"\x18\x1b\\\x1b[?2026l".to_vec();
        bytes.extend_from_slice(transaction.bytes());
        let succeeded =
            if let Ok(mut delivery) = crate::presenter::Delivery::start(&mut self.output, bytes) {
                let result = delivery.finish_before(std::time::Duration::from_millis(250));
                if let Ok(output) = delivery.reclaim() {
                    self.output = Some(output);
                }
                result.is_ok()
            } else {
                false
            };
        self.terminal
            .complete_pointer_restore(transaction, succeeded);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io::{self, Write};
    use std::rc::Rc;

    use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::geometry::{Point, Rect};
    use bmux_tui::hit::{HitId, HitRegion};
    use bmux_tui::interaction::InteractionRouter;

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
        let router = Rc::new(RefCell::new(InteractionRouter::new()));
        let committed_router = Rc::clone(&router);
        let mut presenter = TerminalPresenter::with_commit(
            terminal,
            |program: &mut &'static str, cx: &mut bmux_tui::paint::PaintCx<'_, '_>| {
                let area = cx.area();
                cx.push_hit(
                    HitRegion::new(*program, Rect::new(0, 0, area.width, area.height))
                        .focusable(true),
                );
            },
            move |_: &mut &'static str,
                  hits: &bmux_tui::hit::HitMap,
                  _: &bmux_tui::focus::FocusTrap| {
                observed
                    .borrow_mut()
                    .push(hits.regions()[0].id.as_str().to_owned());
                committed_router
                    .borrow_mut()
                    .commit_scene(hits.clone(), None);
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
        let route = router.borrow_mut().route(Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            Point::new(0, 0),
        )));
        assert!(route.targets(&HitId::new("committed")));
        assert!(!route.targets(&HitId::new("speculative")));
        assert_eq!(
            presenter.interactions().regions()[0].id.as_str(),
            "committed"
        );
    }

    struct PointerGate {
        bytes: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
        release: std::sync::Arc<std::sync::atomic::AtomicBool>,
        started: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }
    impl crate::presenter::CancellableOutput for PointerGate {
        fn deliver(
            &mut self,
            bytes: &[u8],
            cancelled: &std::sync::atomic::AtomicBool,
        ) -> io::Result<()> {
            use std::sync::atomic::Ordering;
            self.bytes.lock().unwrap().extend_from_slice(bytes);
            self.started.store(true, Ordering::Release);
            while !self.release.load(Ordering::Acquire) {
                if cancelled.load(Ordering::Acquire) {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Ok(())
        }
    }

    async fn wait_started(started: &std::sync::atomic::AtomicBool) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !started.load(std::sync::atomic::Ordering::Acquire) {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn async_pointer_ack_preserves_newer_motion_and_commits_only_after_flush() {
        use bmux_terminal_protocol::pointer_output::PointerShapeCapability;
        use bmux_tui::pointer::PointerShape;
        use std::sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        };
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let release = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicBool::new(false));
        let mut terminal = bmux_tui::terminal::Terminal::new(Vec::new(), Rect::new(0, 0, 4, 1));
        terminal.set_pointer_capability(PointerShapeCapability::Set);
        let commits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = Arc::clone(&commits);
        let mut presenter = super::AsyncTerminalPresenter::new(
            terminal,
            PointerGate {
                bytes: Arc::clone(&bytes),
                release: Arc::clone(&release),
                started: Arc::clone(&started),
            },
            |(): &mut (), cx: &mut bmux_tui::paint::PaintCx<'_, '_>| {
                cx.push_hit(
                    HitRegion::new("target", Rect::new(0, 0, 1, 1)).pointer_intent(
                        bmux_tui::pointer::PointerIntent::Shape(PointerShape::Pointer),
                    ),
                );
            },
            move |(): &mut (), _: &bmux_tui::hit::HitMap, _: &bmux_tui::focus::FocusTrap| {
                observed.fetch_add(1, Ordering::SeqCst);
            },
        );
        assert!(
            presenter.observe_terminal_event(&Event::Mouse(MouseEvent::new(
                MouseEventKind::Move,
                Point::new(0, 0)
            )))
        );
        assert!(presenter.start_present(&mut ()).unwrap().is_none());
        wait_started(&started).await;
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        assert_eq!(presenter.terminal().hits().regions(), []);
        presenter.observe_terminal_event(&Event::Mouse(MouseEvent::new(
            MouseEventKind::Move,
            Point::new(3, 0),
        )));
        release.store(true, Ordering::Release);
        std::future::poll_fn(|cx| presenter.poll_present(cx, &mut ()))
            .await
            .unwrap();
        assert_eq!(commits.load(Ordering::SeqCst), 1);
        bytes.lock().unwrap().clear();
        presenter.start_present(&mut ()).unwrap();
        std::future::poll_fn(|cx| presenter.poll_present(cx, &mut ()))
            .await
            .unwrap();
        assert!(
            bytes
                .lock()
                .unwrap()
                .windows(b"\x1b]22;default\x1b\\".len())
                .any(|bytes| bytes == b"\x1b]22;default\x1b\\")
        );
        presenter
            .restore_pointer(std::time::Duration::from_secs(1))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn cancelled_stack_push_never_commits_or_replays_cleanup() {
        use bmux_terminal_protocol::pointer_output::PointerShapeCapability;
        use std::sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        };
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let started = Arc::new(AtomicBool::new(false));
        let mut terminal = bmux_tui::terminal::Terminal::new(Vec::new(), Rect::new(0, 0, 4, 1));
        terminal.set_pointer_capability(PointerShapeCapability::Stack);
        let commits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = Arc::clone(&commits);
        let mut presenter = super::AsyncTerminalPresenter::new(
            terminal,
            PointerGate {
                bytes: Arc::clone(&bytes),
                release: Arc::new(AtomicBool::new(false)),
                started: Arc::clone(&started),
            },
            |(): &mut (), _: &mut bmux_tui::paint::PaintCx<'_, '_>| {},
            move |(): &mut (), _: &bmux_tui::hit::HitMap, _: &bmux_tui::focus::FocusTrap| {
                observed.fetch_add(1, Ordering::SeqCst);
            },
        );
        presenter.start_present(&mut ()).unwrap();
        wait_started(&started).await;
        presenter.cancel_present();
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        let count = bytes.lock().unwrap().len();
        assert!(
            presenter
                .restore_pointer(std::time::Duration::from_secs(1))
                .await
                .is_err()
        );
        assert!(presenter.start_present(&mut ()).is_err());
        drop(presenter);
        assert_eq!(bytes.lock().unwrap().len(), count);
    }
}
