//! Image-capable [`bmux_tui::terminal::Terminal`] presenter adapter.

use std::io::{self, Write};

use bmux_image::ImageConfig;
use bmux_image::compositor::PaneRect;
use bmux_image::host_caps::HostImageCapabilities;
use bmux_image::tui::{TuiImageCompositor, TuiImageError};
use bmux_tui::focus::FocusTrap;
use bmux_tui::geometry::{Rect, Size};
use bmux_tui::hit::HitMap;
use bmux_tui::paint::PaintCx;
use bmux_tui::presentation::{PreparedPresentation, PresentationPhase};
use bmux_tui::selection::SelectionScene;
use bmux_tui::terminal::Terminal;

use crate::presenter::{PresentReport, Presenter, ResetReason};
use crate::terminal_presenter::NoopPresentationCommit;

/// Presentation error produced by an image-capable terminal presenter.
#[derive(Debug)]
pub enum ImagePresentationError {
    /// Cell or terminal output failed.
    Io(io::Error),
    /// A protocol-neutral image payload was invalid.
    Image(TuiImageError),
}

impl std::fmt::Display for ImagePresentationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::Image(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ImagePresentationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Image(error) => Some(error),
        }
    }
}

impl From<io::Error> for ImagePresentationError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Default prepared-snapshot configuration preserving existing constructors.
pub struct NoopPreparedConfiguration;

/// Caller-owned configuration applied before output, never a focus authority.
pub trait PreparedConfiguration<P> {
    /// Stage metadata and validate admission before the backend writes anything.
    ///
    /// # Errors
    /// Returns a rejected configuration or admission error.
    fn configure(&mut self, program: &mut P, prepared: &mut PreparedPresentation)
    -> io::Result<()>;
}

impl<P> PreparedConfiguration<P> for NoopPreparedConfiguration {
    fn configure(
        &mut self,
        _program: &mut P,
        _prepared: &mut PreparedPresentation,
    ) -> io::Result<()> {
        Ok(())
    }
}

impl<P, F> PreparedConfiguration<P> for F
where
    F: FnMut(&mut P, &mut PreparedPresentation) -> io::Result<()>,
{
    fn configure(
        &mut self,
        program: &mut P,
        prepared: &mut PreparedPresentation,
    ) -> io::Result<()> {
        self(program, prepared)
    }
}

/// Presenter that commits cell and terminal-image output through one flush.
pub struct ImageTerminalPresenter<W, R, C = NoopPresentationCommit, Q = NoopPreparedConfiguration> {
    terminal: Terminal<W>,
    render: R,
    commit: C,
    configure: Q,
    compositor: TuiImageCompositor,
    host_capabilities: HostImageCapabilities,
    image_config: ImageConfig,
}

impl<W: Write, R> ImageTerminalPresenter<W, R, NoopPresentationCommit> {
    /// Create an image-capable presenter using safe environment-only capability detection.
    ///
    /// This constructor performs no terminal I/O and therefore cannot consume
    /// input intended for the runtime. Callers that need active terminal
    /// queries should detect capabilities while raw mode is active, before
    /// starting runtime input admission, and pass them to [`Self::new`].
    #[must_use]
    pub fn detect(terminal: Terminal<W>, render: R, image_config: ImageConfig) -> Self {
        Self::new(
            terminal,
            render,
            bmux_image::host_caps::detect_from_env(),
            image_config,
        )
    }

    /// Create an image-capable presenter with pre-detected capabilities.
    #[must_use]
    pub fn new(
        terminal: Terminal<W>,
        render: R,
        host_capabilities: HostImageCapabilities,
        image_config: ImageConfig,
    ) -> Self {
        Self::with_commit(
            terminal,
            render,
            NoopPresentationCommit,
            host_capabilities,
            image_config,
        )
    }
}

impl<W: Write, R, C, Q> ImageTerminalPresenter<W, R, C, Q> {
    /// Refresh pixel geometry without reading terminal input or changing protocols.
    pub fn refresh_cell_metrics(&mut self) {
        let (width, height) = bmux_image::host_caps::query_cell_pixel_size();
        if width != 0 && height != 0 {
            self.host_capabilities.cell_pixel_width = width;
            self.host_capabilities.cell_pixel_height = height;
        }
    }
}

impl<W: Write, R, C> ImageTerminalPresenter<W, R, C> {
    /// Create an image-capable presenter with a successful-commit observer.
    #[must_use]
    pub fn with_commit(
        terminal: Terminal<W>,
        render: R,
        commit: C,
        host_capabilities: HostImageCapabilities,
        image_config: ImageConfig,
    ) -> Self {
        Self {
            terminal,
            render,
            commit,
            configure: NoopPreparedConfiguration,
            compositor: TuiImageCompositor::new(),
            host_capabilities,
            image_config,
        }
    }
}

impl<W: Write, R, C, Q> ImageTerminalPresenter<W, R, C, Q> {
    /// Install caller-owned prepared metadata configuration before output.
    /// The callback receives `(&mut P, &mut PreparedPresentation)` and returns
    /// `io::Result<()>`. Stage scopes first, then call `validate_limits` with
    /// the application's admission budget; rejection performs no output.
    #[must_use]
    pub fn configure_prepared<F>(self, configure: F) -> ImageTerminalPresenter<W, R, C, F> {
        ImageTerminalPresenter {
            terminal: self.terminal,
            render: self.render,
            commit: self.commit,
            configure,
            compositor: self.compositor,
            host_capabilities: self.host_capabilities,
            image_config: self.image_config,
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

    /// Return the last committed interaction scene.
    #[must_use]
    pub const fn interactions(&self) -> &HitMap {
        self.terminal.hits()
    }

    /// Return the last committed selection scene.
    #[must_use]
    pub const fn selection(&self) -> &SelectionScene {
        self.terminal.selection()
    }

    /// Return the last committed focus state.
    #[must_use]
    pub const fn focus(&self) -> &FocusTrap {
        self.terminal.focus()
    }

    /// Return detected host image capabilities.
    #[must_use]
    pub const fn host_capabilities(&self) -> &HostImageCapabilities {
        &self.host_capabilities
    }

    /// Reset retained terminal and image presentation state.
    ///
    /// Existing host images are removed before the next frame is presented.
    /// If cleanup output fails, the presenter retains the pending removals and
    /// retries them during the next presentation.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when protocol cleanup cannot be written or flushed.
    pub fn reset_presentation(&mut self) -> Result<(), ImagePresentationError> {
        self.cleanup_images()?;
        self.terminal.reset();
        Ok(())
    }

    /// Remove all committed terminal images from the host.
    ///
    /// Call this before returning terminal ownership, including graceful exit,
    /// suspension, and recovery from an application error.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when protocol cleanup cannot be written or flushed.
    pub fn cleanup_images(&mut self) -> Result<(), ImagePresentationError> {
        let mut candidate = self.compositor.clone();
        let result = candidate
            .cleanup(self.terminal.writer_mut(), &self.host_capabilities)
            .and_then(|()| self.terminal.writer_mut().flush());
        self.terminal.reset();
        if let Err(error) = result {
            self.compositor.invalidate_after_failed_output(&candidate);
            return Err(error.into());
        }
        self.compositor = candidate;
        Ok(())
    }

    /// Restore all presentation protocols through the same output owner.
    /// Both cleanups are attempted even if one fails.
    /// # Errors
    /// Reports image or pointer cleanup failure; uncertain stack state is not replayed.
    pub fn cleanup_presentation(&mut self) -> Result<(), ImagePresentationError> {
        let images = self.cleanup_images();
        let pointer = self.terminal.restore_pointer();
        images?;
        pointer?;
        Ok(())
    }

    /// Consume the presenter after cleaning host image and pointer resources and return its terminal.
    ///
    /// # Errors
    ///
    /// Returns an error when protocol cleanup cannot be written or flushed.
    pub fn into_clean_terminal(mut self) -> Result<Terminal<W>, ImagePresentationError> {
        self.cleanup_presentation()?;
        Ok(self.terminal)
    }
}

impl<P, W, R, Q> Presenter<P> for ImageTerminalPresenter<W, R, NoopPresentationCommit, Q>
where
    W: Write,
    R: FnMut(&mut P, &mut PaintCx<'_, '_>),
    Q: PreparedConfiguration<P>,
{
    type Error = ImagePresentationError;

    fn observe_terminal_event(&mut self, event: &bmux_tui::event::Event) -> bool {
        self.terminal.observe_event(event)
    }
    fn set_pointer_context(&mut self, context: bmux_tui::terminal::PointerContext) {
        self.terminal.set_pointer_context(context);
    }
    fn restore(&mut self) -> impl std::future::Future<Output = Result<(), Self::Error>> {
        futures_util::future::lazy(move |_| self.cleanup_presentation())
    }

    fn resize(&mut self, size: Size) {
        self.refresh_cell_metrics();
        self.terminal
            .resize(Rect::new(0, 0, size.width, size.height));
    }

    fn reset(&mut self, _reason: ResetReason) {
        self.compositor.clear();
        self.terminal.reset();
    }

    fn present(&mut self, program: &mut P) -> Result<PresentReport, Self::Error> {
        present_image_frame(
            &mut self.terminal,
            &mut self.render,
            &mut self.configure,
            &mut self.compositor,
            &self.host_capabilities,
            &self.image_config,
            program,
        )
    }
}

impl<P, W, R, C, Q> Presenter<P> for ImageTerminalPresenter<W, R, C, Q>
where
    W: Write,
    R: FnMut(&mut P, &mut PaintCx<'_, '_>),
    C: FnMut(&mut P, &HitMap, &FocusTrap),
    Q: PreparedConfiguration<P>,
{
    type Error = ImagePresentationError;

    fn observe_terminal_event(&mut self, event: &bmux_tui::event::Event) -> bool {
        self.terminal.observe_event(event)
    }
    fn set_pointer_context(&mut self, context: bmux_tui::terminal::PointerContext) {
        self.terminal.set_pointer_context(context);
    }
    fn restore(&mut self) -> impl std::future::Future<Output = Result<(), Self::Error>> {
        futures_util::future::lazy(move |_| self.cleanup_presentation())
    }

    fn resize(&mut self, size: Size) {
        self.refresh_cell_metrics();
        self.terminal
            .resize(Rect::new(0, 0, size.width, size.height));
    }

    fn reset(&mut self, _reason: ResetReason) {
        self.compositor.clear();
        self.terminal.reset();
    }

    fn present(&mut self, program: &mut P) -> Result<PresentReport, Self::Error> {
        let report = present_image_frame(
            &mut self.terminal,
            &mut self.render,
            &mut self.configure,
            &mut self.compositor,
            &self.host_capabilities,
            &self.image_config,
            program,
        )?;
        (self.commit)(program, self.terminal.hits(), self.terminal.focus());
        Ok(report)
    }
}

fn present_image_frame<P, W, R, Q>(
    terminal: &mut Terminal<W>,
    render: &mut R,
    configure: &mut Q,
    compositor: &mut TuiImageCompositor,
    capabilities: &HostImageCapabilities,
    config: &ImageConfig,
    program: &mut P,
) -> Result<PresentReport, ImagePresentationError>
where
    W: Write,
    R: FnMut(&mut P, &mut PaintCx<'_, '_>),
    Q: PreparedConfiguration<P>,
{
    let terminal_area = terminal.area();
    let pane = PaneRect {
        x: terminal_area.x,
        y: terminal_area.y,
        w: terminal_area.width,
        h: terminal_area.height,
    };
    let staged_compositor = std::cell::RefCell::new(
        PresentationPhase::CompositorClone.measure(|| compositor.clone(), |_| true),
    );
    let program = std::cell::RefCell::new(program);
    let stats = terminal.draw_damage_with_prepared_output_hooks(
        bmux_tui::damage::Damage::Full,
        |cx| render(&mut program.borrow_mut(), cx),
        |prepared| configure.configure(&mut program.borrow_mut(), prepared),
        |writer| {
            PresentationPhase::CompositorPrepareOutput.measure(
                || staged_compositor.borrow_mut().prepare_output(writer),
                Result::is_ok,
            )
        },
        |writer, scene, delta| {
            let mut staged_compositor = staged_compositor.borrow_mut();
            PresentationPhase::CompositorApplyDelta
                .measure(|| staged_compositor.apply_delta(delta), |()| true);
            PresentationPhase::CompositorRender
                .measure(
                    || staged_compositor.render(writer, scene, pane, capabilities, config),
                    Result::is_ok,
                )
                .map_err(|error| match error {
                    TuiImageError::Io(error) => error,
                    TuiImageError::Cancelled => {
                        io::Error::new(io::ErrorKind::Interrupted, "image presentation cancelled")
                    }
                    error @ TuiImageError::InvalidPayload { .. } => {
                        io::Error::new(io::ErrorKind::InvalidData, error)
                    }
                })
        },
    );
    let stats = match stats {
        Ok(stats) => stats,
        Err(error) => {
            compositor.invalidate_after_failed_output(&staged_compositor.borrow());
            return Err(error.into());
        }
    };
    *compositor = staged_compositor.into_inner();
    Ok(PresentReport {
        changed_cells: stats.changed_cells,
        full_repaint: stats.full_repaint,
    })
}

/// Image presenter with one owned off-loop transaction and exact candidate tokens.
///
/// The inner terminal uses an in-memory staging writer; it never writes to the host.
/// Only the acknowledgment callback may publish application interaction authority.
pub struct AsyncImageTerminalPresenter<
    R,
    Q,
    O: crate::presenter::CancellableOutput,
    T,
    Take,
    Commit,
> {
    inner: ImageTerminalPresenter<Vec<u8>, R, NoopPresentationCommit, Q>,
    output: Option<O>,
    take: Take,
    commit: Commit,
    pending: Option<ImageDelivery<O, T>>,
}

struct ImageDelivery<O, T> {
    delivery: crate::presenter::Delivery<FrameOutput<O>>,
    token: T,
}

struct FrameOutput<O> {
    output: O,
    terminal: Terminal<Vec<u8>>,
    compositor: TuiImageCompositor,
    prepared: Option<PreparedPresentation>,
    capabilities: HostImageCapabilities,
    config: ImageConfig,
    report: Option<PresentReport>,
}

impl<O: crate::CancellableOutput> crate::CancellableOutput for FrameOutput<O> {
    fn deliver(&mut self, _: &[u8], _: &std::sync::atomic::AtomicBool) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "owned codec cancellation required",
        ))
    }
    fn deliver_owned(
        &mut self,
        _: &[u8],
        cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> io::Result<()> {
        if cancelled.load(std::sync::atomic::Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "encoding cancelled",
            ));
        }
        let area = self.terminal.area();
        let pane = PaneRect {
            x: area.x,
            y: area.y,
            w: area.width,
            h: area.height,
        };
        let compositor = std::cell::RefCell::new(&mut self.compositor);
        let stats = self.terminal.draw_prepared_with_output_hooks(
            self.prepared.take(),
            |writer| {
                PresentationPhase::CompositorPrepareOutput.measure(
                    || compositor.borrow_mut().prepare_output(writer),
                    Result::is_ok,
                )
            },
            |writer, scene, delta| {
                let mut compositor = compositor.borrow_mut();
                PresentationPhase::CompositorApplyDelta
                    .measure(|| compositor.apply_delta(delta), |()| true);
                PresentationPhase::CompositorRender
                    .measure(
                        || {
                            compositor.render_cancellable(
                                writer,
                                scene,
                                pane,
                                &self.capabilities,
                                &self.config,
                                Some(std::sync::Arc::clone(&cancelled)),
                            )
                        },
                        Result::is_ok,
                    )
                    .map_err(|error| match error {
                        TuiImageError::Cancelled => {
                            io::Error::new(io::ErrorKind::Interrupted, "image encoding cancelled")
                        }
                        TuiImageError::Io(error) => error,
                        error @ TuiImageError::InvalidPayload { .. } => {
                            io::Error::new(io::ErrorKind::InvalidData, error)
                        }
                    })
            },
        )?;
        let mut bytes = b"\x18\x1b\\\x1b[?2026l".to_vec();
        bytes.extend(std::mem::take(self.terminal.writer_mut()));
        self.output.deliver(&bytes, &cancelled)?;
        self.report = Some(PresentReport {
            changed_cells: stats.changed_cells,
            full_repaint: stats.full_repaint,
        });
        Ok(())
    }
}

impl<R, Q, O: crate::presenter::CancellableOutput, T, Take, Commit>
    AsyncImageTerminalPresenter<R, Q, O, T, Take, Commit>
{
    /// Construct the opt-in asynchronous adapter. `take` extracts the exact prepared
    /// application snapshot; `commit` receives it only after successful host flush.
    #[must_use]
    pub const fn new(
        inner: ImageTerminalPresenter<Vec<u8>, R, NoopPresentationCommit, Q>,
        output: O,
        take: Take,
        commit: Commit,
    ) -> Self {
        Self {
            inner,
            output: Some(output),
            take,
            commit,
            pending: None,
        }
    }

    /// Last successfully delivered terminal authority (not the pending candidate).
    #[must_use]
    pub const fn terminal(&self) -> &Terminal<Vec<u8>> {
        &self.inner.terminal
    }
}

impl<R, Q, O: crate::presenter::CancellableOutput, T, Take, Commit>
    AsyncImageTerminalPresenter<R, Q, O, T, Take, Commit>
{
    /// Start a connection-local shared-memory probe on this exclusive output owner.
    /// # Errors
    /// Returns probe preparation or bounded delivery errors.
    #[cfg(feature = "image-kitty")]
    pub async fn begin_shared_memory_probe(
        &mut self,
        now: std::time::Instant,
    ) -> Result<bool, ImagePresentationError> {
        if self.pending.is_some() {
            return Err(io::Error::other("frame in flight").into());
        }
        let mut bytes = Vec::new();
        let started = self
            .inner
            .compositor
            .begin_shared_memory_probe(&mut bytes, now)?;
        if started
            && let Err(error) = self
                .deliver_control(bytes, std::time::Duration::from_millis(250))
                .await
        {
            self.inner.compositor.abort_shared_memory();
            return Err(error);
        }
        Ok(started)
    }
    /// Consume only a matching current-connection reply. Busy candidates defer admission.
    #[cfg(feature = "image-kitty")]
    pub fn handle_shared_memory_reply(&mut self, frame: &[u8], now: std::time::Instant) -> bool {
        if self.pending.is_some() {
            return false;
        }
        self.inner.compositor.expire_shared_memory(now);
        self.inner.compositor.handle_shared_memory_reply(frame, now)
    }

    /// Deliver lifecycle/probe bytes through the same exclusive output owner.
    /// Does not publish frame authority. No frame may be in flight.
    /// # Errors
    /// Returns a busy, output, worker, or deadline error.
    pub async fn deliver_control(
        &mut self,
        bytes: Vec<u8>,
        deadline: std::time::Duration,
    ) -> Result<(), ImagePresentationError> {
        if self.pending.is_some() {
            return Err(io::Error::other("frame in flight").into());
        }
        let mut delivery = crate::presenter::Delivery::start(&mut self.output, bytes)?;
        let result = tokio::time::timeout(deadline, std::future::poll_fn(|cx| delivery.poll(cx)))
            .await
            .unwrap_or_else(|_| {
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "control delivery deadline",
                ))
            });
        self.output = Some(delivery.reclaim()?);
        result.map_err(Into::into)
    }

    /// Cancel pending output and remove host images within `deadline`.
    ///
    /// Call before restoring the terminal. Failure may leave host resources visible.
    /// Dropping this future cancels/joins output, but closes its owner; the adapter
    /// cannot be reused after an interrupted cleanup future.
    /// # Errors
    /// Returns output, worker-start/join, or deadline errors.
    pub async fn cleanup_images(
        &mut self,
        deadline: std::time::Duration,
    ) -> Result<(), ImagePresentationError> {
        if let Some(mut pending) = self.pending.take()
            && let Ok(frame) = pending.delivery.reclaim()
        {
            self.output = Some(frame.output);
            self.inner
                .terminal
                .invalidate_after_failed_delivery(&frame.terminal);
            self.inner
                .compositor
                .invalidate_after_failed_output(&frame.compositor);
        }
        let mut candidate = self.inner.compositor.clone();
        // CAN aborts an interrupted CSI/string; ST terminates APC/DCS/OSC,
        // including Kitty/iTerm/Sixel payloads. End synchronized update before
        // appending ordinary image cleanup or terminal restoration commands.
        let mut bytes = b"\x18\x1b\\\x1b[?2026l".to_vec();
        candidate.cleanup(&mut bytes, &self.inner.host_capabilities)?;
        let pointer = self.inner.terminal.prepare_pointer_restore();
        if let Ok(Some(pointer)) = &pointer {
            bytes.extend_from_slice(pointer.bytes());
        }
        let mut delivery = crate::presenter::Delivery::start(&mut self.output, bytes)?;
        let result = tokio::time::timeout(deadline, std::future::poll_fn(|cx| delivery.poll(cx)))
            .await
            .unwrap_or_else(|_| {
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "image cleanup deadline",
                ))
            });
        self.output = Some(delivery.reclaim()?);
        match pointer {
            Ok(Some(pointer)) => self
                .inner
                .terminal
                .complete_pointer_restore(pointer, result.is_ok()),
            Ok(None) => {}
            Err(error) => return Err(error.into()),
        }
        self.inner.terminal.reset();
        if let Err(error) = result {
            self.inner
                .compositor
                .invalidate_after_failed_output(&candidate);
            return Err(error.into());
        }
        self.inner.compositor = candidate;
        #[cfg(feature = "image-kitty")]
        self.inner.compositor.abort_shared_memory();
        Ok(())
    }
}

impl<P, R, Q, O, T, Take, Commit> Presenter<P>
    for AsyncImageTerminalPresenter<R, Q, O, T, Take, Commit>
where
    R: FnMut(&mut P, &mut PaintCx<'_, '_>),
    Q: PreparedConfiguration<P>,
    O: crate::presenter::CancellableOutput,
    Take: FnMut(&mut P) -> T,
    Commit: FnMut(&mut P, T, &HitMap, &FocusTrap),
{
    type Error = ImagePresentationError;

    fn observe_terminal_event(&mut self, event: &bmux_tui::event::Event) -> bool {
        self.inner.terminal.observe_event(event)
    }
    fn set_pointer_context(&mut self, context: bmux_tui::terminal::PointerContext) {
        self.inner.terminal.set_pointer_context(context);
    }
    async fn restore(&mut self) -> Result<(), Self::Error> {
        self.cleanup_images(std::time::Duration::from_millis(250))
            .await
    }

    fn resize(&mut self, size: Size) {
        self.inner.refresh_cell_metrics();
        self.inner
            .terminal
            .resize(Rect::new(0, 0, size.width, size.height));
    }
    fn reset(&mut self, _: ResetReason) {
        self.inner.compositor.clear();
        self.inner.terminal.reset();
    }
    fn present(&mut self, _: &mut P) -> Result<PresentReport, Self::Error> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "use Runtime/start_present for asynchronous output",
        )
        .into())
    }
    fn start_present(&mut self, program: &mut P) -> Result<Option<PresentReport>, Self::Error> {
        if self.pending.is_some() {
            return Err(io::Error::other("presentation already in flight").into());
        }
        let terminal = self.inner.terminal.fork(Vec::new());
        let mut prepared = terminal.prepare_frame(bmux_tui::damage::Damage::Full, |cx| {
            (self.inner.render)(program, cx);
        });
        if let Some(prepared) = &mut prepared {
            PresentationPhase::Configure.measure(
                || self.inner.configure.configure(program, prepared),
                Result::is_ok,
            )?;
        }
        let token = (self.take)(program);
        let output = self
            .output
            .take()
            .ok_or_else(|| io::Error::other("output owner unavailable"))?;
        let mut frame = Some(FrameOutput {
            output,
            terminal,
            compositor: PresentationPhase::CompositorClone
                .measure(|| self.inner.compositor.clone(), |_| true),
            prepared,
            capabilities: self.inner.host_capabilities.clone(),
            config: self.inner.image_config.clone(),
            report: None,
        });
        let delivery = match crate::presenter::Delivery::start(&mut frame, Vec::new()) {
            Ok(delivery) => delivery,
            Err(error) => {
                self.output = frame.map(|frame| frame.output);
                return Err(error.into());
            }
        };
        self.pending = Some(ImageDelivery { delivery, token });
        Ok(None)
    }
    fn poll_present(
        &mut self,
        cx: &mut std::task::Context<'_>,
        program: &mut P,
    ) -> std::task::Poll<Result<PresentReport, Self::Error>> {
        let Some(pending) = &mut self.pending else {
            return std::task::Poll::Pending;
        };
        let result = std::task::ready!(pending.delivery.poll(cx));
        let mut pending = self.pending.take().expect("pending delivery");
        let frame = match pending.delivery.reclaim() {
            Ok(frame) => frame,
            Err(error) => return std::task::Poll::Ready(Err(error.into())),
        };
        self.output = Some(frame.output);
        if let Err(error) = result {
            self.inner
                .compositor
                .invalidate_after_failed_output(&frame.compositor);
            self.inner
                .terminal
                .invalidate_after_failed_delivery(&frame.terminal);
            return std::task::Poll::Ready(Err(error.into()));
        }
        self.inner.terminal.acknowledge(frame.terminal);
        self.inner.compositor = frame.compositor;
        (self.commit)(
            program,
            pending.token,
            self.inner.terminal.hits(),
            self.inner.terminal.focus(),
        );
        std::task::Poll::Ready(Ok(frame.report.unwrap_or_default()))
    }
    fn cancel_present(&mut self) {
        if let Some(mut pending) = self.pending.take() {
            if let Ok(frame) = pending.delivery.reclaim() {
                self.output = Some(frame.output);
                self.inner
                    .terminal
                    .invalidate_after_failed_delivery(&frame.terminal);
                self.inner
                    .compositor
                    .invalidate_after_failed_output(&frame.compositor);
            }
            self.inner.terminal.reset();
        }
    }
}

impl<R, Q, O: crate::presenter::CancellableOutput, T, Take, Commit> Drop
    for AsyncImageTerminalPresenter<R, Q, O, T, Take, Commit>
{
    fn drop(&mut self) {
        if let Some(mut pending) = self.pending.take()
            && let Ok(frame) = pending.delivery.reclaim()
        {
            self.output = Some(frame.output);
            self.inner
                .terminal
                .invalidate_after_failed_delivery(&frame.terminal);
            self.inner
                .compositor
                .invalidate_after_failed_output(&frame.compositor);
        }
        let mut candidate = self.inner.compositor.clone();
        let mut bytes = b"\x18\x1b\\\x1b[?2026l".to_vec();
        let _ = candidate.cleanup(&mut bytes, &self.inner.host_capabilities);
        let pointer = self.inner.terminal.prepare_pointer_restore();
        if let Ok(Some(pointer)) = &pointer {
            bytes.extend_from_slice(pointer.bytes());
        }
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
        if let Ok(Some(pointer)) = pointer {
            self.inner
                .terminal
                .complete_pointer_restore(pointer, succeeded);
        }
    }
}

#[cfg(test)]
mod async_tests {
    use super::*;
    use crate::{Program, Runtime, RuntimeConfig, RuntimeEvent, Update};
    use std::convert::Infallible;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use std::time::Duration;

    struct Gate {
        release: Arc<AtomicBool>,
        started: Arc<AtomicUsize>,
        alive: Arc<AtomicUsize>,
        fail: bool,
        fail_write: bool,
    }
    impl Write for Gate {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.fail_write {
                Err(io::Error::other("injected write failure"))
            } else {
                Ok(bytes.len())
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            if self.fail {
                Err(io::Error::other("injected flush failure"))
            } else {
                Ok(())
            }
        }
    }
    // Bounded sleeps and atomic cancellation checks; flush gate is cancellable.
    impl crate::CancellableOutput for Gate {
        fn deliver(&mut self, bytes: &[u8], cancelled: &AtomicBool) -> io::Result<()> {
            self.write_all(bytes)?;
            self.alive.fetch_add(1, Ordering::SeqCst);
            self.started.fetch_add(1, Ordering::SeqCst);
            while !self.release.load(Ordering::SeqCst) && !cancelled.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
            self.alive.fetch_sub(1, Ordering::SeqCst);
            if cancelled.load(Ordering::SeqCst) {
                Err(io::Error::other("injected cancellation"))
            } else {
                self.flush()
            }
        }
    }
    struct App {
        value: u64,
        observed: tokio::sync::mpsc::UnboundedSender<u64>,
    }
    impl Program for App {
        type Message = u64;
        type Error = Infallible;
        fn update(&mut self, event: RuntimeEvent<u64>) -> Result<Update<u64>, Infallible> {
            if let RuntimeEvent::Message(value) = event {
                if value == 99 {
                    return Ok(Update::exit());
                }
                self.value = value;
                self.observed.send(value).expect("observer");
                return Ok(Update::redraw());
            }
            Ok(Update::none())
        }
    }
    fn paint(app: &mut App, cx: &mut PaintCx<'_, '_>) {
        cx.fill(
            bmux_tui::paint::LocalRect::new(0, 0, 2, 1),
            "x",
            bmux_tui::style::Style::new(),
        );
        cx.push_hit(
            bmux_tui::hit::HitRegion::new(app.value.to_string(), Rect::new(0, 0, 2, 1))
                .focusable(true),
        );
    }
    async fn started(count: &AtomicUsize) {
        while count.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn actual_image_delivery_keeps_input_live_and_commits_exact_snapshot_without_backlog() {
        let release = Arc::new(AtomicBool::new(false));
        let count = Arc::new(AtomicUsize::new(0));
        let alive = Arc::new(AtomicUsize::new(0));
        let (observed, mut observations) = tokio::sync::mpsc::unbounded_channel();
        let (committed, mut commits) = tokio::sync::mpsc::unbounded_channel();
        let inner = ImageTerminalPresenter::new(
            Terminal::new(Vec::new(), Rect::new(0, 0, 2, 1)),
            paint,
            HostImageCapabilities::default(),
            ImageConfig::default(),
        );
        let presenter = AsyncImageTerminalPresenter::new(
            inner,
            Gate {
                release: Arc::clone(&release),
                started: Arc::clone(&count),
                alive: Arc::clone(&alive),
                fail: false,
                fail_write: false,
            },
            |app: &mut App| app.value,
            move |_: &mut App, token: u64, hits: &HitMap, _: &FocusTrap| {
                assert_eq!(hits.regions()[0].id.as_str(), token.to_string());
                committed.send(token).expect("commit observer");
            },
        );
        let (runtime, handle) = Runtime::new(
            App { value: 0, observed },
            presenter,
            RuntimeConfig {
                frame_interval: None,
                ..RuntimeConfig::default()
            },
        );
        let drive = async {
            started(&count).await;
            for value in [1, 2, 2] {
                handle.send(value).await.expect("input");
            }
            for value in [1, 2, 2] {
                assert_eq!(observations.recv().await, Some(value));
            }
            assert!(
                commits.try_recv().is_err(),
                "blocked flush must not publish scene"
            );
            assert_eq!(count.load(Ordering::SeqCst), 1, "no frame backlog");
            release.store(true, Ordering::SeqCst);
            assert_eq!(
                commits.recv().await,
                Some(0),
                "ack exact submitted candidate, not latest value"
            );
            assert_eq!(commits.recv().await, Some(2), "coalesce to latest frame");
            handle.send(99).await.expect("quit");
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(Box::pin(runtime.run()), drive)
        })
        .await
        .expect("bounded runtime");
        assert!(result.is_ok());
        assert_eq!(alive.load(Ordering::SeqCst), 0);
        assert!(count.load(Ordering::SeqCst) <= 3);
    }

    #[tokio::test]
    async fn actual_image_failed_flush_preserves_committed_scene_and_drops_candidate() {
        let (observed, _) = tokio::sync::mpsc::unbounded_channel();
        let commits = Arc::new(AtomicUsize::new(0));
        let committed = Arc::clone(&commits);
        let inner = ImageTerminalPresenter::new(
            Terminal::new(Vec::new(), Rect::new(0, 0, 2, 1)),
            paint,
            HostImageCapabilities::default(),
            ImageConfig::default(),
        );
        let mut presenter = AsyncImageTerminalPresenter::new(
            inner,
            Gate {
                release: Arc::new(AtomicBool::new(true)),
                started: Arc::new(AtomicUsize::new(0)),
                alive: Arc::new(AtomicUsize::new(0)),
                fail: false,
                fail_write: false,
            },
            |app: &mut App| app.value,
            move |_: &mut App, _: u64, _: &HitMap, _: &FocusTrap| {
                committed.fetch_add(1, Ordering::SeqCst);
            },
        );
        let mut app = App { value: 1, observed };
        assert!(presenter.start_present(&mut app).unwrap().is_none());
        std::future::poll_fn(|cx| presenter.poll_present(cx, &mut app))
            .await
            .unwrap();
        assert_eq!(presenter.terminal().hits().regions()[0].id.as_str(), "1");
        app.value = 2;
        presenter.output.as_mut().unwrap().fail = true;
        assert!(presenter.start_present(&mut app).unwrap().is_none());
        assert!(
            std::future::poll_fn(|cx| presenter.poll_present(cx, &mut app))
                .await
                .is_err()
        );
        assert_eq!(presenter.terminal().hits().regions()[0].id.as_str(), "1");
        assert_eq!(commits.load(Ordering::SeqCst), 1);
        assert!(presenter.terminal().retained_buffer().is_none());
        presenter.output.as_mut().unwrap().fail = false;
        presenter.output.as_mut().unwrap().fail_write = true;
        assert!(presenter.start_present(&mut app).unwrap().is_none());
        assert!(
            std::future::poll_fn(|cx| presenter.poll_present(cx, &mut app))
                .await
                .is_err()
        );
        assert_eq!(presenter.terminal().hits().regions()[0].id.as_str(), "1");
        assert_eq!(commits.load(Ordering::SeqCst), 1);
        presenter.output.as_mut().unwrap().fail_write = false;
        presenter
            .output
            .as_mut()
            .unwrap()
            .release
            .store(false, Ordering::SeqCst);
        assert!(
            presenter
                .cleanup_images(Duration::from_millis(10))
                .await
                .is_err()
        );
        assert!(
            presenter.output.is_some(),
            "cleanup deadline reclaims output owner"
        );
    }

    #[tokio::test]
    async fn partial_kitty_output_is_terminated_before_cleanup_or_restoration() {
        struct PartialOutput {
            bytes: Arc<std::sync::Mutex<Vec<u8>>>,
            first: bool,
        }
        impl crate::CancellableOutput for PartialOutput {
            fn deliver(&mut self, bytes: &[u8], _: &AtomicBool) -> io::Result<()> {
                let mut output = self.bytes.lock().unwrap();
                if self.first {
                    self.first = false;
                    output.extend_from_slice(b"\x1b[?2026h\x1b_Ga=T,i=1;YWJj");
                    drop(output);
                    return Err(io::Error::other("partial Kitty APC"));
                }
                output.extend_from_slice(bytes);
                drop(output);
                Ok(())
            }
        }
        let bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (observed, _) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App { value: 0, observed };
        let inner = ImageTerminalPresenter::new(
            Terminal::new(Vec::new(), Rect::new(0, 0, 2, 1)),
            paint,
            HostImageCapabilities::default(),
            ImageConfig::default(),
        );
        let mut presenter = AsyncImageTerminalPresenter::new(
            inner,
            PartialOutput {
                bytes: Arc::clone(&bytes),
                first: true,
            },
            |app: &mut App| app.value,
            |_: &mut App, _: u64, _: &HitMap, _: &FocusTrap| {},
        );
        presenter.start_present(&mut app).unwrap();
        assert!(
            std::future::poll_fn(|cx| presenter.poll_present(cx, &mut app))
                .await
                .is_err()
        );
        let boundary = bytes.lock().unwrap().len();
        presenter.start_present(&mut app).unwrap();
        std::future::poll_fn(|cx| presenter.poll_present(cx, &mut app))
            .await
            .unwrap();
        assert!(
            bytes.lock().unwrap()[boundary..].starts_with(b"\x18\x1b\\\x1b[?2026l"),
            "retry must repair partial APC before ordinary frame bytes"
        );
        let boundary = bytes.lock().unwrap().len();
        presenter
            .cleanup_images(Duration::from_secs(1))
            .await
            .unwrap();
        let mut output = bytes.lock().unwrap();
        assert!(output[boundary..].starts_with(b"\x18\x1b\\\x1b[?2026l"));
        // Restoration is appended only after successful parser repair delivery.
        output.extend_from_slice(b"\x1b[?1049l");
        let mut in_apc = false;
        let mut synchronized = false;
        let mut index = 0;
        while index < output.len() {
            if output[index] == 0x18 {
                in_apc = false;
                index += 1;
            } else if output[index..].starts_with(b"\x1b\\") {
                in_apc = false;
                index += 2;
            } else if in_apc {
                index += 1;
            } else if output[index..].starts_with(b"\x1b_G") {
                in_apc = true;
                index += 3;
            } else if output[index..].starts_with(b"\x1b[?2026h") {
                synchronized = true;
                index += 8;
            } else if output[index..].starts_with(b"\x1b[?2026l") {
                synchronized = false;
                index += 8;
            } else if output[index..].starts_with(b"\x1b[?1049l") {
                assert!(!synchronized);
                index += 8;
            } else {
                index += 1;
            }
        }
        assert!(!in_apc);
        assert!(!synchronized);
        drop(output);
    }

    #[cfg(all(target_os = "linux", feature = "image-kitty", feature = "crossterm"))]
    #[tokio::test]
    #[expect(
        clippy::too_many_lines,
        reason = "single PTY owns probe/upload/timeout teardown sequence end to end"
    )]
    #[expect(
        clippy::items_after_statements,
        reason = "fixture-local PTY writer and parsers stay beside their setup and never escape test"
    )]
    async fn pty_raw_managed_probe_ack_upload_pixels_timeout_and_cleanup() {
        use bmux_tui::crossterm::input::{LinuxInputPacket, RawInput};
        use bmux_tui::image::*;
        use std::io::Read;
        use std::os::fd::{AsRawFd, FromRawFd};
        #[link(name = "util")]
        unsafe extern "C" {
            fn openpty(
                master: *mut i32,
                slave: *mut i32,
                name: *mut u8,
                termios: *const std::ffi::c_void,
                size: *const std::ffi::c_void,
            ) -> i32;
            fn fcntl(fd: i32, command: i32, ...) -> i32;
        }
        let mut master = -1;
        let mut slave = -1;
        // SAFETY: valid descriptor outputs; optional configuration is omitted.
        assert_eq!(
            unsafe {
                openpty(
                    &raw mut master,
                    &raw mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0
        );
        // SAFETY: openpty returned distinct owned descriptors.
        let mut master = unsafe { std::fs::File::from_raw_fd(master) };
        // SAFETY: openpty returned an owned slave descriptor.
        let slave = unsafe { std::fs::File::from_raw_fd(slave) };
        let slave_path = format!("/proc/{}/fd/{}", std::process::id(), slave.as_raw_fd());
        assert!(
            std::process::Command::new("stty")
                .args(["-F", &slave_path, "raw", "-echo"])
                .status()
                .unwrap()
                .success()
        );
        // SAFETY: live Linux PTY descriptors; F_SETFL=4, O_NONBLOCK=2048.
        assert_eq!(unsafe { fcntl(slave.as_raw_fd(), 4, 2048) }, 0);
        // SAFETY: live owned master descriptor.
        assert_eq!(unsafe { fcntl(master.as_raw_fd(), 4, 2048) }, 0);
        struct PtyOutput(std::fs::File);
        impl crate::CancellableOutput for PtyOutput {
            fn deliver(&mut self, bytes: &[u8], cancelled: &AtomicBool) -> io::Result<()> {
                let mut rest = bytes;
                while !rest.is_empty() {
                    if cancelled.load(Ordering::Acquire) {
                        return Err(io::Error::new(io::ErrorKind::Interrupted, "cancel"));
                    }
                    match self.0.write(rest) {
                        Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                        Ok(count) => rest = &rest[count..],
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Err(error) => return Err(error),
                    }
                }
                Ok(())
            }
        }
        fn paint_image((): &mut (), cx: &mut PaintCx<'_, '_>) {
            cx.push_image(ImageContribution::Present(ImagePlacement {
                key: "fixture".into(),
                payload: ImagePayload::Pixels {
                    bytes: [17, 34, 51, 255].repeat(4),
                    width: 2,
                    height: 2,
                    format: ImagePixelFormat::Rgba8,
                },
                destination: Rect::new(0, 0, 2, 2),
                clip: Rect::new(0, 0, 2, 2),
                fit: ImageFit::Stretch,
                allocation: None,
                lifecycle: ImageLifecycle::Frame,
            }));
        }
        fn read_wire(master: &mut std::fs::File) -> Vec<u8> {
            let mut wire = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                match master.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => wire.extend_from_slice(&buffer[..count]),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => panic!("PTY read: {error}"),
                }
            }
            wire
        }
        fn command_field<'a>(wire: &'a str, marker: &str) -> &'a str {
            wire.split_once(marker)
                .unwrap()
                .1
                .split([',', ';'])
                .next()
                .unwrap()
        }
        fn shm_path(command: &str) -> std::path::PathBuf {
            let encoded = command
                .split_once(';')
                .unwrap()
                .1
                .split("\x1b\\")
                .next()
                .unwrap();
            let name =
                String::from_utf8(bmux_image::codec::kitty::base64_decode(encoded.as_bytes()))
                    .unwrap();
            std::path::Path::new("/dev/shm").join(name.trim_start_matches('/'))
        }
        let writer = slave.try_clone().unwrap();
        let mut input =
            crate::input::ManagedRawInput::from_reader(RawInput::from_owned_tty(slave).unwrap(), 4);
        let caps = HostImageCapabilities {
            kitty_graphics: true,
            cell_pixel_width: 1,
            cell_pixel_height: 1,
            ..HostImageCapabilities::default()
        };
        let inner = ImageTerminalPresenter::new(
            Terminal::new(Vec::new(), Rect::new(0, 0, 2, 2)),
            paint_image,
            caps,
            ImageConfig::default(),
        );
        let mut presenter = AsyncImageTerminalPresenter::new(
            inner,
            PtyOutput(writer),
            |(): &mut ()| (),
            |(): &mut (), (), _: &HitMap, _: &FocusTrap| {},
        );
        let now = std::time::Instant::now();
        assert!(presenter.begin_shared_memory_probe(now).await.unwrap());
        let probe = String::from_utf8(read_wire(&mut master)).unwrap();
        let probe_path = shm_path(probe.split_once("\x1b_G").unwrap().1);
        assert!(probe_path.exists());
        let id = command_field(&probe, ",i=");
        let ack = format!("\x1b_Gi={id};OK\x1b\\");
        master.write_all(&ack.as_bytes()[..5]).unwrap();
        tokio::task::yield_now().await;
        master.write_all(&ack.as_bytes()[5..]).unwrap();
        master.write_all(b"\x1b_Gi=0;unmatched\x1b\\q").unwrap();
        let unmatched = crate::input::OpaqueInputQueue::default();
        let mut accepted = false;
        let mut key = false;
        tokio::time::timeout(Duration::from_secs(2), async {
            while !accepted || !key {
                match input.recv().await.unwrap().unwrap() {
                    LinuxInputPacket::Opaque(frame) => {
                        if presenter.handle_shared_memory_reply(&frame, now) {
                            accepted = true;
                        } else {
                            unmatched.push(frame).unwrap();
                        }
                    }
                    LinuxInputPacket::Event(bmux_tui::event::Event::Key(stroke)) => {
                        assert_eq!(stroke.key, bmux_keyboard::KeyCode::Char('q'));
                        key = true;
                    }
                    LinuxInputPacket::Event(bmux_tui::event::Event::Resize(_)) => {}
                    other => panic!("unexpected packet: {other:?}"),
                }
            }
        })
        .await
        .unwrap();
        assert!(!probe_path.exists());
        assert_eq!(
            unmatched.take(),
            Some(b"\x1b_Gi=0;unmatched\x1b\\".to_vec())
        );
        presenter.start_present(&mut ()).unwrap();
        std::future::poll_fn(|cx| presenter.poll_present(cx, &mut ()))
            .await
            .unwrap();
        let upload = String::from_utf8(read_wire(&mut master)).unwrap();
        let upload_command = upload
            .split_once("a=t,t=s")
            .expect("ACK selected SHM upload")
            .1;
        let lease = shm_path(upload_command);
        assert_eq!(std::fs::read(&lease).unwrap(), [17, 34, 51, 255].repeat(4));
        presenter
            .cleanup_images(Duration::from_secs(1))
            .await
            .unwrap();
        let cleanup = read_wire(&mut master);
        assert!(cleanup.starts_with(b"\x18\x1b\\\x1b[?2026l"));
        assert!(!lease.exists());
        input.shutdown().await;
        // A fresh connection without an ACK remains direct after expiry.
        let inner = ImageTerminalPresenter::new(
            Terminal::new(Vec::new(), Rect::new(0, 0, 2, 2)),
            paint_image,
            HostImageCapabilities {
                kitty_graphics: true,
                cell_pixel_width: 1,
                cell_pixel_height: 1,
                ..HostImageCapabilities::default()
            },
            ImageConfig::default(),
        );
        let mut fallback = AsyncImageTerminalPresenter::new(
            inner,
            PtyOutput(presenter.output.take().unwrap().0),
            |(): &mut ()| (),
            |(): &mut (), (), _: &HitMap, _: &FocusTrap| {},
        );
        assert!(fallback.begin_shared_memory_probe(now).await.unwrap());
        let late_probe = String::from_utf8(read_wire(&mut master)).unwrap();
        let late_id = command_field(&late_probe, ",i=");
        assert!(!fallback.handle_shared_memory_reply(
            format!("\x1b_Gi={late_id};OK\x1b\\").as_bytes(),
            now + Duration::from_secs(10)
        ));
        fallback.start_present(&mut ()).unwrap();
        std::future::poll_fn(|cx| fallback.poll_present(cx, &mut ()))
            .await
            .unwrap();
        let direct = String::from_utf8(read_wire(&mut master)).unwrap();
        assert!(direct.contains("a=t"));
        assert!(!direct.contains("t=s"));
        fallback
            .cleanup_images(Duration::from_secs(1))
            .await
            .unwrap();
    }

    #[cfg(all(target_os = "linux", feature = "image-kitty"))]
    #[tokio::test]
    async fn actual_output_probe_ack_is_connection_local_and_late_ack_falls_back() {
        struct Recorded(Arc<std::sync::Mutex<Vec<u8>>>);
        impl crate::CancellableOutput for Recorded {
            fn deliver(&mut self, bytes: &[u8], _: &AtomicBool) -> io::Result<()> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(())
            }
        }
        let recorded = Arc::new(std::sync::Mutex::new(Vec::new()));
        let inner = ImageTerminalPresenter::new(
            Terminal::new(Vec::new(), Rect::new(0, 0, 2, 1)),
            paint,
            HostImageCapabilities::default(),
            ImageConfig::default(),
        );
        let mut presenter = AsyncImageTerminalPresenter::<_, _, _, u64, _, _>::new(
            inner,
            Recorded(Arc::clone(&recorded)),
            |app: &mut App| app.value,
            |_: &mut App, _: u64, _: &HitMap, _: &FocusTrap| {},
        );
        let now = std::time::Instant::now();
        assert!(presenter.begin_shared_memory_probe(now).await.unwrap());
        let bytes = recorded.lock().unwrap().clone();
        let command = String::from_utf8(bytes).unwrap();
        let id = command
            .split_once(",i=")
            .unwrap()
            .1
            .split(',')
            .next()
            .unwrap();
        let ack = format!("\x1b_Gi={id};OK\x1b\\");
        assert!(!presenter.handle_shared_memory_reply(b"\x1b_Gi=0;OK\x1b\\", now));
        assert!(presenter.handle_shared_memory_reply(ack.as_bytes(), now));
        let inner = ImageTerminalPresenter::new(
            Terminal::new(Vec::new(), Rect::new(0, 0, 2, 1)),
            paint,
            HostImageCapabilities::default(),
            ImageConfig::default(),
        );
        let mut other = AsyncImageTerminalPresenter::<_, _, _, u64, _, _>::new(
            inner,
            Recorded(Arc::new(std::sync::Mutex::new(Vec::new()))),
            |app: &mut App| app.value,
            |_: &mut App, _: u64, _: &HitMap, _: &FocusTrap| {},
        );
        assert!(!other.handle_shared_memory_reply(ack.as_bytes(), now));
        assert!(other.begin_shared_memory_probe(now).await.unwrap());
        assert!(!other.handle_shared_memory_reply(ack.as_bytes(), now + Duration::from_secs(10)));
        presenter
            .cleanup_images(Duration::from_secs(1))
            .await
            .unwrap();
        other.cleanup_images(Duration::from_secs(1)).await.unwrap();
    }

    #[cfg(feature = "image-kitty")]
    #[tokio::test]
    async fn actual_midcodec_token_cancellation_reclaims_and_joins_owned_worker() {
        struct EncodingOutput {
            checkpoint: std::sync::mpsc::Sender<()>,
            exited: Arc<AtomicBool>,
            interrupted: Arc<AtomicBool>,
        }
        impl crate::CancellableOutput for EncodingOutput {
            fn deliver(&mut self, _: &[u8], _: &AtomicBool) -> io::Result<()> {
                panic!("owned codec path required")
            }
            fn deliver_owned(&mut self, _: &[u8], token: Arc<AtomicBool>) -> io::Result<()> {
                let mut checks = 0;
                let encoded = bmux_image::codec::kitty::encode_transmit_chunks_cancellable(
                    17,
                    bmux_image::model::KittyFormat::Rgba,
                    &[73; 12_288],
                    64,
                    48,
                    || {
                        checks += 1;
                        // The first 3072-byte base64 chunk has actually encoded.
                        // Pause inside the codec, not at the output writer.
                        if checks == 2 {
                            self.checkpoint.send(()).unwrap();
                            while !token.load(Ordering::Acquire) {
                                std::thread::yield_now();
                            }
                        }
                        token.load(Ordering::Acquire)
                    },
                );
                self.interrupted.store(
                    encoded
                        .as_ref()
                        .is_err_and(|error| error.kind() == io::ErrorKind::Interrupted),
                    Ordering::Release,
                );
                self.exited.store(true, Ordering::Release);
                // No bytes can reach output or presentation authority on failure.
                encoded.map(|_| panic!("cancelled codec must not publish encoded bytes"))
            }
        }
        let (checkpoint, observed) = std::sync::mpsc::channel();
        let exited = Arc::new(AtomicBool::new(false));
        let interrupted = Arc::new(AtomicBool::new(false));
        let mut output = Some(EncodingOutput {
            checkpoint,
            exited: Arc::clone(&exited),
            interrupted: Arc::clone(&interrupted),
        });
        let mut delivery = crate::presenter::Delivery::start(&mut output, Vec::new()).unwrap();
        observed.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(!exited.load(Ordering::Acquire));
        let _reclaimed = delivery.reclaim().unwrap();
        assert!(
            exited.load(Ordering::Acquire),
            "reclaim joined the codec worker"
        );
        assert!(interrupted.load(Ordering::Acquire));
        let error = std::future::poll_fn(|cx| delivery.poll(cx))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }

    #[tokio::test]
    async fn owned_frame_encoding_runs_on_output_worker_not_input_thread() {
        struct ThreadOutput(std::thread::ThreadId);
        impl crate::CancellableOutput for ThreadOutput {
            fn deliver(&mut self, bytes: &[u8], _: &AtomicBool) -> io::Result<()> {
                assert_ne!(std::thread::current().id(), self.0);
                assert!(bytes.starts_with(b"\x18\x1b\\\x1b[?2026l"));
                Ok(())
            }
        }
        let terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 2, 1));
        let prepared = terminal.prepare_frame(bmux_tui::damage::Damage::Full, |_| {});
        assert!(terminal.retained_buffer().is_none());
        let mut frame = Some(FrameOutput {
            output: ThreadOutput(std::thread::current().id()),
            terminal,
            compositor: TuiImageCompositor::default(),
            prepared,
            capabilities: HostImageCapabilities::default(),
            config: ImageConfig::default(),
            report: None,
        });
        let mut delivery = crate::presenter::Delivery::start(&mut frame, Vec::new()).unwrap();
        std::future::poll_fn(|cx| delivery.poll(cx)).await.unwrap();
        let completed = delivery.reclaim().unwrap();
        assert!(completed.report.is_some());
        assert!(completed.terminal.retained_buffer().is_some());
    }

    #[tokio::test]
    async fn actual_image_quit_cancels_gated_flush_and_joins_worker() {
        image_quit_cleanup(true).await;
    }

    #[tokio::test]
    async fn image_quit_reports_blocked_cleanup_without_hanging_or_detaching() {
        image_quit_cleanup(false).await;
    }

    async fn image_quit_cleanup(unblock_cleanup: bool) {
        let release = Arc::new(AtomicBool::new(false));
        let count = Arc::new(AtomicUsize::new(0));
        let alive = Arc::new(AtomicUsize::new(0));
        let (observed, _) = tokio::sync::mpsc::unbounded_channel();
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 2, 1));
        // This regression isolates image cancellation and restoration; pointer
        // stack failure behavior has its own explicit-capability coverage.
        terminal.set_pointer_capability(
            bmux_terminal_protocol::pointer_output::PointerShapeCapability::Unsupported,
        );
        let inner = ImageTerminalPresenter::new(
            terminal,
            paint,
            HostImageCapabilities::default(),
            ImageConfig::default(),
        );
        let presenter = AsyncImageTerminalPresenter::new(
            inner,
            Gate {
                release: Arc::clone(&release),
                started: Arc::clone(&count),
                alive: Arc::clone(&alive),
                fail: false,
                fail_write: false,
            },
            |app: &mut App| app.value,
            |_: &mut App, _: u64, _: &HitMap, _: &FocusTrap| panic!("cancelled frame committed"),
        );
        let (runtime, handle) = Runtime::new(
            App { value: 0, observed },
            presenter,
            RuntimeConfig::default(),
        );
        let drive = async {
            started(&count).await;
            handle.send(99).await.expect("quit");
            if unblock_cleanup {
                // The cancelled frame's gate remains closed. Open only after
                // the restoration transaction has acquired the reclaimed owner.
                while count.load(Ordering::SeqCst) < 2 {
                    tokio::task::yield_now().await;
                }
                release.store(true, Ordering::SeqCst);
            }
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(Box::pin(runtime.run()), drive)
        })
        .await
        .expect("bounded cancellation");
        if unblock_cleanup {
            assert!(result.is_ok());
        } else {
            let Err(crate::RuntimeError::Presenter {
                error,
                cleanup_error,
                ..
            }) = &result
            else {
                panic!("blocked restoration must be reported");
            };
            assert!(matches!(error, super::ImagePresentationError::Io(error)
                if error.kind() == io::ErrorKind::TimedOut));
            assert!(cleanup_error.is_none());
        }
        assert_eq!(alive.load(Ordering::SeqCst), 0);
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }
}

#[cfg(test)]
mod prepared_tests {
    use super::ImageTerminalPresenter;
    use crate::presenter::Presenter;
    use bmux_tui::focus::{FocusScopeId, FocusTrap};
    use bmux_tui::geometry::Rect;
    use bmux_tui::hit::{HitMap, HitRegion};
    use bmux_tui::paint::PaintCx;
    use bmux_tui::presentation::{PreparedPresentation, PresentationLimits};
    use bmux_tui::terminal::Terminal;
    use std::io::{self, Write};

    #[derive(Default)]
    struct Writer {
        bytes: Vec<u8>,
        fail_flush: bool,
    }
    impl Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            if self.fail_flush {
                Err(io::Error::other("flush failed"))
            } else {
                Ok(())
            }
        }
    }
    #[derive(Default)]
    struct Program {
        scopes: Vec<FocusScopeId>,
        commits: usize,
        reject: bool,
    }
    fn render(program: &mut Program, cx: &mut PaintCx<'_, '_>) {
        cx.push_hit(HitRegion::new("background", Rect::new(0, 0, 4, 2)).focusable(true));
        for scope in &program.scopes {
            cx.set_focus_scope(Some(scope.clone()));
            cx.push_hit(HitRegion::new(scope.clone(), Rect::new(1, 0, 2, 1)).focusable(true));
        }
    }
    fn configure(program: &mut Program, prepared: &mut PreparedPresentation) -> io::Result<()> {
        if program.reject || !prepared.set_modal_scopes(&program.scopes) {
            return Err(io::Error::other("configuration rejected"));
        }
        prepared
            .validate_limits(PresentationLimits {
                max_cells: 8,
                max_entries: 128,
                max_bytes: 8192,
            })
            .map_err(io::Error::other)
    }
    fn commit(program: &mut Program, hits: &HitMap, focus: &FocusTrap) {
        program.commits += 1;
        assert_eq!(focus.active_scope(), program.scopes.last());
        if let Some(scope) = program.scopes.last() {
            assert_eq!(focus.active(), Some(scope));
            assert_eq!(hits.focus_targets(None), []);
        }
    }
    #[test]
    fn configured_scopes_commit_only_after_successful_flush() {
        let mut presenter = ImageTerminalPresenter::with_commit(
            Terminal::new(Writer::default(), Rect::new(0, 0, 4, 2)),
            render,
            commit,
            bmux_image::HostImageCapabilities::default(),
            bmux_image::ImageConfig::default(),
        )
        .configure_prepared(configure);
        let mut program = Program::default();
        presenter.present(&mut program).unwrap();
        program.scopes.push(FocusScopeId::new("modal"));
        presenter.terminal_mut().writer_mut().fail_flush = true;
        assert!(presenter.present(&mut program).is_err());
        assert_eq!(program.commits, 1);
        assert!(presenter.terminal().focus().active_scope().is_none());
        presenter.terminal_mut().writer_mut().fail_flush = false;
        presenter.present(&mut program).unwrap();
        assert_eq!(program.commits, 2);
        let written = presenter.terminal().writer().bytes.len();
        program.reject = true;
        assert!(presenter.present(&mut program).is_err());
        assert_eq!(presenter.terminal().writer().bytes.len(), written);
        assert_eq!(program.commits, 2);
        assert_eq!(
            presenter.terminal().focus().active_scope(),
            program.scopes.last()
        );
    }
    #[tokio::test]
    async fn image_pointer_lifecycle_delegates_through_context_adapter() {
        use crate::presenter::PointerContextPresenter;
        use bmux_terminal_protocol::pointer_output::PointerShapeCapability;
        let mut terminal = Terminal::new(Writer::default(), Rect::new(0, 0, 4, 2));
        terminal.set_pointer_capability(PointerShapeCapability::Stack);
        let inner = ImageTerminalPresenter::new(
            terminal,
            render,
            bmux_image::HostImageCapabilities::default(),
            bmux_image::ImageConfig::default(),
        );
        let mut presenter = PointerContextPresenter::new(inner, |_: &Program| {
            bmux_tui::terminal::PointerContext::default()
        });
        assert!(
            presenter.observe_terminal_event(&bmux_tui::event::Event::Mouse(
                bmux_tui::event::MouseEvent::new(
                    bmux_tui::event::MouseEventKind::Move,
                    bmux_tui::geometry::Point::new(0, 0),
                ),
            ))
        );
        presenter.present(&mut Program::default()).unwrap();
        let mut expected_pop = Vec::new();
        bmux_terminal_protocol::pointer_shape::encode_pointer_shape_pop(&mut expected_pop);
        presenter
            .inner_mut()
            .terminal_mut()
            .writer_mut()
            .bytes
            .clear();
        <_ as Presenter<Program>>::restore(&mut presenter)
            .await
            .unwrap();
        assert!(
            presenter
                .inner_mut()
                .terminal()
                .writer()
                .bytes
                .windows(expected_pop.len())
                .any(|bytes| bytes == expected_pop)
        );
        presenter
            .inner_mut()
            .terminal_mut()
            .writer_mut()
            .bytes
            .clear();
        <_ as Presenter<Program>>::restore(&mut presenter)
            .await
            .unwrap();
        assert!(
            !presenter
                .inner_mut()
                .terminal()
                .writer()
                .bytes
                .windows(expected_pop.len())
                .any(|bytes| bytes == expected_pop)
        );
    }

    #[test]
    fn prepared_configuration_also_supports_noop_commit_constructor() {
        let mut presenter = ImageTerminalPresenter::new(
            Terminal::new(Writer::default(), Rect::new(0, 0, 4, 2)),
            render,
            bmux_image::HostImageCapabilities::default(),
            bmux_image::ImageConfig::default(),
        )
        .configure_prepared(configure);
        let mut program = Program {
            scopes: vec![FocusScopeId::new("modal")],
            ..Program::default()
        };
        presenter.present(&mut program).unwrap();
        assert_eq!(
            presenter.terminal().focus().active_scope(),
            program.scopes.last()
        );
    }
}

#[cfg(all(test, feature = "image-kitty"))]
mod tests {
    use bmux_tui::geometry::Rect;
    use bmux_tui::image::{
        ImageContribution, ImageKey, ImageLifecycle, ImagePayload, ImagePixelFormat, ImagePlacement,
    };

    use super::ImageTerminalPresenter;
    use crate::presenter::{Presenter, ResetReason};

    fn capabilities() -> bmux_image::HostImageCapabilities {
        bmux_image::HostImageCapabilities {
            kitty_graphics: true,
            ..bmux_image::HostImageCapabilities::default()
        }
    }

    #[cfg(all(feature = "image-sixel", feature = "image-iterm2"))]
    #[test]
    fn raster_lifecycle_clears_before_complete_repaint() {
        use bmux_image::HostImageCapabilities;
        use bmux_tui::paint::PaintCx;
        use bmux_tui::terminal::Terminal;
        for caps in [
            HostImageCapabilities {
                sixel: true,
                ..Default::default()
            },
            HostImageCapabilities {
                iterm2_inline: true,
                ..Default::default()
            },
        ] {
            let terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 4, 2));
            let render = |visible: &mut bool, frame: &mut PaintCx<'_, '_>| {
                if *visible {
                    frame.push_image(ImageContribution::Present(ImagePlacement {
                        fit: bmux_tui::image::ImageFit::Stretch,
                        allocation: None,
                        key: ImageKey::new("raster"),
                        payload: ImagePayload::Pixels {
                            bytes: vec![255, 0, 0, 255],
                            width: 1,
                            height: 1,
                            format: ImagePixelFormat::Rgba8,
                        },
                        destination: Rect::new(0, 0, 1, 1),
                        clip: frame.clip(),
                        lifecycle: ImageLifecycle::Frame,
                    }));
                }
            };
            let mut presenter = ImageTerminalPresenter::new(
                terminal,
                render,
                caps,
                bmux_image::ImageConfig::default(),
            );
            let mut visible = true;
            presenter.present(&mut visible).unwrap();
            for next in [true, false, true, false, true] {
                let before = presenter.terminal().writer().len();
                let had_image = visible;
                visible = next;
                let report = presenter.present(&mut visible).unwrap();
                let output = &presenter.terminal().writer()[before..];
                assert_eq!(
                    output.windows(4).any(|bytes| bytes == b"\x1b[2J"),
                    had_image
                );
                if had_image {
                    assert_eq!(report.changed_cells, 8);
                }
            }
            let before = presenter.terminal().writer().len();
            presenter.cleanup_images().unwrap();
            assert!(
                presenter.terminal().writer()[before..]
                    .windows(4)
                    .any(|bytes| bytes == b"\x1b[2J")
            );
        }
    }

    #[test]
    fn reset_resize_and_cleanup_cover_image_lifecycle_handoffs() {
        let terminal = bmux_tui::terminal::Terminal::new(Vec::new(), Rect::new(0, 0, 4, 2));
        let render = |(): &mut (), frame: &mut bmux_tui::paint::PaintCx<'_, '_>| {
            frame.push_image(ImageContribution::Present(ImagePlacement {
                fit: bmux_tui::image::ImageFit::Stretch,
                allocation: None,
                key: ImageKey::new("image"),
                payload: ImagePayload::Pixels {
                    bytes: vec![1, 2, 3, 4],
                    width: 1,
                    height: 1,
                    format: ImagePixelFormat::Rgba8,
                },
                destination: Rect::new(0, 0, 1, 1),
                clip: frame.clip(),
                lifecycle: ImageLifecycle::Frame,
            }));
        };
        let mut presenter = ImageTerminalPresenter::new(
            terminal,
            render,
            capabilities(),
            bmux_image::ImageConfig::default(),
        );
        presenter.present(&mut ()).expect("initial presentation");
        presenter.resize(bmux_tui::geometry::Size::new(8, 4));
        presenter.reset(ResetReason::Resize);
        presenter.present(&mut ()).expect("resize presentation");
        let before = presenter.terminal().writer().len();

        presenter.reset_presentation().expect("full reset cleanup");

        let output = String::from_utf8(presenter.terminal().writer()[before..].to_vec())
            .expect("terminal output");
        assert!(output.contains("a=d"));
        assert_eq!(presenter.terminal().area(), Rect::new(0, 0, 8, 4));
    }

    #[test]
    fn consuming_presenter_cleans_images_before_returning_terminal() {
        let terminal = bmux_tui::terminal::Terminal::new(Vec::new(), Rect::new(0, 0, 4, 2));
        let render = |(): &mut (), frame: &mut bmux_tui::paint::PaintCx<'_, '_>| {
            frame.push_image(ImageContribution::Present(ImagePlacement {
                fit: bmux_tui::image::ImageFit::Stretch,
                allocation: None,
                key: ImageKey::new("image"),
                payload: ImagePayload::Pixels {
                    bytes: vec![1, 2, 3, 4],
                    width: 1,
                    height: 1,
                    format: ImagePixelFormat::Rgba8,
                },
                destination: Rect::new(0, 0, 1, 1),
                clip: frame.clip(),
                lifecycle: ImageLifecycle::Frame,
            }));
        };
        let mut presenter = ImageTerminalPresenter::new(
            terminal,
            render,
            capabilities(),
            bmux_image::ImageConfig::default(),
        );
        presenter.present(&mut ()).expect("initial presentation");

        let terminal = presenter.into_clean_terminal().expect("clean handoff");
        let output = String::from_utf8(terminal.writer().clone()).expect("terminal output");
        assert!(output.contains("a=d"));
    }

    #[test]
    fn cleanup_removes_host_images_before_terminal_handoff() {
        let terminal = bmux_tui::terminal::Terminal::new(Vec::new(), Rect::new(0, 0, 4, 2));
        let render = |(): &mut (), frame: &mut bmux_tui::paint::PaintCx<'_, '_>| {
            frame.push_image(ImageContribution::Present(ImagePlacement {
                fit: bmux_tui::image::ImageFit::Stretch,
                allocation: None,
                key: ImageKey::new("image"),
                payload: ImagePayload::Pixels {
                    bytes: vec![1, 2, 3, 4],
                    width: 1,
                    height: 1,
                    format: ImagePixelFormat::Rgba8,
                },
                destination: Rect::new(0, 0, 1, 1),
                clip: frame.clip(),
                lifecycle: ImageLifecycle::Frame,
            }));
        };
        let mut presenter = ImageTerminalPresenter::new(
            terminal,
            render,
            capabilities(),
            bmux_image::ImageConfig::default(),
        );
        presenter.present(&mut ()).expect("initial presentation");
        let before = presenter.terminal().writer().len();

        presenter.cleanup_images().expect("image cleanup");

        let output = String::from_utf8(presenter.terminal().writer()[before..].to_vec())
            .expect("terminal output");
        assert!(output.contains("a=d"));
    }

    #[test]
    fn reset_removes_old_host_image_before_retransmission() {
        let terminal = bmux_tui::terminal::Terminal::new(Vec::new(), Rect::new(0, 0, 4, 2));
        let render = |(): &mut (), frame: &mut bmux_tui::paint::PaintCx<'_, '_>| {
            frame.push_image(ImageContribution::Present(ImagePlacement {
                fit: bmux_tui::image::ImageFit::Stretch,
                allocation: None,
                key: ImageKey::new("image"),
                payload: ImagePayload::Pixels {
                    bytes: vec![1, 2, 3, 4],
                    width: 1,
                    height: 1,
                    format: ImagePixelFormat::Rgba8,
                },
                destination: Rect::new(0, 0, 1, 1),
                clip: frame.clip(),
                lifecycle: ImageLifecycle::Frame,
            }));
        };
        let mut presenter = ImageTerminalPresenter::new(
            terminal,
            render,
            capabilities(),
            bmux_image::ImageConfig::default(),
        );
        presenter.present(&mut ()).expect("initial presentation");
        let before = presenter.terminal().writer().len();

        presenter.reset(ResetReason::Application);
        presenter.present(&mut ()).expect("reset presentation");

        let output = String::from_utf8(presenter.terminal().writer()[before..].to_vec())
            .expect("terminal output");
        assert!(output.contains("a=d"));
        assert!(output.contains("a=t"));
    }
}
