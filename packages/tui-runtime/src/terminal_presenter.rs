//! [`bmux_tui::terminal::Terminal`] presenter adapter.
//!
//! Paint-callback presenters receive the application and a root
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

/// Synchronous presenter composing caller-owned frame policy with terminal lifecycle.
///
/// The callback draws through the same borrowed terminal and owns damage, image
/// composition, and application commit policy. It must publish interaction state
/// only after successful delivery. This adapter adds no output owner or input
/// router, and does not draw, flush, or commit a second time.
///
/// Restoration releases the terminal's pointer ownership only. Application-owned
/// image resources still require application-owned cleanup.
pub struct BorrowedTerminalPresenter<'a, W, F> {
    terminal: &'a mut Terminal<W>,
    present: F,
}

impl<'a, W, F> BorrowedTerminalPresenter<'a, W, F> {
    /// Compose an application frame callback with a caller-owned terminal.
    ///
    /// Construction performs no terminal I/O. The terminal remains caller-owned
    /// and becomes available again when this adapter's borrow ends.
    #[must_use]
    pub const fn new(terminal: &'a mut Terminal<W>, present: F) -> Self {
        Self { terminal, present }
    }
}

impl<P, W, F, E> Presenter<P> for BorrowedTerminalPresenter<'_, W, F>
where
    W: Write,
    F: FnMut(&mut P, &mut Terminal<W>) -> Result<PresentReport, E>,
    E: From<io::Error>,
{
    type Error = E;

    fn observe_terminal_event(&mut self, event: &bmux_tui::event::Event) -> bool {
        self.terminal.observe_event(event)
    }

    fn set_pointer_context(&mut self, context: bmux_tui::terminal::PointerContext) {
        self.terminal.set_pointer_context(context);
    }

    fn resize(&mut self, size: Size) {
        self.terminal
            .resize(Rect::new(0, 0, size.width, size.height));
    }

    fn reset(&mut self, _reason: ResetReason) {
        self.terminal.reset();
    }

    fn present(&mut self, program: &mut P) -> Result<PresentReport, Self::Error> {
        (self.present)(program, self.terminal)
    }

    fn restore(&mut self) -> impl std::future::Future<Output = Result<(), Self::Error>> {
        futures_util::future::lazy(move |_| self.terminal.restore_pointer().map_err(E::from))
    }
}

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
mod borrowed_tests {
    use std::io::{self, Write};

    use bmux_terminal_protocol::pointer_output::PointerShapeCapability;
    use bmux_tui::damage::Damage;
    use bmux_tui::event::{Event, MouseEvent, MouseEventKind};
    use bmux_tui::geometry::{Point, Rect, Size};
    use bmux_tui::hit::{HitRegion, HitRole};
    use bmux_tui::pointer::{PointerIntent, PointerShape};
    use bmux_tui::terminal::{PointerContext, Terminal};

    use super::BorrowedTerminalPresenter;
    use crate::presenter::{PointerContextPresenter, PresentReport, Presenter, ResetReason};

    #[derive(Default)]
    struct Output {
        bytes: Vec<u8>,
        flushes: usize,
        fail_flush: bool,
    }

    impl Write for Output {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            if self.fail_flush {
                Err(io::Error::other("injected borrowed flush failure"))
            } else {
                Ok(())
            }
        }
    }

    #[derive(Debug)]
    enum FrameError {
        Io(io::Error),
        Rejected,
    }

    impl From<io::Error> for FrameError {
        fn from(error: io::Error) -> Self {
            Self::Io(error)
        }
    }

    struct Model {
        id: &'static str,
        intent: PointerIntent,
        damage: Damage,
        calls: usize,
        paints: usize,
        commits: usize,
        reject: bool,
        capture: bool,
    }

    impl Default for Model {
        fn default() -> Self {
            Self {
                id: "target",
                intent: PointerIntent::Shape(PointerShape::Pointer),
                damage: Damage::Full,
                calls: 0,
                paints: 0,
                commits: 0,
                reject: false,
                capture: false,
            }
        }
    }

    fn frame<W: Write>(
        model: &mut Model,
        terminal: &mut Terminal<W>,
    ) -> Result<PresentReport, FrameError> {
        model.calls += 1;
        if model.reject {
            return Err(FrameError::Rejected);
        }
        let stats = terminal.draw_damage(model.damage.clone(), |cx| {
            model.paints += 1;
            cx.push_hit(
                HitRegion::new(model.id, Rect::new(0, 0, 2, 1)).pointer_intent(model.intent),
            );
            cx.push_hit(HitRegion::new("drag", Rect::new(2, 0, 2, 1)).role(HitRole::DragHandle));
        })?;
        model.commits += 1;
        Ok(PresentReport {
            changed_cells: stats.changed_cells,
            full_repaint: stats.full_repaint,
        })
    }

    fn motion() -> Event {
        Event::Mouse(MouseEvent::new(MouseEventKind::Move, Point::new(0, 0)))
    }

    fn contains(bytes: &[u8], value: &[u8]) -> bool {
        bytes.windows(value.len()).any(|window| window == value)
    }

    #[test]
    fn borrowed_frame_preserves_damage_errors_and_caller_ownership() {
        let mut output = Output::default();
        let mut terminal = Terminal::new(&mut output, Rect::new(0, 0, 4, 1));
        let mut model = Model::default();
        {
            let mut presenter = BorrowedTerminalPresenter::new(&mut terminal, frame::<&mut Output>);
            assert_eq!(presenter.terminal.writer().flushes, 0);
            let report = presenter.start_present(&mut model).unwrap().unwrap();
            assert!(report.full_repaint);
            assert_eq!(model.calls, 1);
            assert_eq!(model.commits, 1);
            assert_eq!(presenter.terminal.writer().flushes, 1);

            model.damage = Damage::None;
            model.id = "not-painted";
            presenter.present(&mut model).unwrap();
            assert_eq!(model.calls, 2);
            assert_eq!(model.paints, 1);
            assert_eq!(presenter.terminal.hits().regions()[0].id.as_str(), "target");

            model.reject = true;
            let flushes = presenter.terminal.writer().flushes;
            assert!(matches!(
                presenter.present(&mut model),
                Err(FrameError::Rejected)
            ));
            assert_eq!(model.calls, 3);
            assert_eq!(model.commits, 2);
            assert_eq!(presenter.terminal.writer().flushes, flushes);
        }
        terminal.draw(|_| {}).unwrap();
        assert!(output.flushes >= 2);
    }

    #[test]
    fn borrowed_pointer_intent_and_stationary_context_use_existing_router() {
        let mut terminal = Terminal::new(Output::default(), Rect::new(0, 0, 4, 1));
        terminal.set_pointer_capability(PointerShapeCapability::Set);
        let mut model = Model::default();
        {
            let inner = BorrowedTerminalPresenter::new(&mut terminal, frame::<Output>);
            let mut presenter =
                PointerContextPresenter::new(inner, |model: &Model| PointerContext {
                    scope: None,
                    capture: model.capture.then(|| "drag".into()),
                });
            assert!(presenter.observe_terminal_event(&motion()));
            assert!(!presenter.observe_terminal_event(&motion()));
            presenter.present(&mut model).unwrap();
            assert!(contains(
                &presenter.inner_mut().terminal.writer().bytes,
                b"\x1b]22;pointer\x1b\\",
            ));
            presenter.inner_mut().terminal.writer_mut().bytes.clear();
            model.capture = true;
            // No motion or repaint: only the existing router's capture changed.
            model.damage = Damage::None;
            presenter.present(&mut model).unwrap();
            assert_eq!(model.paints, 1);
            assert!(contains(
                &presenter.inner_mut().terminal.writer().bytes,
                b"\x1b]22;grabbing\x1b\\",
            ));
        }
    }

    #[tokio::test]
    async fn borrowed_unsupported_host_emits_no_pointer_protocol() {
        let mut terminal = Terminal::new(Output::default(), Rect::new(0, 0, 4, 1));
        terminal.set_pointer_capability(PointerShapeCapability::Unsupported);
        let mut presenter = BorrowedTerminalPresenter::new(&mut terminal, frame::<Output>);
        let mut model = Model::default();
        assert!(presenter.observe_terminal_event(&motion()));
        presenter.present(&mut model).unwrap();
        presenter.restore().await.unwrap();
        assert!(!contains(&presenter.terminal.writer().bytes, b"\x1b]22;"));
        assert_eq!(model.commits, 1);
    }

    #[test]
    fn borrowed_failed_flush_preserves_scene_and_retries_pointer_intent() {
        let mut terminal = Terminal::new(Output::default(), Rect::new(0, 0, 4, 1));
        terminal.set_pointer_capability(PointerShapeCapability::Set);
        let mut presenter = BorrowedTerminalPresenter::new(&mut terminal, frame::<Output>);
        let mut model = Model::default();
        presenter.observe_terminal_event(&motion());
        presenter.present(&mut model).unwrap();
        model.id = "replacement";
        model.intent = PointerIntent::Shape(PointerShape::Text);
        presenter.terminal.writer_mut().fail_flush = true;
        assert!(matches!(
            presenter.present(&mut model),
            Err(FrameError::Io(_))
        ));
        assert_eq!(model.commits, 1);
        assert_eq!(presenter.terminal.hits().regions()[0].id.as_str(), "target");
        presenter.terminal.writer_mut().fail_flush = false;
        presenter.terminal.writer_mut().bytes.clear();
        presenter.present(&mut model).unwrap();
        assert_eq!(model.commits, 2);
        assert_eq!(
            presenter.terminal.hits().regions()[0].id.as_str(),
            "replacement"
        );
        assert!(contains(
            &presenter.terminal.writer().bytes,
            b"\x1b]22;text\x1b\\"
        ));
    }

    #[test]
    fn borrowed_resize_and_reset_delegate_without_drawing() {
        let mut terminal = Terminal::new(Output::default(), Rect::new(0, 0, 4, 1));
        let mut presenter = BorrowedTerminalPresenter::new(&mut terminal, frame::<Output>);
        let mut model = Model::default();
        presenter.present(&mut model).unwrap();
        let flushes = presenter.terminal.writer().flushes;
        presenter.resize(Size::new(6, 2));
        assert_eq!(presenter.terminal.area(), Rect::new(0, 0, 6, 2));
        assert!(presenter.terminal.retained_buffer().is_none());
        assert_eq!(presenter.terminal.writer().flushes, flushes);
        assert!(presenter.present(&mut model).unwrap().full_repaint);
        presenter.reset(ResetReason::Application);
        assert!(presenter.terminal.retained_buffer().is_none());
        assert_eq!(model.calls, 2);
        assert!(presenter.present(&mut model).unwrap().full_repaint);
    }

    #[tokio::test]
    async fn borrowed_restore_is_lazy_and_converts_cleanup_failure() {
        let mut terminal = Terminal::new(Output::default(), Rect::new(0, 0, 4, 1));
        terminal.set_pointer_capability(PointerShapeCapability::Set);
        let mut presenter = BorrowedTerminalPresenter::new(&mut terminal, frame::<Output>);
        let mut model = Model::default();
        presenter.observe_terminal_event(&motion());
        presenter.present(&mut model).unwrap();
        presenter.terminal.writer_mut().bytes.clear();
        let flushes = presenter.terminal.writer().flushes;
        drop(presenter.restore());
        assert_eq!(presenter.terminal.writer().flushes, flushes);
        assert_eq!(presenter.terminal.writer().bytes, [] as [u8; 0]);
        presenter.terminal.writer_mut().fail_flush = true;
        match presenter.restore().await {
            Err(FrameError::Io(error)) => {
                assert_eq!(error.to_string(), "injected borrowed flush failure");
            }
            result => panic!("expected converted restoration failure, got {result:?}"),
        }
        assert_eq!(model.commits, 1);
        assert!(contains(
            &presenter.terminal.writer().bytes,
            b"\x1b]22;\x1b\\"
        ));
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
