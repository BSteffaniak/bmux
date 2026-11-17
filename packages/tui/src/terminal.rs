//! Terminal runtime abstraction.

use std::io::{self, Write};

use crate::ansi::{AnsiFrameDiffStats, write_ansi_frame_with_color_depth};
use crate::buffer::Buffer;
use crate::capabilities::TerminalColorDepth;
use crate::damage::Damage;
use crate::event::{Event, FocusEvent};
use crate::focus::{FocusId, FocusTrap};
use crate::frame::{Cursor, Frame, OverChanges};
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

/// Whether presentation may move rows with terminal scroll regions.
///
/// Scroll acceleration applies to frames prepared with [`Damage::Scroll`] and
/// to frames whose components reported a scroll while painting
/// ([`crate::paint::PaintCx::hint_scroll`]). It changes only the bytes
/// written, never the visible result: the remaining cells are diffed against
/// a model of the scrolled screen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ScrollRegionAcceleration {
    /// Always repaint changed cells; never emit scroll regions.
    #[default]
    Off,
    /// Emit a scroll region (DECSTBM with SU/SD) when it is safe, and fall
    /// back to the cell diff otherwise.
    ///
    /// It is safe when a committed frame is retained and is not being
    /// repainted, the frame has no local grid fragments, the shift is smaller
    /// than the region, and no image placement of the committed or the new
    /// frame overlaps the moved cells (graphics protocols do not scroll with
    /// text consistently). Each painted hint is scrolled on its own, so
    /// several panes can scroll in one frame, as long as the cells they move
    /// do not overlap.
    /// Declared [`Damage::Scroll`] must span the full presentation width. A
    /// painted scroll hint narrower than the presentation scrolls its full
    /// rows instead when that leaves fewer cells to rewrite than not
    /// scrolling (for example when the columns beside it are blank or
    /// repeat on every row). The presentation area must span the terminal's
    /// full width, as it does for full-screen presenters; surfaces narrower
    /// than the terminal must keep this `Off`.
    Auto,
    /// [`Self::Auto`], and a painted scroll hint narrower than the
    /// presentation may also scroll only its own columns with left/right
    /// margins (DEC private mode 69, DECSLRM), whichever leaves the fewest
    /// cells to rewrite. Select it only for terminals known to implement
    /// left/right margin mode (see
    /// [`crate::capabilities::TerminalCapabilities::left_right_margins`]):
    /// elsewhere the margin sequence is misread and the screen is corrupted.
    AutoWithMargins,
}

/// Rewritten cells a scroll must save before it is emitted, covering the
/// scroll sequence itself.
const SCROLL_MIN_SAVED_CELLS: usize = 16;

/// A terminal scroll that moved committed rows during the last draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppliedScroll {
    /// Cells that moved: full-width rows, or the columns inside left/right
    /// margins under [`ScrollRegionAcceleration::AutoWithMargins`].
    pub region: Rect,
    /// Signed rows moved; positive moves content toward row 0.
    pub rows: i64,
}

/// Encode `prepared` as a diff against `previous` (moving rows first for the planned
/// `scrolls`), or as a full frame without a retained frame. Returns the scrolls emitted.
fn encode_frame(
    encoded: &mut Vec<u8>,
    previous: Option<&Buffer>,
    prepared: &PreparedPresentation,
    depth: TerminalColorDepth,
    scrolls: &[AppliedScroll],
    model: &mut Buffer,
) -> io::Result<(DrawStats, Vec<AppliedScroll>)> {
    let Some(previous) = previous else {
        write_ansi_frame_with_color_depth(encoded, prepared.buffer(), prepared.cursor(), depth)?;
        return Ok((DrawStats::full(prepared.buffer().cells().len()), Vec::new()));
    };
    // A regional frame's cells equal the previous frame's outside its regions, so only the
    // regions are compared.
    let scope = prepared
        .damage()
        .is_regional()
        .then(|| prepared.damage().retained_regions());
    let moves: Vec<(Rect, i64)> = scrolls
        .iter()
        .map(|scroll| (scroll.region, scroll.rows))
        .collect();
    let (stats, emitted) = crate::ansi::scroll_moves_diff(
        encoded,
        crate::ansi::ScrollFrames {
            previous,
            current: prepared.buffer(),
            cursor: prepared.cursor(),
            depth,
            scope,
        },
        &moves,
        true,
        model,
        prepared.painted().map(|painted| crate::ansi::PaintedOver {
            painted,
            moves: prepared.moves(),
        }),
    )?;
    let applied = scrolls
        .iter()
        .zip(emitted)
        .filter(|(_, emitted)| *emitted)
        .map(|(scroll, _)| *scroll)
        .collect();
    Ok((stats.into(), applied))
}

/// Unchanged cells between two painted runs that are cheaper to rewrite than to move the cursor
/// over (a cursor move and a style change cost about this many bytes).
const PAINTED_RUN_GAP: u16 = 8;

/// Encode a frame painted in place: `scrolls` first, then every painted run of `buffer` (joined
/// across short gaps and widened to whole graphemes), then the cursor.
fn encode_painted_over(
    encoded: &mut Vec<u8>,
    buffer: &Buffer,
    changes: &OverChanges,
    scrolls: &[AppliedScroll],
    cursor: Option<Cursor>,
    depth: TerminalColorDepth,
) -> io::Result<DrawStats> {
    let area = buffer.area();
    encoded.extend_from_slice(b"\x1b[?25l");
    for scroll in scrolls {
        let full = scroll.region.x == area.x && scroll.region.width == area.width;
        crate::ansi::write_scroll(encoded, scroll.region, scroll.rows, full)?;
    }
    let mut style = crate::style::Style::new();
    let mut changed_cells = 0_usize;
    for y in area.y..area.bottom() {
        let mut pending: Option<(u16, u16)> = None;
        for run in changes.runs(y) {
            match pending.as_mut() {
                Some((_, end)) if run.start <= end.saturating_add(PAINTED_RUN_GAP) => {
                    *end = run.end;
                }
                _ => {
                    if let Some((start, end)) = pending.replace((run.start, run.end)) {
                        changed_cells +=
                            write_painted_run(encoded, buffer, y, start, end, &mut style, depth)?;
                    }
                }
            }
        }
        if let Some((start, end)) = pending {
            changed_cells += write_painted_run(encoded, buffer, y, start, end, &mut style, depth)?;
        }
    }
    crate::ansi::write_ansi_style(encoded, crate::style::Style::new(), depth)?;
    crate::ansi::write_ansi_cursor(encoded, cursor)?;
    Ok(DrawStats {
        changed_cells,
        full_repaint: false,
    })
}

/// Write row `y`, columns `start..end` widened to whole graphemes, returning the cells written.
fn write_painted_run(
    encoded: &mut Vec<u8>,
    buffer: &Buffer,
    y: u16,
    mut start: u16,
    mut end: u16,
    style: &mut crate::style::Style,
    depth: TerminalColorDepth,
) -> io::Result<usize> {
    let area = buffer.area();
    if start > area.x
        && buffer
            .get(Point::new(start, y))
            .is_some_and(crate::buffer::Cell::is_wide_continuation)
    {
        start -= 1;
    }
    if end < area.right()
        && buffer
            .get(Point::new(end, y))
            .is_some_and(crate::buffer::Cell::is_wide_continuation)
    {
        end += 1;
    }
    crate::ansi::emit_row_span(encoded, buffer, y, start, end, style, depth)?;
    Ok(usize::from(end - start))
}

/// A simple ANSI terminal runtime backed by an arbitrary writer.
///
/// Why a fallible frame was not drawn (see [`Terminal::try_draw_over_with_output_hooks`]).
#[derive(Debug)]
pub enum TryDrawError<E> {
    /// The render rejected the frame; nothing was written or committed.
    Rejected(E),
    /// Output or a hook failed; nothing was committed.
    Io(io::Error),
}

/// What a frame painted in place over the last one changed: its cell moves, then the cells it
/// changed (see [`Terminal::last_changes`]).
pub struct FrameChanges<'terminal> {
    changes: &'terminal OverChanges,
}

impl FrameChanges<'_> {
    /// The frame's cell moves, in order: each area and the rows it moved by (positive up). A
    /// move blanks the rows it exposes.
    pub fn moves(&self) -> impl Iterator<Item = (Rect, i64)> + '_ {
        self.changes
            .moves
            .iter()
            .map(|moved| (moved.area, moved.rows))
    }

    /// The runs of cells the frame changed after its moves, by row: `(y, columns)`. A run may
    /// cut a wide grapheme; the whole grapheme changed.
    pub fn runs(&self) -> impl Iterator<Item = (u16, std::ops::Range<u16>)> + '_ {
        let area = self.changes.area();
        (area.y..area.bottom()).flat_map(move |y| self.changes.runs(y).map(move |run| (y, run)))
    }
}

/// `Terminal` owns the previous frame buffer so repeated draws can use
/// damage-aware flushing.
pub struct Terminal<W> {
    writer: W,
    /// Reused staging for encoded cells, so a frame reaches the writer in one
    /// write however small its pieces (unbuffered or line-buffered writers
    /// such as `Stdout` would otherwise make many system calls per frame).
    encoded: Vec<u8>,
    /// Reused model of the screen after a terminal scroll, diffed against the next frame.
    scroll_model: Buffer,
    state: PresentationState,
    pointer_output: PointerOutput,
    pointer_capability: PointerShapeCapability,
    pointer_position: Option<Point>,
    pointer_context: PointerContext,
    color_depth: TerminalColorDepth,
    scroll_acceleration: ScrollRegionAcceleration,
    committed_damage: Damage,
    applied_scrolls: Vec<AppliedScroll>,
    /// Change tracking reused by frames painted over the last one.
    over_storage: Option<OverChanges>,
    /// The last frame was painted in place over the one before, and `over_storage` holds what it
    /// changed.
    last_over: bool,
    /// Frames are encoded and written; when off, changes are only tracked (see
    /// [`Terminal::set_encoding`]).
    encoding: bool,
    /// Frames are wrapped in a synchronized update (DEC mode 2026).
    synchronized_output: bool,
}

/// Write a frame with `frame`, inside a synchronized update (DEC mode 2026) when `synchronized`.
/// The update is ended even when the frame fails, so the host does not stay frozen; the frame's
/// error comes first.
fn write_synchronized<W: Write>(
    writer: &mut W,
    synchronized: bool,
    frame: impl FnOnce(&mut W) -> io::Result<DrawStats>,
) -> io::Result<DrawStats> {
    if synchronized {
        writer.write_all(b"\x1b[?2026h")?;
    }
    let output = frame(writer);
    let end = if synchronized {
        writer.write_all(b"\x1b[?2026l")
    } else {
        Ok(())
    };
    let stats = output?;
    end?;
    Ok(stats)
}

impl<W: Write> Terminal<W> {
    /// Create a terminal runtime for `area`.
    #[must_use]
    pub fn new(writer: W, area: Rect) -> Self {
        Self {
            writer,
            encoded: Vec::new(),
            scroll_model: Buffer::empty(Rect::new(0, 0, 0, 0)),
            state: PresentationState::new(area),
            pointer_output: PointerOutput::new(
                bmux_terminal_protocol::pointer_output::tmux_passthrough_enabled(),
            ),
            pointer_capability: PointerShapeCapability::detect(),
            pointer_position: None,
            pointer_context: PointerContext::default(),
            color_depth: TerminalColorDepth::TrueColor,
            scroll_acceleration: ScrollRegionAcceleration::Off,
            committed_damage: Damage::None,
            applied_scrolls: Vec::new(),
            over_storage: None,
            last_over: false,
            encoding: true,
            synchronized_output: true,
        }
    }

    /// Fork committed authority into a staging terminal with a different writer.
    /// The original remains authoritative until the caller acknowledges delivery.
    #[must_use]
    pub fn fork<V: Write>(&self, writer: V) -> Terminal<V> {
        Terminal {
            writer,
            encoded: Vec::new(),
            scroll_model: Buffer::empty(Rect::new(0, 0, 0, 0)),
            state: self.state.clone(),
            pointer_output: self.pointer_output.clone(),
            pointer_capability: self.pointer_capability,
            pointer_position: self.pointer_position,
            pointer_context: self.pointer_context.clone(),
            color_depth: self.color_depth,
            scroll_acceleration: self.scroll_acceleration,
            committed_damage: self.committed_damage.clone(),
            applied_scrolls: self.applied_scrolls.clone(),
            over_storage: None,
            last_over: false,
            encoding: true,
            synchronized_output: self.synchronized_output,
        }
    }

    /// Move committed authority into a staging terminal with a different
    /// writer, like [`Self::fork`] but taking over the retained cells instead
    /// of copying them (see [`PresentationState::stage`]).
    ///
    /// Until the staging terminal is acknowledged ([`Self::acknowledge`]),
    /// this terminal keeps its interactions and focus but no retained cells,
    /// so it would repaint fully; a failed delivery is handled with
    /// [`Self::invalidate_after_failed_delivery`] as for a fork.
    #[must_use]
    pub fn stage<V: Write>(&mut self, writer: V) -> Terminal<V> {
        Terminal {
            writer,
            encoded: Vec::new(),
            scroll_model: std::mem::replace(
                &mut self.scroll_model,
                Buffer::empty(Rect::new(0, 0, 0, 0)),
            ),
            state: self.state.stage(),
            pointer_output: self.pointer_output.clone(),
            pointer_capability: self.pointer_capability,
            pointer_position: self.pointer_position,
            pointer_context: self.pointer_context.clone(),
            color_depth: self.color_depth,
            scroll_acceleration: self.scroll_acceleration,
            committed_damage: self.committed_damage.clone(),
            applied_scrolls: self.applied_scrolls.clone(),
            over_storage: None,
            last_over: false,
            encoding: true,
            synchronized_output: self.synchronized_output,
        }
    }

    /// Adopt an exactly delivered staging terminal's authority, retaining this writer.
    pub fn acknowledge<V>(&mut self, staged: Terminal<V>) {
        self.scroll_model = staged.scroll_model;
        self.state = staged.state;
        self.pointer_output = staged.pointer_output;
        self.committed_damage = staged.committed_damage;
        self.applied_scrolls = staged.applied_scrolls;
    }

    /// Write raw control bytes (for example terminal capability queries)
    /// outside of frame presentation and flush them.
    ///
    /// # Errors
    ///
    /// Returns any writer error.
    pub fn write_control(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()
    }

    /// Whether scroll damage may be presented with terminal scroll regions.
    ///
    /// Defaults to [`ScrollRegionAcceleration::Off`]. Staging terminals
    /// created by [`Self::fork`] inherit the current value.
    #[must_use]
    pub const fn scroll_region_acceleration(&self) -> ScrollRegionAcceleration {
        self.scroll_acceleration
    }

    /// Select whether frames are encoded and written (the default). Off, a frame painted over
    /// the last one is committed and its change tracked (see [`Self::last_changes`]) without
    /// encoding or writing anything: for a frame kept only for whoever mirrors it.
    pub const fn set_encoding(&mut self, enabled: bool) {
        self.encoding = enabled;
    }

    /// Select whether frames are wrapped in a synchronized update (DEC mode 2026), for a host
    /// known to support it or one whose support is unknown and harmless to ask (the default).
    pub const fn set_synchronized_output(&mut self, enabled: bool) {
        self.synchronized_output = enabled;
    }

    /// Select whether scroll damage may use terminal scroll regions.
    pub const fn set_scroll_region_acceleration(&mut self, mode: ScrollRegionAcceleration) {
        self.scroll_acceleration = mode;
    }

    /// Effective damage of the most recent successful draw: the damage the
    /// frame was prepared with after promotion (for example, `Full` for the
    /// first frame or after a reset), or [`Damage::None`] when that draw had
    /// nothing to present. `None` before the first draw.
    #[must_use]
    pub const fn committed_damage(&self) -> &Damage {
        &self.committed_damage
    }

    /// The largest terminal scroll the most recent successful draw emitted, if any.
    #[must_use]
    pub fn applied_scroll(&self) -> Option<AppliedScroll> {
        self.applied_scrolls.first().copied()
    }

    /// Every terminal scroll the most recent successful draw emitted, largest first.
    #[must_use]
    pub fn applied_scrolls(&self) -> &[AppliedScroll] {
        &self.applied_scrolls
    }

    /// What the last frame changed when it was painted in place over the one before (see
    /// [`Self::draw_over`]): applying its moves to that frame's cells, then setting its changed
    /// runs from [`Self::retained_buffer`], gives the frame. `None` after any other frame.
    #[must_use]
    pub fn last_changes(&self) -> Option<FrameChanges<'_>> {
        self.over_storage
            .as_ref()
            .filter(|_| self.last_over)
            .map(|changes| FrameChanges { changes })
    }

    /// Color depth frames are reduced to when encoded.
    ///
    /// Defaults to [`TerminalColorDepth::TrueColor`], which emits colors unchanged.
    #[must_use]
    pub const fn color_depth(&self) -> TerminalColorDepth {
        self.color_depth
    }

    /// Reduce colors in subsequently encoded frames to `depth`.
    ///
    /// Applications typically pass [`crate::capabilities::TerminalCapabilities`]
    /// detection or an explicit user override. Changing depth does not repaint
    /// cells already on screen; request a full repaint to apply it everywhere.
    pub const fn set_color_depth(&mut self, depth: TerminalColorDepth) {
        self.color_depth = depth;
    }

    /// Host pointer-shape capability (detected from the environment unless
    /// set explicitly).
    #[must_use]
    pub const fn pointer_capability(&self) -> PointerShapeCapability {
        self.pointer_capability
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
    pub fn retained_buffer(&self) -> Option<&Buffer> {
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
        self.last_over = false;
        self.state.reset();
    }

    /// Set the presentation clock given to subsequent frames.
    ///
    /// See [`crate::presentation::PresentationState::set_presentation_clock`]. Staging terminals
    /// created by [`Self::fork`] inherit the current value.
    pub const fn set_presentation_clock(&mut self, clock: Option<crate::clock::PresentationClock>) {
        self.state.set_presentation_clock(clock);
    }

    /// The presentation clock given to subsequent frames.
    #[must_use]
    pub const fn presentation_clock(&self) -> Option<crate::clock::PresentationClock> {
        self.state.presentation_clock()
    }

    /// Resize the terminal area and force the next draw to repaint fully.
    pub fn resize(&mut self, area: Rect) {
        if area != self.state.area() {
            self.last_over = false;
        }
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

    /// Draw a frame painted over the committed one: what `render` paints replaces what the
    /// terminal shows and nothing else changes.
    ///
    /// This is for output already planned as changes, such as a multiplexer's: the frame is
    /// painted in place over the committed cells, and every painted cell is written without
    /// comparing it to what it replaced (writing a cell that did not change is harmless, and
    /// the caller painted it because it changed). Cell moves
    /// ([`PaintCx::move_cells`](crate::paint::PaintCx::move_cells)) are emitted as scroll
    /// regions where the terminal can hold them, or else rewritten. The committed hit regions
    /// and other interaction metadata stay as they were; only the cells and cursor change.
    /// Without committed cells the frame is a full one painted over a blank screen.
    ///
    /// # Errors
    /// Returns any I/O error reported by the backend writer.
    pub fn draw_over(
        &mut self,
        render: impl FnOnce(&mut PaintCx<'_, '_>),
    ) -> io::Result<DrawStats> {
        self.draw_over_inner(render, |_| Ok(false), |_, _, _| Ok(()), true)
    }

    /// [`Self::draw_over_with_output_hooks`] for a render that may reject its frame, and that
    /// says whether the frame writes anything besides its cells (`Ok(true)`: the hooks have
    /// output). A frame that changes no cell, keeps the cursor and has no other output writes
    /// nothing and returns `Ok(None)`.
    ///
    /// A rejected frame writes and commits nothing; the terminal then holds no committed cells,
    /// so the next frame repaints in full.
    ///
    /// # Errors
    /// Returns the render's rejection, or output and hook errors without committing the frame.
    pub fn try_draw_over_with_output_hooks<E>(
        &mut self,
        render: impl FnOnce(&mut PaintCx<'_, '_>) -> Result<bool, E>,
        before: impl FnOnce(&mut W) -> io::Result<bool>,
        overlay: impl FnOnce(&mut W, &ImageScene, &ImageSceneDelta) -> io::Result<()>,
    ) -> Result<Option<DrawStats>, TryDrawError<E>> {
        self.draw_over_core(render, before, overlay, true)
    }

    /// [`Self::draw_over`] with the output hooks of
    /// [`Self::draw_damage_with_output_hooks`]; `overlay` receives the committed image scene. The
    /// frame is written even when it paints nothing, so the hooks always run.
    ///
    /// # Errors
    /// Returns output or hook errors without committing failed output.
    pub fn draw_over_with_output_hooks(
        &mut self,
        render: impl FnOnce(&mut PaintCx<'_, '_>),
        before: impl FnOnce(&mut W) -> io::Result<bool>,
        overlay: impl FnOnce(&mut W, &ImageScene, &ImageSceneDelta) -> io::Result<()>,
    ) -> io::Result<DrawStats> {
        self.draw_over_inner(render, before, overlay, false)
    }

    fn draw_over_inner(
        &mut self,
        render: impl FnOnce(&mut PaintCx<'_, '_>),
        before: impl FnOnce(&mut W) -> io::Result<bool>,
        overlay: impl FnOnce(&mut W, &ImageScene, &ImageSceneDelta) -> io::Result<()>,
        skip_unchanged: bool,
    ) -> io::Result<DrawStats> {
        let painted = |cx: &mut PaintCx<'_, '_>| {
            render(cx);
            Ok::<bool, std::convert::Infallible>(false)
        };
        match self.draw_over_core(painted, before, overlay, skip_unchanged) {
            Ok(stats) => Ok(stats.unwrap_or(DrawStats {
                changed_cells: 0,
                full_repaint: false,
            })),
            Err(TryDrawError::Io(error)) => Err(error),
            Err(TryDrawError::Rejected(never)) => match never {},
        }
    }

    #[allow(clippy::too_many_lines)] // One ordered frame: paint, decide, encode, commit.
    fn draw_over_core<E>(
        &mut self,
        render: impl FnOnce(&mut PaintCx<'_, '_>) -> Result<bool, E>,
        before: impl FnOnce(&mut W) -> io::Result<bool>,
        overlay: impl FnOnce(&mut W, &ImageScene, &ImageSceneDelta) -> io::Result<()>,
        skip_unchanged: bool,
    ) -> Result<Option<DrawStats>, TryDrawError<E>> {
        self.last_over = false;
        let Some(mut buffer) = self.state.take_retained() else {
            // Nothing to paint over: a full frame, which a rejection leaves uncommitted.
            let mut outcome = None;
            let prepared = self
                .state
                .prepare(Damage::Full, |cx| outcome = Some(render(cx)));
            if let Some(Err(rejection)) = outcome {
                return Err(TryDrawError::Rejected(rejection));
            }
            return self
                .draw_prepared_with_output_hooks(prepared, before, overlay)
                .map(Some)
                .map_err(TryDrawError::Io);
        };
        let area = buffer.area();
        let (outcome, mut changes, cursor, declared_images) = {
            let mut frame = Frame::new(&mut buffer);
            frame.set_clock(self.state.clock());
            frame.track_over(self.over_storage.take());
            let outcome = PresentationPhase::Paint
                .measure(|| render(&mut PaintCx::new(&mut frame)), |_| true);
            let cursor = frame.cursor();
            let declared_images = frame.images_declared().then(|| frame.take_images());
            (
                outcome,
                frame
                    .take_over()
                    .unwrap_or_else(|| OverChanges::empty(area)),
                cursor,
                declared_images,
            )
        };
        let extra_output = match outcome {
            Ok(extra_output) => extra_output,
            Err(rejection) => {
                // The lent cells were painted but will not be shown: drop them, so the next
                // frame repaints in full.
                self.over_storage = Some(changes);
                self.applied_scrolls.clear();
                return Err(TryDrawError::Rejected(rejection));
            }
        };
        // A frame that declared its image set replaces the scene with it; one that did not keeps
        // the scene.
        let images = declared_images.map(|(images, occlusions)| {
            let mut scene = self.state.image_scene().clone();
            let delta = scene.reconcile_occluded(&images, &occlusions);
            (images, scene, delta)
        });
        let image_delta_empty = images
            .as_ref()
            .is_none_or(|(_, _, delta)| delta.upserted.is_empty() && delta.removed.is_empty());
        // Painting nothing, with the cursor where it was, the images as they were and nothing
        // else to write, changes nothing.
        if skip_unchanged
            && !extra_output
            && image_delta_empty
            && changes.is_clean()
            && changes.moves.is_empty()
            && cursor == self.state.cursor()
        {
            self.over_storage = Some(changes);
            self.state.install_painted_over(buffer, cursor);
            if let Some((images, scene, delta)) = images {
                self.state.install_painted_images(images, scene, delta);
            }
            self.committed_damage = Damage::None;
            self.applied_scrolls.clear();
            self.last_over = true;
            return Ok(None);
        }
        if !self.encoding {
            // Changes are tracked for whoever mirrors the frame; nothing is written.
            let changed_cells = changes.marked_cells();
            self.over_storage = Some(changes);
            self.state.install_painted_over(buffer, cursor);
            if let Some((images, scene, delta)) = images {
                self.state.install_painted_images(images, scene, delta);
            }
            self.committed_damage = Damage::Regions(Vec::new());
            self.applied_scrolls.clear();
            self.last_over = true;
            return Ok(Some(DrawStats {
                changed_cells,
                full_repaint: false,
            }));
        }
        let scrolls = self.over_scrolls(
            &mut changes,
            area,
            images.as_ref().map(|(_, scene, _)| scene),
        );
        let position = self.pointer_position;
        let shape = match resolve_pointer_intent(
            self.state.hits(),
            position,
            self.state.focus().active_scope(),
            self.pointer_context.capture.as_ref(),
        ) {
            PointerIntent::Shape(shape) => shape,
            PointerIntent::Auto | PointerIntent::Hidden => PointerShape::Default,
        };
        let transaction = self
            .pointer_output
            .begin_present(self.pointer_capability, shape)
            .map_err(TryDrawError::Io)?;
        let depth = self.color_depth;
        let synchronized = self.synchronized_output;
        let writer = &mut self.writer;
        let encoded = &mut self.encoded;
        let no_delta = ImageSceneDelta::default();
        let (image_scene, image_delta) = images.as_ref().map_or_else(
            || (self.state.image_scene(), &no_delta),
            |(_, scene, delta)| (scene, delta),
        );
        let result = (|| -> io::Result<DrawStats> {
            let stats = write_synchronized(writer, synchronized, |writer| {
                let repaint = before(writer)?;
                encoded.clear();
                let stats = PresentationPhase::AnsiEncode.measure(
                    || -> io::Result<DrawStats> {
                        if repaint {
                            write_ansi_frame_with_color_depth(encoded, &buffer, cursor, depth)?;
                            return Ok(DrawStats::full(buffer.cells().len()));
                        }
                        encode_painted_over(encoded, &buffer, &changes, &scrolls, cursor, depth)
                    },
                    Result::is_ok,
                )?;
                writer.write_all(encoded)?;
                overlay(writer, image_scene, image_delta)?;
                if let Some(transaction) = &transaction {
                    writer.write_all(transaction.bytes())?;
                }
                Ok(stats)
            })?;
            PresentationPhase::WriterFlush.measure(|| writer.flush(), Result::is_ok)?;
            Ok(stats)
        })();
        if let Some(transaction) = transaction {
            self.pointer_output.complete(transaction, result.is_ok());
        }
        self.over_storage = Some(changes);
        match result {
            Ok(stats) => {
                self.state.install_painted_over(buffer, cursor);
                if let Some((images, scene, delta)) = images {
                    self.state.install_painted_images(images, scene, delta);
                }
                self.committed_damage = Damage::Regions(Vec::new());
                self.applied_scrolls = scrolls;
                self.last_over = true;
                Ok(Some(stats))
            }
            Err(error) => {
                // The lent cells were painted but not shown: the next frame repaints in full.
                self.state.reset();
                self.applied_scrolls.clear();
                Err(TryDrawError::Io(error))
            }
        }
    }

    /// The scrolls that show a frame's moves, all of them or none: a move the terminal cannot
    /// hold (an image under it, no left/right margins for a narrow area, a shift as tall as
    /// the area) leaves every moved area to be rewritten.
    fn over_scrolls(
        &self,
        changes: &mut OverChanges,
        area: Rect,
        next_images: Option<&ImageScene>,
    ) -> Vec<AppliedScroll> {
        // Images shown now and images about to be shown alike.
        let images: Vec<Rect> = self
            .state
            .image_scene()
            .placements()
            .chain(next_images.into_iter().flat_map(ImageScene::placements))
            .map(|placement| placement.destination.intersection(placement.clip))
            .collect();
        let shown = changes.moves.iter().all(|moved| {
            let full = moved.area.x == area.x && moved.area.width == area.width;
            self.scroll_acceleration != ScrollRegionAcceleration::Off
                && moved.rows != 0
                && moved.rows.unsigned_abs() < u64::from(moved.area.height)
                && images
                    .iter()
                    .all(|image| image.intersection(moved.area).is_empty())
                && (full
                    || (self.scroll_acceleration == ScrollRegionAcceleration::AutoWithMargins
                        && moved.clean_edges))
        });
        if shown {
            changes
                .moves
                .iter()
                .map(|moved| AppliedScroll {
                    region: moved.area,
                    rows: moved.rows,
                })
                .collect()
        } else {
            let areas: Vec<Rect> = changes.moves.iter().map(|moved| moved.area).collect();
            for moved in areas {
                changes.mark(moved);
            }
            Vec::new()
        }
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
        self.last_over = false;
        let damage = prepared
            .as_ref()
            .map_or(Damage::None, |prepared| prepared.damage().clone());
        let scrolls = prepared
            .as_ref()
            .map(|prepared| self.scroll_plan(prepared))
            .unwrap_or_default();
        let position = self.pointer_position;
        let context = &self.pointer_context;
        let pointer_output = &mut self.pointer_output;
        let capability = self.pointer_capability;
        if prepared.is_none() {
            self.committed_damage = Damage::None;
            self.applied_scrolls.clear();
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
        let encoded = &mut self.encoded;
        let scroll_model = &mut self.scroll_model;
        let depth = self.color_depth;
        let synchronized = self.synchronized_output;
        let mut applied = Vec::new();
        let result = self
            .state
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
                    let stats = write_synchronized(writer, synchronized, |writer| {
                        let repaint = before(writer)?;
                        encoded.clear();
                        let stats = PresentationPhase::AnsiEncode.measure(
                            || -> io::Result<DrawStats> {
                                let (stats, scrolled) = encode_frame(
                                    encoded,
                                    previous.filter(|_| !repaint),
                                    prepared,
                                    depth,
                                    &scrolls,
                                    scroll_model,
                                )?;
                                applied = scrolled;
                                Ok(stats)
                            },
                            Result::is_ok,
                        )?;
                        writer.write_all(encoded)?;
                        overlay(writer, prepared.image_scene(), prepared.image_delta())?;
                        if let Some(transaction) = &transaction {
                            writer.write_all(transaction.bytes())?;
                        }
                        Ok(stats)
                    })?;
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
            });
        if result.is_ok() {
            self.committed_damage = damage;
            self.applied_scrolls = applied;
        }
        result
    }

    /// The scrolls to emit for `prepared` under the selected acceleration: a declared scroll,
    /// or one per painted hint that passes every safety condition (see
    /// [`ScrollRegionAcceleration::Auto`]), largest first, none overlapping another.
    fn scroll_plan(&self, prepared: &PreparedPresentation) -> Vec<AppliedScroll> {
        if self.scroll_acceleration == ScrollRegionAcceleration::Off {
            return Vec::new();
        }
        let Some(previous) = self.state.retained_buffer() else {
            return Vec::new();
        };
        let area = prepared.buffer().area();
        if previous.area() != area || !prepared.grid_fragments().is_empty() {
            return Vec::new();
        }
        // Graphics protocols do not scroll with text consistently: nothing that moves may
        // overlap an image, shown or about to be.
        let images: Vec<Rect> = prepared
            .image_scene()
            .placements()
            .chain(self.state.image_scene().placements())
            .map(|placement| placement.destination.intersection(placement.clip))
            .collect();
        let clear = |region: Rect| {
            images
                .iter()
                .all(|image| image.intersection(region).is_empty())
        };
        let movable = |region: Rect, rows: i64| {
            area.intersection(region) == region
                && rows != 0
                && rows.unsigned_abs() < u64::from(region.height)
                && clear(region)
        };
        if let Some((region, rows)) = prepared.damage().scroll_shift() {
            // Declared scroll damage keeps its contract: full-width regions only.
            let full = Rect::new(area.x, region.y, area.width, region.height);
            return (movable(region, rows) && region == full)
                .then_some(AppliedScroll { region, rows })
                .into_iter()
                .collect();
        }
        // A hint is an estimate: scroll only when it leaves fewer cells to rewrite.
        let current = prepared.buffer();
        let mut planned: Vec<AppliedScroll> = Vec::new();
        for &(region, rows) in prepared.scroll_hints() {
            if !movable(region, rows) {
                continue;
            }
            let free = |candidate: Rect| {
                planned
                    .iter()
                    .all(|scroll| scroll.region.intersection(candidate).is_empty())
            };
            let full = Rect::new(area.x, region.y, area.width, region.height);
            // A move already happened in the frame: scrolling the same cells costs nothing to
            // weigh, unless only full rows can show it and they would move other columns too.
            if prepared.moves().contains(&(region, rows)) {
                if region == full && clear(full) && free(full) {
                    planned.push(AppliedScroll { region, rows });
                    continue;
                }
                if self.scroll_acceleration == ScrollRegionAcceleration::AutoWithMargins
                    && clear(region)
                    && free(region)
                    && previous.band_has_clean_margins(region)
                {
                    planned.push(AppliedScroll { region, rows });
                    continue;
                }
            }
            let unscrolled = previous.band_changes(current, full, 0);
            let full_rows = (clear(full) && free(full)).then(|| {
                (
                    previous.band_changes(current, full, rows),
                    AppliedScroll { region: full, rows },
                )
            });
            let margins = (region != full
                && self.scroll_acceleration == ScrollRegionAcceleration::AutoWithMargins
                && free(region)
                && previous.band_has_clean_margins(region))
            .then(|| {
                (
                    unscrolled - previous.band_changes(current, region, 0)
                        + previous.band_changes(current, region, rows),
                    AppliedScroll { region, rows },
                )
            });
            // Full rows win a tie: they need no margin mode.
            let best = [full_rows, margins]
                .into_iter()
                .flatten()
                .min_by_key(|(cells, _)| *cells);
            if let Some((cells, scroll)) = best
                && cells + SCROLL_MIN_SAVED_CELLS <= unscrolled
            {
                planned.push(scroll);
            }
        }
        planned
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

/// Terminal output owner opened independently from stdin/stdout.
///
/// Uses nonblocking writes and at most 10ms poll waits, so cancellation never
/// waits for a terminal consumer to drain. Dropping does not flush. The descriptor is a separate open of stdout's terminal device, so its
/// nonblocking flag never affects stdout, stdin, or other processes sharing
/// the terminal.
#[cfg(unix)]
pub struct TtyOutput(std::fs::File);

/// The Linux name of [`TtyOutput`].
#[cfg(target_os = "linux")]
pub type LinuxTerminalOutput = TtyOutput;

#[cfg(unix)]
impl TtyOutput {
    /// Reopen the actual stdout tty with independent nonblocking flags.
    ///
    /// Does not use `/dev/tty`, which can differ from redirected stdout.
    /// # Errors
    /// Returns `Unsupported` for non-tty stdout or identity mismatch; otherwise
    /// returns the reopen/metadata error. No shared descriptor flags change.
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
        let path = Self::device_path(fd)?;
        let source = Self::identity(fd)?;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .open(&path)?;
        let reopened = file.metadata()?;
        // SAFETY: file owns a live descriptor for this entire call.
        if unsafe { libc::isatty(file.as_raw_fd()) } != 1
            || source != (reopened.dev(), reopened.ino(), reopened.rdev())
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "stdout tty identity changed",
            ));
        }
        Ok(Self(file))
    }

    /// A path that opens the terminal device behind `fd` anew.
    #[cfg(target_os = "linux")]
    fn device_path(fd: std::os::fd::RawFd) -> io::Result<std::path::PathBuf> {
        Ok(format!("/proc/self/fd/{fd}").into())
    }

    /// A path that opens the terminal device behind `fd` anew.
    #[cfg(not(target_os = "linux"))]
    fn device_path(fd: std::os::fd::RawFd) -> io::Result<std::path::PathBuf> {
        use std::os::unix::ffi::OsStrExt;
        let mut name = [0 as libc::c_char; 256];
        // SAFETY: the buffer and its length describe writable memory owned here.
        let result = unsafe { libc::ttyname_r(fd, name.as_mut_ptr(), name.len()) };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
        // SAFETY: a successful ttyname_r wrote a NUL-terminated path into the buffer.
        let name = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) };
        Ok(std::ffi::OsStr::from_bytes(name.to_bytes()).into())
    }

    /// Device, inode, and represented device of the file behind `fd`.
    fn identity(fd: std::os::fd::RawFd) -> io::Result<(u64, u64, u64)> {
        // SAFETY: zeroed stat is a valid out-parameter for fstat.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: fstat writes into the owned stat for a caller-provided descriptor.
        if unsafe { libc::fstat(fd, &raw mut stat) } != 0 {
            return Err(io::Error::last_os_error());
        }
        #[allow(
            clippy::unnecessary_cast,
            clippy::cast_sign_loss,
            reason = "stat field widths and signedness differ between platforms"
        )]
        Ok((stat.st_dev as u64, stat.st_ino as u64, stat.st_rdev as u64))
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
    fn painting_nothing_over_the_last_frame_writes_nothing() {
        let area = crate::geometry::Rect::new(0, 0, 8, 2);
        let mut terminal = Terminal::new(Vec::new(), area);
        terminal
            .draw(|cx| cx.set_cursor(crate::geometry::Point::new(1, 1), true))
            .unwrap();
        terminal.writer_mut().clear();
        terminal
            .draw_over(|cx| cx.set_cursor(crate::geometry::Point::new(1, 1), true))
            .unwrap();
        assert!(
            terminal.writer().is_empty(),
            "{:?}",
            String::from_utf8_lossy(terminal.writer())
        );
        terminal
            .draw_over(|cx| cx.set_cursor(crate::geometry::Point::new(2, 1), true))
            .unwrap();
        assert!(!terminal.writer().is_empty(), "a moved cursor is presented");
    }
    fn sized(text: &str) -> Vec<crate::paint::SizedCell<'_>> {
        text.char_indices()
            .map(|(start, ch)| crate::paint::SizedCell {
                symbol: &text[start..start + ch.len_utf8()],
                width: u8::try_from(unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1))
                    .unwrap_or(1),
                style: Style::new(),
            })
            .collect()
    }

    /// A rejected frame writes and commits nothing, and the next frame repaints in full; an
    /// unchanged frame with no other output writes nothing; other output is always written.
    #[test]
    fn fallible_frames_write_nothing_when_rejected_or_unchanged() {
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 20, 2));
        let shown = |terminal: &mut Terminal<Vec<u8>>, text: &'static str| {
            terminal.try_draw_over_with_output_hooks(
                |cx| {
                    cx.replace_cells(0, 0, sized(text));
                    Ok::<_, &str>(false)
                },
                |_| Ok(false),
                |_, _, _| Ok(()),
            )
        };
        assert!(matches!(shown(&mut terminal, "first"), Ok(Some(_))));
        terminal.writer_mut().clear();
        assert!(matches!(shown(&mut terminal, "first"), Ok(None)));
        assert_eq!(terminal.writer().as_slice(), b"");

        let rejected = terminal.try_draw_over_with_output_hooks(
            |cx| {
                cx.replace_cells(0, 0, sized("half"));
                Err::<bool, _>("rejected")
            },
            |_| Ok(false),
            |_, _, _| Ok(()),
        );
        assert!(matches!(
            rejected,
            Err(super::TryDrawError::Rejected("rejected"))
        ));
        assert!(
            terminal.writer().is_empty(),
            "a rejected frame writes nothing"
        );
        assert!(terminal.retained_buffer().is_none(), "and commits nothing");
        let repaint = shown(&mut terminal, "first").unwrap().unwrap();
        assert!(repaint.full_repaint, "the next frame repaints in full");

        terminal.writer_mut().clear();
        let extra = terminal.try_draw_over_with_output_hooks(
            |_| Ok::<_, &str>(true),
            |writer| writer.write_all(b"<graphics>").map(|()| false),
            |_, _, _| Ok(()),
        );
        assert!(matches!(extra, Ok(Some(_))));
        assert!(String::from_utf8_lossy(terminal.writer()).contains("<graphics>"));
    }

    /// With encoding off, frames painted over the last one commit and track their change but
    /// write nothing.
    #[test]
    fn frames_without_encoding_track_changes_and_write_nothing() {
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 20, 2));
        terminal
            .draw(|cx| cx.replace_cells(0, 0, sized("hello")))
            .unwrap();
        terminal.set_encoding(false);
        terminal.writer_mut().clear();
        let stats = terminal
            .draw_over(|cx| cx.replace_cells(0, 1, sized("there")))
            .unwrap();
        assert_eq!(terminal.writer().as_slice(), b"");
        assert_eq!(stats.changed_cells, 5);
        let changes = terminal.last_changes().expect("the change is tracked");
        assert_eq!(changes.runs().collect::<Vec<_>>(), [(1, 0..5)]);
        assert_eq!(
            terminal
                .retained_buffer()
                .unwrap()
                .row_symbols(1)
                .unwrap()
                .trim_end(),
            "there"
        );
    }

    /// A frame painted in place that declares its images replaces the image scene with them and
    /// hands the change to the output hook; one that does not keeps the scene; an unchanged
    /// image set writes nothing; images-only occlusion hides images without touching cells.
    #[test]
    fn frames_painted_over_carry_declared_images() {
        let placement = |key: &str, x: u16| ImagePlacement {
            key: ImageKey::new(key),
            payload: ImagePayload::Png {
                bytes: std::sync::Arc::from(&b"png"[..]),
                width: 1,
                height: 1,
            },
            destination: Rect::new(x, 0, 2, 1),
            fit: crate::image::ImageFit::default(),
            allocation: None,
            clip: Rect::new(x, 0, 2, 1),
            lifecycle: ImageLifecycle::Frame,
            z: 0,
        };
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 10, 2));
        terminal
            .draw(|cx| cx.replace_cells(0, 1, sized("x")))
            .unwrap();
        let mut deltas = Vec::new();
        let mut frame = |terminal: &mut Terminal<Vec<u8>>,
                         images: Option<Vec<ImagePlacement>>,
                         occlude: Option<Rect>| {
            let mut seen = None;
            let drawn = terminal
                .try_draw_over_with_output_hooks(
                    |cx| {
                        if let Some(images) = images {
                            cx.declare_images();
                            for image in images {
                                cx.push_image(ImageContribution::Present(image));
                            }
                        }
                        if let Some(area) = occlude {
                            cx.occlude_images(LocalRect::terminal(area));
                        }
                        Ok::<_, ()>(false)
                    },
                    |_| Ok(false),
                    |_, _, delta| {
                        seen = Some(delta.clone());
                        Ok(())
                    },
                )
                .unwrap();
            deltas.push(seen);
            drawn
        };
        assert!(frame(&mut terminal, Some(vec![placement("a", 0)]), None).is_some());
        assert_eq!(terminal.image_scene().placements().len(), 1);
        assert!(
            frame(&mut terminal, Some(vec![placement("a", 0)]), None).is_none(),
            "an unchanged image set writes nothing"
        );
        assert!(frame(&mut terminal, None, None).is_none());
        assert_eq!(
            terminal.image_scene().placements().len(),
            1,
            "undeclared keeps"
        );
        assert!(frame(&mut terminal, Some(Vec::new()), None).is_some());
        assert_eq!(
            terminal.image_scene().placements().len(),
            0,
            "declared empty removes"
        );
        assert!(
            frame(
                &mut terminal,
                Some(vec![placement("b", 4)]),
                Some(Rect::new(0, 0, 10, 1))
            )
            .is_none(),
            "a wholly occluded image shows nothing, so nothing changes"
        );
        assert_eq!(terminal.image_scene().placements().len(), 0);
        assert_eq!(
            terminal
                .retained_buffer()
                .unwrap()
                .row_symbols(1)
                .unwrap()
                .trim_end(),
            "x",
            "images-only occlusion leaves cells alone"
        );
        let first = deltas[0].as_ref().expect("the first frame wrote");
        assert_eq!(first.upserted.len(), 1);
        let removal = deltas[3].as_ref().expect("the removal wrote");
        assert_eq!(removal.removed, vec![ImageKey::new("a")]);
    }

    /// A frame painted in place writes the cells that change, not every cell it is given.
    #[test]
    fn frames_painted_over_write_only_the_cells_that_change() {
        let mut terminal = Terminal::new(Vec::new(), Rect::new(0, 0, 40, 2));
        terminal
            .draw(|cx| cx.replace_cells(0, 0, sized("hello brave new world")))
            .unwrap();
        terminal.writer_mut().clear();
        let stats = terminal
            .draw_over(|cx| cx.replace_cells(0, 0, sized("hello brave new world")))
            .unwrap();
        assert_eq!(stats.changed_cells, 0);
        assert!(
            terminal.writer().is_empty(),
            "nothing changed, nothing written"
        );

        let stats = terminal
            .draw_over(|cx| cx.replace_cells(0, 0, sized("hello brave NEW world")))
            .unwrap();
        let output = String::from_utf8_lossy(terminal.writer()).into_owned();
        assert!(output.contains("NEW"), "{output:?}");
        assert!(
            !output.contains("hello") && !output.contains("world"),
            "{output:?}"
        );
        assert!(stats.changed_cells <= 3, "{stats:?}");
    }

    /// Copied cells keep every grapheme whole: a run starting on the right half of a wide one in
    /// the source, or ending inside one in the destination, takes all of it.
    #[test]
    fn copied_cells_keep_wide_graphemes_whole() {
        let area = Rect::new(0, 0, 6, 1);
        let mut source = crate::buffer::Buffer::empty(area);
        let mut destination = crate::buffer::Buffer::empty(area);
        {
            let mut frame = crate::frame::Frame::new(&mut source);
            PaintCx::new(&mut frame).replace_cells(0, 0, sized("a\u{754c}bcd"));
        }
        {
            let mut frame = crate::frame::Frame::new(&mut destination);
            PaintCx::new(&mut frame).replace_cells(0, 0, sized("vwx\u{754c}z"));
        }
        {
            let mut frame = crate::frame::Frame::new(&mut destination);
            let mut cx = PaintCx::new(&mut frame);
            // Column 2 is the right half of the source's wide grapheme; column 4 is the right
            // half of the destination's.
            cx.copy_cells(&source, 0, 2..3);
            cx.copy_cells(&source, 0, 4..5);
        }
        // Columns 1 through 4 are copied; the edges keep their own cells.
        let mut expected = crate::buffer::Buffer::empty(area);
        {
            let mut frame = crate::frame::Frame::new(&mut expected);
            PaintCx::new(&mut frame).replace_cells(0, 0, sized("v\u{754c}bcz"));
        }
        assert_eq!(destination, expected);
    }

    /// Applying a frame's moves to the frame before it, then copying its changed runs from the
    /// cells it shows, gives the frame: what an observer's copy needs.
    #[test]
    fn last_changes_rebuild_a_frame_painted_over_the_last() {
        let area = Rect::new(0, 0, 12, 5);
        let mut seed = 0x51f1_5eed_u64;
        let mut next = move |bound: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % bound
        };
        let texts = ["ab", "\u{754c}x", "zz\u{754c}", " ", "q\u{754c}\u{754c}"];
        let mut terminal = Terminal::new(Vec::new(), area);
        terminal.set_scroll_region_acceleration(super::ScrollRegionAcceleration::Auto);
        terminal
            .draw(|cx| cx.fill(LocalRect::terminal(area), "x", Style::new()))
            .unwrap();
        assert!(
            terminal.last_changes().is_none(),
            "a full frame has no in-place change"
        );
        for frame in 0..500 {
            let mut copy = terminal.retained_buffer().unwrap().clone();
            let ops: Vec<(u64, i32, i64, &str)> = (0..next(5))
                .map(|_| {
                    (
                        next(4),
                        i32::try_from(next(12)).unwrap(),
                        i64::try_from(next(5)).unwrap(),
                        texts[usize::try_from(next(5)).unwrap()],
                    )
                })
                .collect();
            terminal
                .draw_over(|cx| {
                    for &(kind, x, y, text) in &ops {
                        match kind {
                            0 => cx.move_cells(
                                LocalRect::new(0, y.min(3), 12, 2),
                                if x % 2 == 0 { 1 } else { -1 },
                            ),
                            1 => cx.fill(LocalRect::new(x, y, 2, 1), "f", Style::new()),
                            _ => cx.replace_cells(x, y, sized(text)),
                        }
                    }
                })
                .unwrap();
            let shown = terminal.retained_buffer().unwrap();
            let changes = terminal
                .last_changes()
                .expect("a frame painted over the last has its change");
            {
                let mut frame = crate::frame::Frame::new(&mut copy);
                let mut cx = PaintCx::new(&mut frame);
                for (area, rows) in changes.moves() {
                    cx.move_cells(LocalRect::terminal(area), rows);
                }
                for (y, run) in changes.runs() {
                    cx.copy_cells(shown, y, run);
                }
            }
            assert_eq!(&copy, shown, "frame {frame}: {ops:?}");
        }
    }

    #[test]
    fn frames_without_synchronized_output_do_not_ask_for_it() {
        let area = crate::geometry::Rect::new(0, 0, 8, 2);
        for synchronized in [true, false] {
            let mut terminal = Terminal::new(Vec::new(), area);
            terminal.set_synchronized_output(synchronized);
            terminal
                .draw(|cx| {
                    cx.write_line(LocalRect::new(0, 0, 4, 1), &crate::text::Line::raw("full"));
                })
                .unwrap();
            terminal
                .draw_over(|cx| {
                    cx.write_line(LocalRect::new(0, 1, 4, 1), &crate::text::Line::raw("over"));
                })
                .unwrap();
            let output = String::from_utf8_lossy(terminal.writer()).into_owned();
            for mode in ["\u{1b}[?2026h", "\u{1b}[?2026l"] {
                assert_eq!(
                    output.matches(mode).count(),
                    if synchronized { 2 } else { 0 }
                );
            }
            assert!(output.contains("over"), "{output:?}");
        }
    }

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
        assert!(
            terminal.writer().is_empty(),
            "rejected frame must not emit any output bytes"
        );
    }

    #[test]
    fn staged_frames_move_retained_cells_and_hand_them_back_on_acknowledge() {
        let area = Rect::new(0, 0, 4, 1);
        let mut terminal = Terminal::new(Vec::new(), area);
        let text = |text: &'static str| {
            move |cx: &mut PaintCx<'_, '_>| {
                cx.write_line(LocalRect::new(0, 0, 4, 1), &crate::text::Line::raw(text));
            }
        };
        terminal.draw(text("abcd")).unwrap();
        let mut staged = terminal.stage(Vec::new());
        assert!(terminal.state.retained_buffer().is_none());
        let stats = staged.draw(text("abce")).unwrap();
        assert!(
            !stats.full_repaint,
            "the staged frame diffs against the moved cells"
        );
        assert_eq!(stats.changed_cells, 1);
        terminal.acknowledge(staged);
        let stats = terminal.draw(text("abcf")).unwrap();
        assert!(!stats.full_repaint);
        assert_eq!(stats.changed_cells, 1);

        let staged = terminal.stage(Vec::new());
        terminal.invalidate_after_failed_delivery(&staged);
        assert!(terminal.draw(text("abcf")).unwrap().full_repaint);
    }

    #[cfg(unix)]
    #[test]
    fn stdout_reopen_rejects_redirects_and_preserves_tty_flags() {
        use super::TtyOutput as LinuxTerminalOutput;
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
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
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

    #[cfg(unix)]
    #[test]
    fn nonblocking_tty_backpressure_is_cancellable_without_drain() {
        use super::TtyOutput as LinuxTerminalOutput;
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
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
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
                        bytes: vec![1, 2, 3, 4].into(),
                        width: 1,
                        height: 1,
                        format: crate::image::ImagePixelFormat::Rgba8,
                    },
                    destination: frame.clip(),
                    clip: frame.clip(),
                    lifecycle: ImageLifecycle::Frame,
                    z: 0,
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
                        bytes: vec![5, 6, 7, 8].into(),
                        width: 1,
                        height: 1,
                        format: crate::image::ImagePixelFormat::Rgba8,
                    },
                    destination: frame.clip(),
                    clip: frame.clip(),
                    lifecycle: ImageLifecycle::Frame,
                    z: 0,
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
                bytes: vec![1, 2, 3].into(),
                width: 1,
                height: 1,
            },
            destination: Rect::new(1, 0, 2, 1),
            clip: Rect::new(0, 0, 4, 2),
            lifecycle: ImageLifecycle::Frame,
            z: 0,
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
