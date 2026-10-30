//! Terminal runtime abstraction.

use std::io::{self, Write};

use crate::ansi::{AnsiFrameDiffStats, write_ansi_frame, write_ansi_frame_diff};
use crate::buffer::Buffer;
use crate::damage::Damage;
use crate::event::{Event, FocusEvent};
use crate::focus::{FocusId, FocusTrap};
use crate::geometry::Point;
use crate::geometry::Rect;
use crate::hit::HitId;
use crate::hit::HitMap;
use crate::image::ImageContribution;
use crate::image_scene::{ImageScene, ImageSceneDelta};
use crate::paint::PaintCx;
use crate::pointer::{PointerIntent, PointerShape, resolve_pointer_intent};
use crate::presentation::{PreparedPresentation, PresentationPhase, PresentationState};
use crate::selection::SelectionScene;
use bmux_terminal_protocol::pointer_output::{PointerOutput, PointerShapeCapability};

/// Caller-owned routing context, never a second input router.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PointerContext {
    /// Active modal scope from the application's committed router.
    pub scope: Option<HitId>,
    /// Existing capture owner from the application's router.
    pub capture: Option<HitId>,
}

/// Statistics from one terminal draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrawStats {
    /// Number of cells written by the backend.
    pub changed_cells: usize,
    /// Whether the draw used a full repaint.
    pub full_repaint: bool,
}

impl DrawStats {
    const fn full(cell_count: usize) -> Self {
        Self {
            changed_cells: cell_count,
            full_repaint: true,
        }
    }
}

impl From<AnsiFrameDiffStats> for DrawStats {
    fn from(value: AnsiFrameDiffStats) -> Self {
        Self {
            changed_cells: value.changed_cells,
            full_repaint: value.full_repaint,
        }
    }
}

/// A simple ANSI terminal runtime backed by an arbitrary writer.
///
/// `Terminal` owns the previous frame buffer so repeated draws can use
/// damage-aware flushing.
pub struct Terminal<W> {
    writer: W,
    state: PresentationState,
    pointer_output: PointerOutput,
    pointer_capability: PointerShapeCapability,
    pointer_position: Option<Point>,
    pointer_context: PointerContext,
}

impl<W: Write> Terminal<W> {
    /// Create a terminal runtime for `area`.
    #[must_use]
    pub fn new(writer: W, area: Rect) -> Self {
        Self {
            writer,
            state: PresentationState::new(area),
            pointer_output: PointerOutput::new(
                bmux_terminal_protocol::pointer_output::tmux_passthrough_enabled(),
            ),
            pointer_capability: PointerShapeCapability::detect(),
            pointer_position: None,
            pointer_context: PointerContext::default(),
        }
    }

    /// Fork committed authority into a staging terminal with a different writer.
    /// The original remains authoritative until the caller acknowledges delivery.
    #[must_use]
    pub fn fork<V: Write>(&self, writer: V) -> Terminal<V> {
        Terminal {
            writer,
            state: self.state.clone(),
            pointer_output: self.pointer_output.clone(),
            pointer_capability: self.pointer_capability,
            pointer_position: self.pointer_position,
            pointer_context: self.pointer_context.clone(),
        }
    }

    /// Adopt an exactly delivered staging terminal's authority, retaining this writer.
    pub fn acknowledge<V>(&mut self, staged: Terminal<V>) {
        self.state = staged.state;
        self.pointer_output = staged.pointer_output;
    }

    /// Set explicit host capability before acquiring pointer ownership.
    pub const fn set_pointer_capability(&mut self, capability: PointerShapeCapability) {
        self.pointer_capability = capability;
    }

    /// Observe without consuming or routing input. Applications still receive
    /// the exact original event. Returns whether presentation needs servicing.
    pub fn observe_event(&mut self, event: &Event) -> bool {
        let position = match event {
            Event::Mouse(mouse) => Some(mouse.position),
            Event::Focus(FocusEvent::Lost) => None,
            _ => return false,
        };
        let changed = position != self.pointer_position;
        self.pointer_position = position;
        changed
    }

    /// Supply the existing application's routing authority, not a new router.
    pub fn set_pointer_context(&mut self, context: PointerContext) {
        self.pointer_context = context;
    }

    /// Invalidate host output knowledge after failed delivery of a staging terminal.
    pub fn invalidate_after_failed_delivery<V>(&mut self, candidate: &Terminal<V>) {
        self.pointer_output
            .invalidate_after_failed_delivery(&candidate.pointer_output);
        self.state.reset();
    }

    /// Restore this terminal's pointer lease before suspend, exit, or error recovery.
    /// # Errors
    /// Reports failed output or uncertain stack ownership without replaying it.
    pub fn restore_pointer(&mut self) -> io::Result<()> {
        self.pointer_output.restore(&mut self.writer)
    }

    /// Stage restoration for delivery by an exclusive asynchronous output owner.
    /// # Errors
    /// Returns uncertain stack ownership errors.
    pub fn prepare_pointer_restore(
        &mut self,
    ) -> io::Result<Option<bmux_terminal_protocol::pointer_output::PointerOutputTransaction>> {
        self.pointer_output.begin_restore()
    }

    /// Complete restoration only after actual host flush.
    pub fn complete_pointer_restore(
        &mut self,
        transaction: bmux_terminal_protocol::pointer_output::PointerOutputTransaction,
        succeeded: bool,
    ) {
        self.pointer_output.complete(transaction, succeeded);
    }

    /// Return the terminal area.
    #[must_use]
    pub const fn area(&self) -> Rect {
        self.state.area()
    }

    /// Return the last successfully committed cell buffer, if one exists.
    #[must_use]
    pub const fn retained_buffer(&self) -> Option<&Buffer> {
        self.state.retained_buffer()
    }

    /// Return the cursor state from the last successfully committed draw.
    #[must_use]
    pub const fn cursor(&self) -> Option<crate::frame::Cursor> {
        self.state.cursor()
    }

    /// Return the interaction scene registered by the last draw.
    #[must_use]
    pub const fn hits(&self) -> &HitMap {
        self.state.hits()
    }

    /// Return the selection scene from the last successfully committed draw.
    #[must_use]
    pub const fn selection(&self) -> &SelectionScene {
        self.state.selection()
    }

    /// Return semantic regions from the last successfully committed draw.
    #[must_use]
    pub const fn semantics(&self) -> &crate::semantic::SemanticScene {
        self.state.semantics()
    }

    /// Return ordered focus state derived from the last committed scene.
    #[must_use]
    pub const fn focus(&self) -> &FocusTrap {
        self.state.focus()
    }

    /// Return the active focus target after the last committed draw.
    #[must_use]
    pub fn focused(&self) -> Option<&FocusId> {
        self.state.focus().active()
    }

    /// Set the active focus target when it exists in the committed scene.
    pub fn set_focused(&mut self, id: &FocusId) -> bool {
        self.state.set_focused(id)
    }

    /// Return image lifecycle contributions registered by the last draw.
    #[must_use]
    pub fn images(&self) -> &[ImageContribution] {
        self.state.images()
    }

    /// Return the active image scene after the last draw.
    #[must_use]
    pub const fn image_scene(&self) -> &ImageScene {
        self.state.image_scene()
    }

    /// Return image additions, updates, and removals produced by the last draw.
    #[must_use]
    pub const fn image_delta(&self) -> &ImageSceneDelta {
        self.state.image_delta()
    }

    /// Reset retained terminal presentation state so the next draw repaints fully.
    ///
    /// Committed interaction and selection scenes remain authoritative until a
    /// later successful draw replaces them. This lets consumers continue to
    /// route input against what is still visibly committed after an output
    /// failure or backend reset.
    pub fn reset(&mut self) {
        self.state.reset();
    }

    /// Resize the terminal area and force the next draw to repaint fully.
    pub fn resize(&mut self, area: Rect) {
        self.state.resize(area);
    }

    /// Draw one complete frame.
    ///
    /// # Errors
    ///
    /// Returns any I/O error reported by the backend writer.
    pub fn draw(&mut self, render: impl FnOnce(&mut PaintCx<'_, '_>)) -> io::Result<DrawStats> {
        self.draw_damage(Damage::Full, render)
    }

    /// Draw one complete frame and emit protocol overlays before committing it.
    ///
    /// The overlay callback receives the staged image scene and its delta after
    /// cell output has been written but before the shared output flush. The
    /// frame becomes authoritative only when both cell and overlay output
    /// succeed. This allows image-capable presenters to preserve the same
    /// commit boundary as text, focus, hit, cursor, and selection metadata.
    ///
    /// # Errors
    ///
    /// Returns any I/O error reported by the backend writer or overlay callback.
    pub fn draw_with_overlay(
        &mut self,
        render: impl FnOnce(&mut PaintCx<'_, '_>),
        overlay: impl FnOnce(&mut W, &ImageScene, &ImageSceneDelta) -> io::Result<()>,
    ) -> io::Result<DrawStats> {
        self.draw_damage_with_overlay(Damage::Full, render, overlay)
    }

    /// Draw a frame using process-local retained presentation state outside `damage`.
    ///
    /// Region damage is valid only after a successful complete presentation. If no retained
    /// frame exists, the terminal safely promotes the draw to a complete presentation. Rendering
    /// and metadata are staged and become authoritative only after terminal output flushes. A
    /// regional render that does not emit cursor or focus-scope metadata retains the previously
    /// committed values; emitting either value replaces it.
    ///
    /// # Errors
    ///
    /// Returns any I/O error reported by the backend writer. Failed output does not advance the
    /// retained buffer, hit map, selection scene, cursor, or image scene.
    pub fn draw_damage(
        &mut self,
        damage: Damage,
        render: impl FnOnce(&mut PaintCx<'_, '_>),
    ) -> io::Result<DrawStats> {
        self.draw_damage_with_overlay(damage, render, |_, _, _| Ok(()))
    }

    /// Draw a frame and emit protocol overlays before committing retained state.
    ///
    /// This has the same regional-retention semantics as [`Self::draw_damage`].
    /// The overlay callback observes the complete staged image scene and delta,
    /// and writes through the terminal's writer before its single flush.
    ///
    /// # Errors
    ///
    /// Returns any I/O error reported by the backend writer or overlay callback.
    pub fn draw_damage_with_overlay(
        &mut self,
        damage: Damage,
        render: impl FnOnce(&mut PaintCx<'_, '_>),
        overlay: impl FnOnce(&mut W, &ImageScene, &ImageSceneDelta) -> io::Result<()>,
    ) -> io::Result<DrawStats> {
        self.draw_damage_with_output_hooks(damage, render, |_| Ok(false), overlay)
    }

    /// Draw with a pre-paint output hook and a post-paint overlay in one transaction.
    /// A pre-paint hook returning `true` invalidates the backend's retained cells
    /// and forces complete cell output without changing committed scene authority.
    ///
    /// # Errors
    /// Returns output or hook errors; retained state commits only after flush.
    pub fn draw_damage_with_output_hooks(
        &mut self,
        damage: Damage,
        render: impl FnOnce(&mut PaintCx<'_, '_>),
        before: impl FnOnce(&mut W) -> io::Result<bool>,
        overlay: impl FnOnce(&mut W, &ImageScene, &ImageSceneDelta) -> io::Result<()>,
    ) -> io::Result<DrawStats> {
        self.draw_damage_with_prepared_output_hooks(damage, render, |_| Ok(()), before, overlay)
    }

    /// Configure a prepared snapshot before any output in the same transaction.
    /// Callers stage scopes and validate their admission budget here. Rejection
    /// writes nothing; output failure preserves all committed scene metadata.
    ///
    /// # Errors
    /// Returns configuration, output, or hook errors without committing staging.
    pub fn draw_damage_with_prepared_output_hooks(
        &mut self,
        damage: Damage,
        render: impl FnOnce(&mut PaintCx<'_, '_>),
        configure: impl FnOnce(&mut PreparedPresentation) -> io::Result<()>,
        before: impl FnOnce(&mut W) -> io::Result<bool>,
        overlay: impl FnOnce(&mut W, &ImageScene, &ImageSceneDelta) -> io::Result<()>,
    ) -> io::Result<DrawStats> {
        let mut prepared = self.state.prepare(damage, render);
        if let Some(prepared) = &mut prepared {
            PresentationPhase::Configure.measure(|| configure(prepared), Result::is_ok)?;
        }
        self.draw_prepared_with_output_hooks(prepared, before, overlay)
    }

    /// Paint an owned frame without encoding or committing it.
    pub fn prepare_frame(
        &self,
        damage: Damage,
        render: impl FnOnce(&mut PaintCx<'_, '_>),
    ) -> Option<PreparedPresentation> {
        self.state.prepare(damage, render)
    }

    /// Encode and commit an owned prepared frame through this terminal's writer.
    /// # Errors
    /// Returns output or hook errors without committing failed output.
    pub fn draw_prepared_with_output_hooks(
        &mut self,
        prepared: Option<PreparedPresentation>,
        before: impl FnOnce(&mut W) -> io::Result<bool>,
        overlay: impl FnOnce(&mut W, &ImageScene, &ImageSceneDelta) -> io::Result<()>,
    ) -> io::Result<DrawStats> {
        let position = self.pointer_position;
        let context = &self.pointer_context;
        let pointer_output = &mut self.pointer_output;
        let capability = self.pointer_capability;
        if prepared.is_none() {
            let intent = resolve_pointer_intent(
                self.state.hits(),
                position,
                self.state.focus().active_scope(),
                context.capture.as_ref(),
            );
            let shape = match intent {
                PointerIntent::Shape(shape) => shape,
                PointerIntent::Auto | PointerIntent::Hidden => PointerShape::Default,
            };
            pointer_output.present(&mut self.writer, capability, shape)?;
            return Ok(DrawStats {
                changed_cells: 0,
                full_repaint: false,
            });
        }
        let writer = &mut self.writer;
        self.state
            .present_prepared(prepared, |prepared, previous| {
                prepared.require_uniform_grid().map_err(io::Error::other)?;
                let intent = resolve_pointer_intent(
                    prepared.hits(),
                    position,
                    prepared.focus_scope(),
                    context.capture.as_ref(),
                );
                // Hidden is unsupported by OSC 22: explicitly restore a truthful
                // visible fallback instead of retaining a stale hand or resize shape.
                // The text caret remains entirely independent.
                let shape = match intent {
                    PointerIntent::Shape(shape) => shape,
                    PointerIntent::Auto | PointerIntent::Hidden => PointerShape::Default,
                };
                let transaction = pointer_output.begin_present(capability, shape)?;
                let result = (|| {
                    writer.write_all(b"\x1b[?2026h")?;
                    let output: io::Result<DrawStats> = (|| {
                        let repaint = before(writer)?;
                        let stats = PresentationPhase::AnsiEncode.measure(
                            || -> io::Result<DrawStats> {
                                Ok(if let Some(previous) = previous.filter(|_| !repaint) {
                                    write_ansi_frame_diff(
                                        writer,
                                        previous,
                                        prepared.buffer(),
                                        prepared.cursor(),
                                    )?
                                    .into()
                                } else {
                                    write_ansi_frame(writer, prepared.buffer(), prepared.cursor())?;
                                    DrawStats::full(prepared.buffer().cells().len())
                                })
                            },
                            Result::is_ok,
                        )?;
                        overlay(writer, prepared.image_scene(), prepared.image_delta())?;
                        if let Some(transaction) = &transaction {
                            writer.write_all(transaction.bytes())?;
                        }
                        Ok(stats)
                    })();
                    let end_result = writer.write_all(b"\x1b[?2026l");
                    let stats = output?;
                    end_result?;
                    PresentationPhase::WriterFlush.measure(|| writer.flush(), Result::is_ok)?;
                    Ok(stats)
                })();
                if let Some(transaction) = transaction {
                    pointer_output.complete(transaction, result.is_ok());
                }
                result
            })
            .map(|stats| {
                stats.unwrap_or(DrawStats {
                    changed_cells: 0,
                    full_repaint: false,
                })
            })
    }

    /// Return a reference to the backend writer.
    #[must_use]
    pub const fn writer(&self) -> &W {
        &self.writer
    }

    /// Return a mutable reference to the backend writer.
    pub const fn writer_mut(&mut self) -> &mut W {
        &mut self.writer
    }

    /// Release presentation protocols before temporarily returning terminal ownership.
    /// # Errors
    /// Reports output failure or uncertain stack ownership.
    pub fn suspend(&mut self) -> io::Result<()> {
        let result = self.restore_pointer();
        self.reset();
        result
    }

    /// Invalidate retained encoding after terminal ownership has been reacquired.
    /// Pointer lease uncertainty is deliberately not cleared.
    pub fn resume(&mut self) {
        self.reset();
    }

    /// Restore protocol ownership before recovering the writer.
    /// # Errors
    /// Reports incomplete pointer restoration.
    pub fn into_clean_inner(mut self) -> io::Result<W> {
        self.restore_pointer()?;
        Ok(self.writer)
    }

    /// Consume the terminal and return the backend writer.
    pub fn into_inner(self) -> W {
        self.writer
    }
}

/// Linux output owner opened independently from stdin/stdout.
/// Uses nonblocking writes and at most 10ms poll waits, so cancellation never
/// waits for a terminal consumer to drain. Dropping does not flush.
#[cfg(target_os = "linux")]
pub struct LinuxTerminalOutput(std::fs::File);

#[cfg(target_os = "linux")]
impl LinuxTerminalOutput {
    /// Reopen the actual stdout tty with independent nonblocking flags.
    ///
    /// Does not use `/dev/tty`, which can differ from redirected stdout.
    /// # Errors
    /// Returns `Unsupported` for non-tty stdout or identity mismatch; otherwise
    /// returns the procfs reopen/metadata error. No shared descriptor flags change.
    pub fn open() -> io::Result<Self> {
        Self::reopen_tty(libc::STDOUT_FILENO)
    }

    fn reopen_tty(fd: std::os::fd::RawFd) -> io::Result<Self> {
        use std::os::{
            fd::AsRawFd,
            unix::fs::{MetadataExt, OpenOptionsExt},
        };
        // SAFETY: isatty only inspects the supplied descriptor.
        if unsafe { libc::isatty(fd) } != 1 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "stdout is not a tty",
            ));
        }
        let source = std::fs::metadata(format!("/proc/self/fd/{fd}"))?;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .open(format!("/proc/self/fd/{fd}"))?;
        let reopened = file.metadata()?;
        // SAFETY: file owns a live descriptor for this entire call.
        if unsafe { libc::isatty(file.as_raw_fd()) } != 1
            || (source.dev(), source.ino(), source.rdev())
                != (reopened.dev(), reopened.ino(), reopened.rdev())
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "stdout tty identity changed",
            ));
        }
        Ok(Self(file))
    }

    /// Deliver a complete terminal transaction with bounded cancellation latency.
    /// Successful writes deliver to the kernel terminal queue; no `tcdrain` is
    /// performed. Like `File::flush`, flushing a tty has no userspace buffer.
    /// Partial output on failure requires a full repaint before reuse.
    /// # Errors
    /// Returns write/poll errors or `Interrupted` when cancellation is requested.
    pub fn deliver(
        &mut self,
        bytes: &[u8],
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        use std::sync::atomic::Ordering;
        let mut remaining = bytes;
        while !remaining.is_empty() {
            if cancelled.load(Ordering::Acquire) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "presentation cancelled",
                ));
            }
            let chunk = remaining.len().min(4096);
            match self.0.write(&remaining[..chunk]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "terminal write returned zero",
                    ));
                }
                Ok(count) => remaining = &remaining[count..],
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    let mut fd = libc::pollfd {
                        fd: self.0.as_raw_fd(),
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    // SAFETY: fd points to one initialized pollfd and remains owned by self.
                    let result = unsafe { libc::poll(&raw mut fd, 1, 10) };
                    if result < 0 {
                        let error = io::Error::last_os_error();
                        if error.kind() != io::ErrorKind::Interrupted {
                            return Err(error);
                        }
                    }
                }
                Err(error) => return Err(error),
            }
        }
        if cancelled.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "presentation cancelled",
            ));
        }
        self.0.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::Terminal;
    use crate::damage::Damage;
    use crate::frame::Cursor;
    use crate::geometry::{Point, Rect};
    use crate::hit::HitRegion;
    use crate::image::{ImageContribution, ImageKey, ImageLifecycle, ImagePayload, ImagePlacement};
    use crate::paint::{LocalRect, PaintCx};
    use crate::selection::{SelectionFragment, SelectionScope};
    use crate::semantic::SemanticRegion;
    use crate::style::Style;
    use std::io::{self, Write};

    #[test]
    fn uniform_terminal_rejects_fragments_before_output_or_hooks() {
        use crate::buffer::Buffer;
        use crate::presentation::{GridTransform, PreparedGridFragment, PresentationLimits};
        use std::sync::Arc;
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 2, 1));
        let mut prepared = terminal.prepare_frame(Damage::Full, |_| {}).unwrap();
        prepared
            .set_grid_fragments(
                vec![
                    PreparedGridFragment::new(
                        "grid",
                        Arc::new(Buffer::empty(Rect::new(0, 0, 1, 1))),
                        GridTransform::new(Point::new(0, 0), 1000).unwrap(),
                        vec![Rect::new(0, 0, 1, 1)],
                    )
                    .unwrap(),
                ],
                PresentationLimits {
                    max_cells: 10,
                    max_entries: 10,
                    max_bytes: 1024,
                },
            )
            .unwrap();
        assert!(
            terminal
                .draw_prepared_with_output_hooks(
                    Some(prepared),
                    |_| panic!("before hook must not run"),
                    |_, _, _| panic!("overlay must not run")
                )
                .is_err()
        );
        assert_eq!(terminal.writer().as_slice(), []);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stdout_reopen_rejects_redirects_and_preserves_tty_flags() {
        use super::LinuxTerminalOutput;
        use std::os::fd::{AsRawFd, FromRawFd};
        let regular = std::fs::File::open("/dev/null").unwrap();
        assert!(
            matches!(LinuxTerminalOutput::reopen_tty(regular.as_raw_fd()), Err(error) if error.kind() == io::ErrorKind::Unsupported)
        );
        let mut master = -1;
        let mut slave = -1;
        // SAFETY: valid output pointers with optional configuration omitted.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &raw mut master,
                    &raw mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0
        );
        // SAFETY: successful openpty transfers distinct owned descriptors.
        let _master = unsafe { std::fs::File::from_raw_fd(master) };
        // SAFETY: successful openpty transfers an owned slave descriptor.
        let slave = unsafe { std::fs::File::from_raw_fd(slave) };
        // SAFETY: F_GETFL only inspects a live owned descriptor.
        let before = unsafe { libc::fcntl(slave.as_raw_fd(), libc::F_GETFL) };
        let reopened = LinuxTerminalOutput::reopen_tty(slave.as_raw_fd()).unwrap();
        // SAFETY: F_GETFL only inspects live owned descriptors.
        assert_eq!(
            unsafe { libc::fcntl(slave.as_raw_fd(), libc::F_GETFL) },
            before
        );
        // SAFETY: F_GETFL only inspects the independent live descriptor.
        assert_ne!(
            unsafe { libc::fcntl(reopened.0.as_raw_fd(), libc::F_GETFL) } & libc::O_NONBLOCK,
            0
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn nonblocking_tty_backpressure_is_cancellable_without_drain() {
        use super::LinuxTerminalOutput;
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let mut master = -1;
        let mut slave = -1;
        // SAFETY: valid output pointers, no optional configuration pointers.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &raw mut master,
                    &raw mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0
        );
        // SAFETY: successful openpty yields two distinct owned descriptors.
        let _master = unsafe { std::fs::File::from_raw_fd(master) };
        // SAFETY: successful openpty yields an owned slave descriptor.
        let slave = unsafe { std::fs::File::from_raw_fd(slave) };
        // SAFETY: live exclusively owned PTY descriptor.
        assert_eq!(
            unsafe { libc::fcntl(slave.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
            0
        );
        let mut output = LinuxTerminalOutput(slave);
        // Fill the real kernel tty queue without reading the peer.
        let bytes = [b'x'; 4096];
        loop {
            match output.0.write(&bytes) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("fill tty: {error}"),
            }
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let (done, completed) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = output.deliver(&vec![b'y'; 1024 * 1024], &worker_cancelled);
            done.send(result).unwrap();
        });
        assert!(
            completed
                .recv_timeout(std::time::Duration::from_millis(30))
                .is_err(),
            "output is genuinely backpressured"
        );
        cancelled.store(true, Ordering::Release);
        let result = completed
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("bounded cancellation without peer drain");
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
        worker.join().unwrap();
    }

    struct FailingWriter {
        bytes: Vec<u8>,
        fail: bool,
    }

    impl Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.fail {
                Err(io::Error::other("injected write failure"))
            } else {
                self.bytes.extend_from_slice(bytes);
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

    #[test]
    fn observer_preserves_bytes_and_reports_flush_failure_without_commit() {
        use crate::presentation::{PresentationPhase, with_presentation_observer};
        use std::cell::RefCell;
        use std::rc::Rc;
        let draw = |terminal: &mut Terminal<FailingWriter>| {
            terminal.draw(|cx| {
                cx.fill(
                    crate::paint::LocalRect::new(0, 0, 2, 1),
                    "x",
                    crate::style::Style::new(),
                );
            })
        };
        let make = || {
            Terminal::new(
                FailingWriter {
                    bytes: Vec::new(),
                    fail: false,
                },
                Rect::new(0, 0, 2, 1),
            )
        };
        let mut baseline = make();
        draw(&mut baseline).unwrap();
        draw(&mut baseline).unwrap();
        let records = Rc::new(RefCell::new(Vec::with_capacity(32)));
        let sink = Rc::clone(&records);
        let mut observed = make();
        with_presentation_observer(
            7,
            move |record| sink.borrow_mut().push(record),
            || {
                draw(&mut observed).unwrap();
                draw(&mut observed).unwrap();
            },
        );
        assert_eq!(baseline.writer().bytes, observed.writer().bytes);
        assert_eq!(baseline.retained_buffer(), observed.retained_buffer());
        assert!(
            records
                .borrow()
                .iter()
                .all(|record| record.attempt == 7 && record.success)
        );
        assert!(
            records
                .borrow()
                .iter()
                .any(|record| record.phase == PresentationPhase::AnsiEncode)
        );
        let sink = Rc::clone(&records);
        observed.writer_mut().fail = true;
        with_presentation_observer(
            8,
            move |record| sink.borrow_mut().push(record),
            || {
                assert!(draw(&mut observed).is_err());
            },
        );
        assert!(observed.retained_buffer().is_none());
        assert!(records.borrow().iter().any(|record| record.attempt == 8
            && record.phase == PresentationPhase::TerminalOutputTotal
            && !record.success));
        assert!(
            !records
                .borrow()
                .iter()
                .any(|record| record.attempt == 8
                    && record.phase == PresentationPhase::TerminalCommit)
        );
    }

    #[test]
    fn flush_failure_preserves_scene_after_successful_cell_and_overlay_writes() {
        struct FlushFailure {
            bytes: Vec<u8>,
            fail_flush: bool,
        }
        impl Write for FlushFailure {
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
        let mut terminal = Terminal::new(
            FlushFailure {
                bytes: Vec::new(),
                fail_flush: false,
            },
            Rect::new(0, 0, 2, 1),
        );
        terminal
            .draw(|cx| {
                cx.fill(LocalRect::new(0, 0, 2, 1), "A", Style::new());
                cx.push_hit(HitRegion::new("committed", cx.clip()).focusable(true));
            })
            .unwrap();
        terminal.writer_mut().fail_flush = true;
        let mut overlay_ran = false;
        let failed = terminal.draw_with_overlay(
            |cx| {
                cx.fill(LocalRect::new(0, 0, 2, 1), "B", Style::new());
                cx.push_hit(HitRegion::new("speculative", cx.clip()).focusable(true));
            },
            |writer, _, _| {
                overlay_ran = true;
                writer.write_all(b"overlay")
            },
        );
        assert!(failed.is_err());
        assert!(overlay_ran);
        assert_eq!(terminal.hits().regions()[0].id.as_str(), "committed");
        assert_eq!(terminal.focused().unwrap().as_str(), "committed");
        assert!(terminal.retained_buffer().is_none());
        terminal.writer_mut().fail_flush = false;
        assert!(terminal.draw(|_| {}).unwrap().full_repaint);
    }

    #[test]
    fn sparse_output_short_writes_and_failures_repair_transactionally() {
        struct ShortWriter {
            bytes: Vec<u8>,
            remaining: Option<usize>,
            fail_flush: bool,
        }
        impl Write for ShortWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.remaining == Some(0) {
                    self.remaining = None;
                    return Err(io::Error::other("injected prefix failure"));
                }
                let count = bytes.len().min(3).min(self.remaining.unwrap_or(usize::MAX));
                self.bytes.extend_from_slice(&bytes[..count]);
                if let Some(remaining) = &mut self.remaining {
                    *remaining -= count;
                }
                Ok(count)
            }
            fn flush(&mut self) -> io::Result<()> {
                if self.fail_flush {
                    Err(io::Error::other("injected flush failure"))
                } else {
                    Ok(())
                }
            }
        }
        let paint = |cx: &mut PaintCx<'_, '_>| {
            cx.fill(LocalRect::new(0, 0, 1, 1), "A", Style::new());
            cx.fill(LocalRect::new(79, 0, 1, 1), "Z", Style::new());
            cx.push_hit(HitRegion::new("new", cx.clip()).focusable(true));
            cx.set_cursor(Point::new(79, 0), true);
        };
        for (budget, fail_flush) in [
            (Some(0), false),
            (Some(1), false),
            (Some(6), false),
            (Some(12), false),
            (Some(20), false),
            (Some(30), false),
            (None, true),
            (None, false),
        ] {
            let mut terminal = Terminal::new(
                ShortWriter {
                    bytes: Vec::new(),
                    remaining: None,
                    fail_flush: false,
                },
                Rect::new(0, 0, 80, 1),
            );
            terminal
                .draw(|cx| {
                    cx.push_hit(HitRegion::new("old", cx.clip()).focusable(true));
                    cx.set_cursor(Point::new(0, 0), true);
                })
                .unwrap();
            terminal.writer_mut().bytes.clear();
            terminal.writer_mut().remaining = budget;
            terminal.writer_mut().fail_flush = fail_flush;
            let result = terminal.draw(paint);
            if budget.is_some() || fail_flush {
                assert!(result.is_err());
                assert!(terminal.retained_buffer().is_none());
                assert_eq!(terminal.hits().regions()[0].id.as_str(), "old");
                assert_eq!(terminal.focused().unwrap().as_str(), "old");
                assert_eq!(terminal.cursor(), Some(Cursor::visible(Point::new(0, 0))));
                terminal.writer_mut().remaining = None;
                terminal.writer_mut().fail_flush = false;
                terminal.writer_mut().bytes.clear();
                assert!(terminal.draw(paint).unwrap().full_repaint);
            } else {
                assert_eq!(result.unwrap().changed_cells, 2);
                assert!(terminal.writer().bytes.len() < 80);
            }
            assert_eq!(terminal.hits().regions()[0].id.as_str(), "new");
            assert_eq!(terminal.cursor(), Some(Cursor::visible(Point::new(79, 0))));
            let buffer = terminal.retained_buffer().unwrap();
            assert_eq!(buffer.get(Point::new(0, 0)).unwrap().symbol, "A");
            assert_eq!(buffer.get(Point::new(79, 0)).unwrap().symbol, "Z");
            terminal.resize(Rect::new(0, 0, 81, 1));
            assert!(terminal.draw(paint).unwrap().full_repaint);
        }
    }

    #[test]
    fn failed_overlay_does_not_commit_image_or_interaction_scene() {
        let mut terminal = Terminal::new(
            FailingWriter {
                bytes: Vec::new(),
                fail: false,
            },
            Rect::new(0, 0, 4, 1),
        );
        terminal
            .draw(|frame| {
                frame.push_hit(HitRegion::new("committed", frame.clip()));
                frame.push_image(ImageContribution::Present(ImagePlacement {
                    fit: crate::image::ImageFit::Stretch,
                    allocation: None,
                    key: ImageKey::new("committed"),
                    payload: ImagePayload::Pixels {
                        bytes: vec![1, 2, 3, 4],
                        width: 1,
                        height: 1,
                        format: crate::image::ImagePixelFormat::Rgba8,
                    },
                    destination: frame.clip(),
                    clip: frame.clip(),
                    lifecycle: ImageLifecycle::Frame,
                }));
            })
            .expect("initial frame commits");

        let error = terminal.draw_with_overlay(
            |frame| {
                frame.push_hit(HitRegion::new("speculative", frame.clip()));
                frame.push_image(ImageContribution::Present(ImagePlacement {
                    fit: crate::image::ImageFit::Stretch,
                    allocation: None,
                    key: ImageKey::new("speculative"),
                    payload: ImagePayload::Pixels {
                        bytes: vec![5, 6, 7, 8],
                        width: 1,
                        height: 1,
                        format: crate::image::ImagePixelFormat::Rgba8,
                    },
                    destination: frame.clip(),
                    clip: frame.clip(),
                    lifecycle: ImageLifecycle::Frame,
                }));
            },
            |_, _, _| Err(io::Error::other("injected overlay failure")),
        );

        assert!(error.is_err());
        assert_eq!(terminal.hits().regions()[0].id.as_str(), "committed");
        assert!(
            terminal
                .image_scene()
                .get(&ImageKey::new("committed"))
                .is_some()
        );
        assert!(
            terminal
                .image_scene()
                .get(&ImageKey::new("speculative"))
                .is_none()
        );
        assert!(terminal.retained_buffer().is_none());
        let output = String::from_utf8(terminal.writer().bytes.clone()).expect("ANSI output");
        assert!(output.ends_with("\u{1b}[?2026l"));
    }

    #[test]
    fn failed_draw_does_not_commit_speculative_selection_scene() {
        let mut terminal = Terminal::new(
            FailingWriter {
                bytes: Vec::new(),
                fail: false,
            },
            Rect::new(0, 0, 4, 1),
        );
        terminal
            .draw(|frame| {
                frame.push_selection_scope(SelectionScope::new("committed", frame.clip()));
                frame.push_selection_fragment(SelectionFragment::new(
                    "committed",
                    "content",
                    Rect::new(0, 0, 1, 1),
                    0,
                    0..1,
                ));
            })
            .expect("initial selection scene commits");

        terminal.writer_mut().fail = true;
        assert!(
            terminal
                .draw(|frame| {
                    frame.push_selection_scope(SelectionScope::new("speculative", frame.clip()));
                })
                .is_err()
        );

        assert_eq!(terminal.selection().scopes()[0].id.as_str(), "committed");
    }

    #[test]
    fn terminal_diff_scrolls_zwj_text_without_independent_continuation_writes() {
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 8, 2));
        terminal
            .draw(|frame| {
                frame.write_line(LocalRect::new(0, 0, 8, 1), &crate::text::Line::raw("A👩🏽‍💻│"));
                frame.write_line(
                    LocalRect::new(0, 1, 8, 1),
                    &crate::text::Line::raw("bottom│"),
                );
            })
            .unwrap();
        let first_len = terminal.writer().len();

        terminal
            .draw(|frame| {
                frame.write_line(
                    LocalRect::new(0, 0, 8, 1),
                    &crate::text::Line::raw("bottom│"),
                );
                frame.write_line(LocalRect::new(0, 1, 8, 1), &crate::text::Line::raw("B👩🏽‍💻│"));
            })
            .unwrap();
        let output = String::from_utf8(terminal.writer()[first_len..].to_vec()).unwrap();

        assert!(output.contains("👩🏽‍💻"));
        assert!(!output.contains("\x1b[2;3H "));
    }

    #[test]
    fn regional_overlay_hover_retains_background_cursor_and_sparse_terminal_output() {
        use crate::frame::Cursor;
        use crate::hit::HitRegion;
        use crate::semantic::SemanticRegion;
        let area = Rect::new(0, 0, 24, 6);
        let popup = Rect::new(12, 1, 10, 2);
        let cursor = Cursor::visible(Point::new(4, 5));
        let background = |cx: &mut PaintCx<'_, '_>| {
            cx.fill(cx.area(), ".", Style::new());
            cx.write_line(
                LocalRect::new(0, 5, 10, 1),
                &crate::text::Line::raw("composer"),
            );
            cx.set_cursor(cursor.position, true);
            cx.push_hit(HitRegion::new("background", area).focusable(true));
            cx.push_semantic(SemanticRegion::new("background", area, "editor"));
        };
        let overlay = |cx: &mut PaintCx<'_, '_>, text: &str| {
            cx.occlude(LocalRect::terminal(popup));
            cx.write_line(LocalRect::terminal(popup), &crate::text::Line::raw(text));
            cx.push_hit(HitRegion::new("option", popup).focusable(true));
        };
        let mut terminal = Terminal::new(Vec::new(), area);
        terminal.draw(background).unwrap();
        terminal
            .draw_damage(Damage::Regions(vec![popup]), |cx| overlay(cx, "Send"))
            .unwrap();
        assert_eq!(terminal.cursor(), Some(cursor));
        assert!(
            !terminal
                .hits()
                .regions()
                .iter()
                .any(|hit| hit.id.as_str() == "background"
                    && !hit.area.intersection(popup).is_empty())
        );
        let before = terminal.writer().len();
        let stats = terminal
            .draw_damage(Damage::Regions(vec![popup]), |cx| overlay(cx, "Queue"))
            .unwrap();
        assert!(!stats.full_repaint);
        assert!(stats.changed_cells <= usize::from(popup.width * popup.height));
        assert_eq!(terminal.cursor(), Some(cursor));
        let output = String::from_utf8_lossy(&terminal.writer()[before..]);
        assert!(!output.contains("composer"));
        assert!(output.contains("Queue"));
        assert!(output.contains("\x1b[?25h"));
        let mut fresh = Terminal::new(Vec::new(), area);
        fresh
            .draw(|cx| {
                background(cx);
                overlay(cx, "Queue");
            })
            .unwrap();
        assert_eq!(terminal.retained_buffer(), fresh.retained_buffer());
        assert_eq!(terminal.hits(), fresh.hits());
        // Closing restores exposed background metadata, not only its cells.
        terminal
            .draw_damage(Damage::Regions(vec![popup]), background)
            .unwrap();
        assert!(
            !terminal
                .hits()
                .regions()
                .iter()
                .any(|hit| hit.id.as_str() == "option")
        );
        assert_eq!(terminal.cursor(), Some(cursor));
    }

    #[test]
    fn terminal_first_draw_repaints_full_frame() {
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 2, 1));

        let stats = terminal
            .draw(|frame| {
                frame.write_line(LocalRect::new(0, 0, 1, 1), &crate::text::Line::raw("A"));
            })
            .unwrap();

        assert!(stats.full_repaint);
        assert_eq!(stats.changed_cells, 2);
        assert!(
            String::from_utf8(terminal.into_inner())
                .unwrap()
                .contains('A')
        );
    }

    #[test]
    fn terminal_second_draw_uses_diff_flush() {
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 2, 1));
        terminal
            .draw(|frame| {
                frame.write_line(LocalRect::new(0, 0, 1, 1), &crate::text::Line::raw("A"));
            })
            .unwrap();

        let stats = terminal
            .draw(|frame| {
                frame.write_line(LocalRect::new(1, 0, 1, 1), &crate::text::Line::raw("B"));
            })
            .unwrap();

        assert!(!stats.full_repaint);
        assert_eq!(stats.changed_cells, 2);
    }

    #[test]
    fn terminal_resize_forces_full_repaint() {
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 1, 1));
        terminal.draw(|_| {}).unwrap();

        terminal.resize(Rect::new(0, 0, 2, 1));
        let stats = terminal.draw(|_| {}).unwrap();

        assert!(stats.full_repaint);
        assert_eq!(stats.changed_cells, 2);
    }

    #[test]
    fn region_draw_retains_undamaged_cells_and_metadata() {
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 4, 2));
        let scope = crate::hit::HitId::new("modal");
        terminal
            .draw(|frame| {
                frame.fill(LocalRect::new(0, 0, 4, 2), "A", Style::new());
                frame.push_hit(HitRegion::new("left", Rect::new(0, 0, 2, 2)));
                frame.push_hit(HitRegion::new("right", Rect::new(2, 0, 2, 2)));
                frame.set_focus_scope(Some(scope.clone()));
                frame.set_cursor(Point::new(3, 1), true);
            })
            .unwrap();

        let stats = terminal
            .draw_damage(Damage::Regions(vec![Rect::new(0, 0, 2, 2)]), |frame| {
                frame.fill(LocalRect::new(0, 0, 2, 2), "B", Style::new());
                frame.push_hit(HitRegion::new("new-left", Rect::new(0, 0, 2, 2)));
            })
            .unwrap();

        assert_eq!(stats.changed_cells, 4);
        assert!(!stats.full_repaint);
        assert_eq!(terminal.cursor(), Some(Cursor::visible(Point::new(3, 1))));
        assert_eq!(terminal.focus().active_scope(), Some(&scope));
        assert_eq!(
            terminal
                .hits()
                .regions()
                .iter()
                .map(|region| region.id.as_str())
                .collect::<Vec<_>>(),
            ["right", "new-left"]
        );
    }

    #[test]
    fn region_draw_discards_writes_outside_declared_damage() {
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 4, 1));
        terminal
            .draw(|frame| frame.fill(frame.area(), "A", Style::new()))
            .unwrap();
        let before = terminal.writer().len();

        let stats = terminal
            .draw_damage(Damage::Regions(vec![Rect::new(0, 0, 1, 1)]), |frame| {
                frame.fill(frame.area(), "Z", Style::new());
            })
            .unwrap();

        assert_eq!(stats.changed_cells, 1);
        assert_eq!(
            String::from_utf8_lossy(&terminal.writer()[before..])
                .matches('Z')
                .count(),
            1
        );
    }

    #[test]
    fn region_draw_clears_removed_content_inside_damage() {
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 2, 1));
        terminal
            .draw(|frame| frame.fill(frame.area(), "A", Style::new()))
            .unwrap();

        let stats = terminal
            .draw_damage(Damage::Regions(vec![Rect::new(0, 0, 1, 1)]), |_| {})
            .unwrap();

        assert_eq!(stats.changed_cells, 1);
    }

    #[test]
    fn failed_region_draw_does_not_commit_metadata() {
        let writer = FailingWriter {
            bytes: Vec::new(),
            fail: false,
        };
        let mut terminal = Terminal::new(writer, Rect::new(0, 0, 2, 1));
        terminal
            .draw(|frame| {
                frame.fill(frame.area(), "A", Style::new());
                frame.push_hit(HitRegion::new("committed", frame.clip()));
                frame.push_selection_scope(SelectionScope::new("committed", frame.clip()));
                frame.push_selection_fragment(SelectionFragment::new(
                    "committed",
                    "committed-content",
                    frame.clip(),
                    0,
                    0..1,
                ));
                frame.push_semantic(SemanticRegion::new("committed", frame.clip(), "content"));
                frame.set_cursor(Point::new(1, 0), true);
            })
            .unwrap();
        terminal.writer_mut().fail = true;

        let result = terminal.draw_damage(Damage::Regions(vec![Rect::new(0, 0, 1, 1)]), |frame| {
            frame.fill(frame.area(), "B", Style::new());
            frame.push_hit(HitRegion::new("uncommitted", frame.clip()));
            frame.push_selection_scope(SelectionScope::new("uncommitted", frame.clip()));
            frame.push_selection_fragment(SelectionFragment::new(
                "uncommitted",
                "uncommitted-content",
                frame.clip(),
                0,
                0..1,
            ));
            frame.push_semantic(SemanticRegion::new("uncommitted", frame.clip(), "content"));
            frame.set_cursor(Point::new(0, 0), false);
        });

        assert!(result.is_err());
        assert_eq!(terminal.hits().regions()[0].id.as_str(), "committed");
        assert_eq!(terminal.selection().scopes()[0].id.as_str(), "committed");
        assert_eq!(
            terminal.selection().fragments()[0].content_id.as_str(),
            "committed-content"
        );
        assert_eq!(terminal.semantics().regions()[0].id.as_str(), "committed");
        assert_eq!(terminal.cursor(), Some(Cursor::visible(Point::new(1, 0))));
        terminal.writer_mut().fail = false;
        let recovered = terminal.draw(|frame| frame.fill(frame.area(), "C", Style::new()));
        assert!(recovered.expect("full recovery draw succeeds").full_repaint);
    }

    #[test]
    fn region_draw_matches_complete_frame_for_declared_change() {
        fn first(frame: &mut PaintCx<'_, '_>) {
            frame.fill(frame.area(), "A", Style::new());
            frame.push_hit(HitRegion::new("left", Rect::new(0, 0, 2, 1)));
            frame.push_hit(HitRegion::new("right", Rect::new(2, 0, 2, 1)));
            frame.push_selection_scope(SelectionScope::new("left-scope", Rect::new(0, 0, 2, 1)));
            frame.push_selection_scope(SelectionScope::new("right-scope", Rect::new(2, 0, 2, 1)));
            frame.push_semantic(SemanticRegion::new(
                "left-semantic",
                Rect::new(0, 0, 2, 1),
                "content",
            ));
            frame.push_semantic(SemanticRegion::new(
                "right-semantic",
                Rect::new(2, 0, 2, 1),
                "content",
            ));
            frame.set_cursor(Point::new(3, 0), true);
        }
        fn second(frame: &mut PaintCx<'_, '_>) {
            frame.fill(LocalRect::new(0, 0, 2, 1), "B", Style::new());
            frame.fill(LocalRect::new(2, 0, 2, 1), "A", Style::new());
            frame.push_hit(HitRegion::new("new-left", Rect::new(0, 0, 2, 1)));
            frame.push_hit(HitRegion::new("right", Rect::new(2, 0, 2, 1)));
            frame.push_selection_scope(
                SelectionScope::new("left-scope", Rect::new(0, 0, 2, 1)).revision(1),
            );
            frame.push_selection_scope(SelectionScope::new("right-scope", Rect::new(2, 0, 2, 1)));
            frame.push_semantic(SemanticRegion::new(
                "left-semantic",
                Rect::new(0, 0, 2, 1),
                "new-content",
            ));
            frame.push_semantic(SemanticRegion::new(
                "right-semantic",
                Rect::new(2, 0, 2, 1),
                "content",
            ));
            frame.set_cursor(Point::new(3, 0), true);
        }

        let area = Rect::new(0, 0, 4, 1);
        let mut complete = Terminal::new(Vec::new(), area);
        complete.draw(first).unwrap();
        complete.draw(second).unwrap();
        let mut partial = Terminal::new(Vec::new(), area);
        partial.draw(first).unwrap();
        partial
            .draw_damage(Damage::Regions(vec![Rect::new(0, 0, 2, 1)]), second)
            .unwrap();

        assert_eq!(partial.retained_buffer(), complete.retained_buffer());
        for point in [Point::new(0, 0), Point::new(3, 0)] {
            assert_eq!(
                partial
                    .hits()
                    .hit_test(point)
                    .map(|hit| hit.id().as_str().to_owned()),
                complete
                    .hits()
                    .hit_test(point)
                    .map(|hit| hit.id().as_str().to_owned())
            );
        }
        assert_eq!(partial.cursor(), complete.cursor());
        assert_eq!(partial.selection(), complete.selection());
        assert_eq!(
            partial.semantics().regions().len(),
            complete.semantics().regions().len()
        );
        for expected in complete.semantics().regions() {
            assert_eq!(
                partial
                    .semantics()
                    .regions()
                    .iter()
                    .find(|region| region.id == expected.id),
                Some(expected)
            );
        }
        assert_eq!(partial.image_scene(), complete.image_scene());
    }

    #[test]
    fn region_draw_without_retained_frame_promotes_to_full() {
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 2, 1));

        let stats = terminal
            .draw_damage(Damage::Regions(vec![Rect::new(0, 0, 1, 1)]), |frame| {
                frame.fill(LocalRect::new(0, 0, 1, 1), "X", Style::new());
            })
            .unwrap();

        assert!(stats.full_repaint);
        assert_eq!(stats.changed_cells, 2);
    }

    #[test]
    fn terminal_exposes_only_the_latest_frame_image_contributions() {
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 1, 1));
        terminal
            .draw(|frame| {
                frame.push_image(ImageContribution::Remove(ImageKey::new("old")));
            })
            .unwrap();
        assert_eq!(terminal.images().len(), 1);

        terminal.draw(|_| {}).unwrap();

        assert_eq!(terminal.images(), []);
    }

    #[test]
    fn terminal_reconciles_frame_images_after_rendering() {
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 4, 2));
        let placement = ImagePlacement {
            fit: crate::image::ImageFit::Stretch,
            allocation: None,
            key: ImageKey::new("diagram"),
            payload: ImagePayload::Png {
                bytes: vec![1, 2, 3],
                width: 1,
                height: 1,
            },
            destination: Rect::new(1, 0, 2, 1),
            clip: Rect::new(0, 0, 4, 2),
            lifecycle: ImageLifecycle::Frame,
        };

        terminal
            .draw(|frame| frame.push_image(ImageContribution::Present(placement.clone())))
            .unwrap();
        assert_eq!(terminal.image_delta().upserted, [placement]);
        assert_eq!(
            terminal.image_delta().removed,
            [] as [crate::image::ImageKey; 0]
        );
        assert_eq!(terminal.image_scene().placements().len(), 1);

        terminal.draw(|_| {}).unwrap();
        assert_eq!(
            terminal.image_delta().upserted,
            [] as [crate::image::ImagePlacement; 0]
        );
        assert_eq!(terminal.image_delta().removed, [ImageKey::new("diagram")]);
        assert_eq!(terminal.image_scene().placements().len(), 0);
    }
}

#[cfg(test)]
mod pointer_output_tests {
    use super::{PointerContext, Terminal};
    use crate::event::{Event, MouseEvent, MouseEventKind};
    use crate::geometry::{Point, Rect};
    use crate::hit::{HitRegion, HitRole};
    use crate::pointer::PointerIntent;
    use bmux_terminal_protocol::pointer_output::PointerShapeCapability;
    use std::io::{self, Write};

    #[derive(Default)]
    struct Output {
        bytes: Vec<u8>,
        fail_flush: bool,
    }
    impl Write for Output {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            if self.fail_flush {
                Err(io::Error::other("injected flush"))
            } else {
                Ok(())
            }
        }
    }
    fn terminal() -> Terminal<Output> {
        let mut terminal = Terminal::new(Output::default(), Rect::new(0, 0, 4, 1));
        terminal.set_pointer_capability(PointerShapeCapability::Set);
        terminal.observe_event(&Event::Mouse(MouseEvent::new(
            MouseEventKind::Move,
            Point::new(0, 0),
        )));
        terminal
    }
    fn contains(bytes: &[u8], value: &[u8]) -> bool {
        bytes.windows(value.len()).any(|bytes| bytes == value)
    }

    #[test]
    fn stationary_scene_changes_and_setter_dedup_follow_commits() {
        let mut terminal = terminal();
        terminal
            .draw(|cx| {
                cx.push_hit(HitRegion::new("target", Rect::new(0, 0, 4, 1)).role(HitRole::Action));
            })
            .unwrap();
        assert!(contains(&terminal.writer().bytes, b"\x1b]22;pointer\x1b\\"));
        terminal.writer_mut().bytes.clear();
        terminal
            .draw(|cx| {
                cx.push_hit(HitRegion::new("target", Rect::new(0, 0, 4, 1)).role(HitRole::Action));
            })
            .unwrap();
        assert!(!contains(&terminal.writer().bytes, b"\x1b]22;"));
        terminal.writer_mut().bytes.clear();
        terminal
            .draw(|cx| {
                cx.push_hit(
                    HitRegion::new("target", Rect::new(0, 0, 4, 1)).role(HitRole::TextInput),
                );
            })
            .unwrap();
        assert!(contains(&terminal.writer().bytes, b"\x1b]22;text\x1b\\"));
    }

    #[test]
    fn failed_flush_preserves_hit_authority_and_retries_setter() {
        let mut terminal = terminal();
        terminal
            .draw(|cx| cx.push_hit(HitRegion::new("old", Rect::new(0, 0, 4, 1))))
            .unwrap();
        terminal.writer_mut().fail_flush = true;
        assert!(
            terminal
                .draw(|cx| cx.push_hit(
                    HitRegion::new("new", Rect::new(0, 0, 4, 1)).role(HitRole::TextInput)
                ))
                .is_err()
        );
        assert_eq!(terminal.hits().regions()[0].id.as_str(), "old");
        terminal.writer_mut().fail_flush = false;
        terminal.writer_mut().bytes.clear();
        terminal
            .draw(|cx| {
                cx.push_hit(HitRegion::new("new", Rect::new(0, 0, 4, 1)).role(HitRole::TextInput));
            })
            .unwrap();
        assert!(contains(&terminal.writer().bytes, b"\x1b]22;text\x1b\\"));
    }

    #[test]
    fn capture_uses_caller_authority_and_hidden_never_controls_caret() {
        let mut terminal = terminal();
        terminal.set_pointer_context(PointerContext {
            scope: None,
            capture: Some("drag".into()),
        });
        terminal
            .draw(|cx| {
                cx.push_hit(
                    HitRegion::new("drag", Rect::new(2, 0, 2, 1)).role(HitRole::DragHandle),
                );
            })
            .unwrap();
        assert!(contains(
            &terminal.writer().bytes,
            b"\x1b]22;grabbing\x1b\\"
        ));
        terminal.writer_mut().bytes.clear();
        terminal
            .draw(|cx| {
                cx.push_hit(
                    HitRegion::new("drag", Rect::new(2, 0, 2, 1))
                        .pointer_intent(PointerIntent::Hidden),
                );
            })
            .unwrap();
        assert!(contains(&terminal.writer().bytes, b"\x1b]22;default\x1b\\"));
        // Caret commands remain entirely governed by ordinary frame output.
        assert!(!contains(&terminal.writer().bytes, b"\x1b]22;none"));
    }

    #[test]
    fn closing_modal_uses_new_scope_with_stationary_pointer_and_stale_context() {
        let mut terminal = terminal();
        let scope = crate::focus::FocusScopeId::new("modal");
        terminal.set_pointer_context(PointerContext {
            scope: Some(scope.clone()),
            capture: Some("modal-input".into()),
        });
        terminal
            .draw_damage_with_prepared_output_hooks(
                crate::damage::Damage::Full,
                |cx| {
                    cx.push_hit(
                        HitRegion::new("background", Rect::new(0, 0, 4, 1)).role(HitRole::Action),
                    );
                    cx.push_hit(
                        HitRegion::new("modal-input", Rect::new(0, 0, 4, 1))
                            .role(HitRole::TextInput)
                            .focus_scope(scope.clone()),
                    );
                },
                |prepared| {
                    assert!(prepared.set_modal_scopes(std::slice::from_ref(&scope)));
                    Ok(())
                },
                |_| Ok(false),
                |_, _, _| Ok(()),
            )
            .unwrap();
        assert!(contains(&terminal.writer().bytes, b"\x1b]22;text\x1b\\"));
        terminal.writer_mut().bytes.clear();
        // The application's router still describes the previously committed modal
        // until this frame flushes. The new prepared None scope must win now.
        terminal
            .draw_damage_with_prepared_output_hooks(
                crate::damage::Damage::Full,
                |cx| {
                    cx.push_hit(
                        HitRegion::new("background", Rect::new(0, 0, 4, 1)).role(HitRole::Action),
                    );
                },
                |prepared| {
                    assert!(prepared.set_modal_scopes(&[]));
                    Ok(())
                },
                |_| Ok(false),
                |_, _, _| Ok(()),
            )
            .unwrap();
        assert!(terminal.focus().active_scope().is_none());
        assert!(contains(&terminal.writer().bytes, b"\x1b]22;pointer\x1b\\"));
        terminal.writer_mut().bytes.clear();
        terminal
            .draw_damage(crate::damage::Damage::None, |_| {
                panic!("no paint for pointer-only output")
            })
            .unwrap();
        assert!(
            terminal.writer().bytes.is_empty(),
            "stale context must not emit default after closing"
        );
    }

    #[test]
    fn pointer_only_hidden_falls_back_to_default_and_retries_failed_flush() {
        let mut terminal = terminal();
        terminal
            .draw(|cx| {
                cx.push_hit(HitRegion::new("action", Rect::new(0, 0, 2, 1)).role(HitRole::Action));
                cx.push_hit(
                    HitRegion::new("hidden", Rect::new(2, 0, 2, 1))
                        .pointer_intent(PointerIntent::Hidden),
                );
            })
            .unwrap();
        assert!(contains(&terminal.writer().bytes, b"\x1b]22;pointer\x1b\\"));
        terminal.set_pointer_context(PointerContext {
            scope: None,
            capture: Some("hidden".into()),
        });
        terminal.writer_mut().bytes.clear();
        terminal.writer_mut().fail_flush = true;
        assert!(
            terminal
                .draw_damage(crate::damage::Damage::None, |_| panic!("no paint"))
                .is_err()
        );
        terminal.writer_mut().bytes.clear();
        terminal.writer_mut().fail_flush = false;
        terminal
            .draw_damage(crate::damage::Damage::None, |_| panic!("no paint"))
            .unwrap();
        assert_eq!(terminal.writer().bytes, b"\x1b]22;default\x1b\\");
        terminal.writer_mut().bytes.clear();
        terminal
            .draw_damage(crate::damage::Damage::None, |_| panic!("no paint"))
            .unwrap();
        assert_eq!(terminal.writer().bytes, [] as [u8; 0]);
    }

    #[test]
    fn acknowledgment_does_not_erase_newer_pointer_observation() {
        let mut terminal = terminal();
        let mut candidate = terminal.fork(Vec::new());
        candidate
            .draw(|cx| {
                cx.push_hit(HitRegion::new("target", Rect::new(0, 0, 1, 1)).role(HitRole::Action));
            })
            .unwrap();
        terminal.observe_event(&Event::Mouse(MouseEvent::new(
            MouseEventKind::Move,
            Point::new(3, 0),
        )));
        terminal.acknowledge(candidate);
        terminal.writer_mut().bytes.clear();
        terminal
            .draw(|cx| {
                cx.push_hit(HitRegion::new("target", Rect::new(0, 0, 1, 1)).role(HitRole::Action));
            })
            .unwrap();
        assert!(contains(&terminal.writer().bytes, b"\x1b]22;default\x1b\\"));
        terminal.restore_pointer().unwrap();
        assert!(contains(&terminal.writer().bytes, b"\x1b]22;\x1b\\"));
    }
}
