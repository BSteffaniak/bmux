//! Frame render context.

use crate::buffer::{Buffer, Cell};
use crate::damage::{Damage, DamagePolicy};
use crate::focus::FocusScopeId;
use crate::geometry::{Point, Rect};
use crate::hit::{HitMap, HitRegion};
use crate::image::ImageContribution;
use crate::selection::{
    SelectionFragment, SelectionScene, SelectionScope, SelectionTextRun, paint_selection_highlights,
};
use crate::semantic::{SemanticRegion, SemanticScene};
use crate::style::Style;
use crate::text::Line;

/// Regional producer declaration; absence means ordinary complete metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceMetadataAvailability {
    /// Visible terminal geometry, projected by `PaintCx`.
    pub area: Rect,
    /// Whether logical selection metadata is supplied for this region.
    pub selection: bool,
    /// Whether semantic metadata is supplied for this region.
    pub semantics: bool,
}

/// Cursor requested by a rendered frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Cursor {
    /// Cursor position.
    pub position: Point,
    /// Whether the cursor should be visible.
    pub visible: bool,
}

impl Cursor {
    /// Create a visible cursor at `position`.
    #[must_use]
    pub const fn visible(position: Point) -> Self {
        Self {
            position,
            visible: true,
        }
    }

    /// Create a hidden cursor at `position`.
    #[must_use]
    pub const fn hidden(position: Point) -> Self {
        Self {
            position,
            visible: false,
        }
    }
}

/// Staging container for one frame's cells and interaction metadata.
///
/// A frame owns the buffer being painted plus the cursor, hit, focus-scope,
/// image, selection, semantic, and damage contributions registered while
/// painting. It never accepts writes directly: every contribution flows
/// through the scoped [`crate::paint::PaintCx`] created over it, which
/// applies translation and clipping uniformly. Presenters and offscreen
/// staging code create a frame, paint through a root `PaintCx`, and then read
/// the committed metadata through this type's accessors.
pub struct Frame<'buffer> {
    buffer: &'buffer mut Buffer,
    cursor: Option<Cursor>,
    hits: HitMap,
    focus_scope: Option<FocusScopeId>,
    images: Vec<ImageContribution>,
    occlusions: Vec<(usize, Rect)>,
    selection: SelectionScene,
    semantics: SemanticScene,
    damage: Vec<Rect>,
    scroll_hints: Vec<(Rect, i64)>,
    moves: Vec<(Rect, i64)>,
    metadata_availability: Vec<SurfaceMetadataAvailability>,
    clock: Option<crate::clock::PresentationClock>,
    written: Option<WrittenSpans>,
    painted: Option<WrittenSpans>,
    /// A move covered cells painted before it, which then left the painted spans.
    moved_painted: bool,
    over: Option<OverChanges>,
}

/// What a frame painted in place over the last one changed: the cells painted, carried along by
/// later moves, and the moves themselves.
#[derive(Debug)]
pub(crate) struct OverChanges {
    area: Rect,
    dirty: Vec<bool>,
    /// Per row, whether any of its cells may be marked (false only when none is).
    rows: Vec<bool>,
    /// The moves, in order.
    pub(crate) moves: Vec<OverMove>,
}

/// One cell move of a frame painted over the last one.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OverMove {
    pub(crate) area: Rect,
    pub(crate) rows: i64,
    /// No wide grapheme crossed the area's left or right edge, and nothing had been painted
    /// beside them: a terminal can move the area inside left/right margins.
    pub(crate) clean_edges: bool,
}

impl OverChanges {
    /// No changes over `area`.
    pub(crate) fn empty(area: Rect) -> Self {
        Self::new(area)
    }

    fn new(area: Rect) -> Self {
        Self {
            area,
            dirty: vec![false; usize::from(area.width) * usize::from(area.height)],
            rows: vec![false; usize::from(area.height)],
            moves: Vec::new(),
        }
    }

    /// No changes over `area`, reusing this storage.
    pub(crate) fn reuse(mut self, area: Rect) -> Self {
        if self.area == area {
            for (row, marked) in self.rows.iter_mut().enumerate() {
                if std::mem::take(marked) {
                    let start = row * usize::from(area.width);
                    self.dirty[start..start + usize::from(area.width)].fill(false);
                }
            }
            self.moves.clear();
            self
        } else {
            Self::new(area)
        }
    }

    fn index(&self, x: u16, y: u16) -> usize {
        usize::from(y - self.area.y) * usize::from(self.area.width) + usize::from(x - self.area.x)
    }

    /// Mark the cells of `area` changed.
    pub(crate) fn mark(&mut self, area: Rect) {
        let area = area.intersection(self.area);
        for y in area.y..area.bottom() {
            let start = self.index(area.x, y);
            self.dirty[start..start + usize::from(area.width)].fill(true);
            self.rows[usize::from(y - self.area.y)] = true;
        }
    }

    /// Whether any cell of `area` is marked.
    fn any(&self, area: Rect) -> bool {
        let area = area.intersection(self.area);
        (area.y..area.bottom()).any(|y| {
            let start = self.index(area.x, y);
            self.rows[usize::from(y - self.area.y)]
                && self.dirty[start..start + usize::from(area.width)].contains(&true)
        })
    }

    /// Move the marks of `band` by `rows` with the cells; the rows it exposes are unmarked, as a
    /// terminal scroll blanks them too.
    fn shift(&mut self, band: Rect, rows: i64) {
        let band = band.intersection(self.area);
        let height = usize::from(band.height);
        let moved = usize::try_from(rows.unsigned_abs())
            .unwrap_or(usize::MAX)
            .min(height);
        let columns = usize::from(band.width);
        if band.is_empty() {
            return;
        }
        let (first, stride) = (self.index(band.x, band.y), usize::from(self.area.width));
        let row = |offset: usize| first + offset * stride;
        let top = usize::from(band.y - self.area.y);
        if rows > 0 {
            for offset in 0..height - moved {
                let (target, source) = (row(offset), row(offset + moved));
                self.dirty.copy_within(source..source + columns, target);
                // The row keeps its marks outside the band and gains the source's inside it.
                self.rows[top + offset] |= self.rows[top + offset + moved];
            }
            for offset in height - moved..height {
                self.dirty[row(offset)..row(offset) + columns].fill(false);
            }
        } else if rows < 0 {
            for offset in (moved..height).rev() {
                let (target, source) = (row(offset), row(offset - moved));
                self.dirty.copy_within(source..source + columns, target);
                self.rows[top + offset] |= self.rows[top + offset - moved];
            }
            for offset in 0..moved {
                self.dirty[row(offset)..row(offset) + columns].fill(false);
            }
        }
    }

    /// The marked runs of row `y`, left to right.
    pub(crate) fn runs(&self, y: u16) -> impl Iterator<Item = std::ops::Range<u16>> + '_ {
        let start = self.index(self.area.x, y);
        let row = if self.rows[usize::from(y - self.area.y)] {
            &self.dirty[start..start + usize::from(self.area.width)]
        } else {
            &[][..]
        };
        let x0 = self.area.x;
        let mut column = 0;
        std::iter::from_fn(move || {
            let first = column + row[column..].iter().position(|dirty| *dirty)?;
            let end = first
                + row[first..]
                    .iter()
                    .position(|dirty| !*dirty)
                    .unwrap_or(row.len() - first);
            column = end;
            let to = |offset: usize| x0 + u16::try_from(offset).unwrap_or(u16::MAX);
            Some(to(first)..to(end))
        })
    }

    /// Whether nothing is marked.
    pub(crate) fn is_clean(&self) -> bool {
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, marked)| **marked)
            .all(|(row, _)| {
                let start = row * usize::from(self.area.width);
                !self.dirty[start..start + usize::from(self.area.width)].contains(&true)
            })
    }
}

/// The columns each row of a frame's buffer may have been written in, one `start..end` span
/// per row (empty when `start >= end`).
///
/// A span covers every cell a write may change, including the halves of wide graphemes a
/// write next to them repairs, so cells outside every span are exactly as the frame began.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WrittenSpans {
    area: Rect,
    rows: Vec<(u16, u16)>,
}

impl WrittenSpans {
    fn new(area: Rect) -> Self {
        Self {
            area,
            rows: vec![(u16::MAX, 0); usize::from(area.height)],
        }
    }

    /// Note a write to `area`, widened by the wide-grapheme repair a write may do beside it: the
    /// leader left of a continuation it overwrites, and a continuation (and the one after a wide
    /// grapheme it places) to its right.
    fn note(&mut self, area: Rect) {
        let area = area.intersection(self.area);
        if area.is_empty() {
            return;
        }
        let start = area.x.saturating_sub(1).max(self.area.x);
        let end = area.right().saturating_add(2).min(self.area.right());
        for y in area.y..area.bottom() {
            let row = &mut self.rows[usize::from(y - self.area.y)];
            row.0 = row.0.min(start);
            row.1 = row.1.max(end);
        }
    }

    /// Whether any written span meets `area`.
    fn intersects(&self, area: Rect) -> bool {
        let area = area.intersection(self.area);
        (area.y..area.bottom()).any(|y| {
            let (start, end) = self.rows[usize::from(y - self.area.y)];
            start < end && start < area.right() && area.x < end
        })
    }

    /// The written `(y, start..end)` spans, top to bottom.
    pub(crate) fn spans(&self) -> impl Iterator<Item = (u16, std::ops::Range<u16>)> + '_ {
        self.rows
            .iter()
            .zip(self.area.y..)
            .filter(|((start, end), _)| start < end)
            .map(|((start, end), y)| (y, *start..*end))
    }
}

/// Apply opaque coverage to metadata preceding an overlay, whether staged or retained.
pub(crate) fn occlude_metadata(
    area: Rect,
    cursor: &mut Option<Cursor>,
    hits: &mut HitMap,
    selection: &mut SelectionScene,
    semantics: &mut SemanticScene,
    availability: &mut Vec<SurfaceMetadataAvailability>,
) {
    *availability = availability
        .iter()
        .flat_map(|declaration| {
            crate::image_scene::subtract_rect(declaration.area, area)
                .into_iter()
                .map(|visible| SurfaceMetadataAvailability {
                    area: visible,
                    ..*declaration
                })
        })
        .collect();
    *cursor = cursor.filter(|cursor| !area.contains(cursor.position));
    let mut visible_hits = HitMap::new();
    for hit in hits.regions() {
        for visible in crate::image_scene::subtract_rect(hit.area, area) {
            let mut fragment = hit.clone();
            fragment.area = visible;
            visible_hits.push(fragment);
        }
    }
    *hits = visible_hits;
    for scope in selection.scopes_mut() {
        if !scope.initiation_area.intersection(area).is_empty() {
            // One rectangular initiation area cannot represent a cutout safely.
            scope.initiation_area = Rect::new(0, 0, 0, 0);
        }
    }
    selection.remove_content_in(area);
    let mut visible_semantics = SemanticScene::new();
    for region in semantics.regions() {
        for visible in crate::image_scene::subtract_rect(region.area, area) {
            let mut fragment = region.clone();
            fragment.area = visible;
            visible_semantics.push(fragment);
        }
    }
    *semantics = visible_semantics;
}

impl<'buffer> Frame<'buffer> {
    /// Create a frame that stages painting into `buffer`.
    pub const fn new(buffer: &'buffer mut Buffer) -> Self {
        Self {
            buffer,
            cursor: None,
            hits: HitMap::new(),
            focus_scope: None,
            images: Vec::new(),
            occlusions: Vec::new(),
            selection: SelectionScene::new(),
            semantics: SemanticScene::new(),
            damage: Vec::new(),
            scroll_hints: Vec::new(),
            moves: Vec::new(),
            metadata_availability: Vec::new(),
            clock: None,
            written: None,
            painted: None,
            moved_painted: false,
            over: None,
        }
    }

    /// Record which cells painting writes, for [`Self::take_written`].
    pub(crate) fn track_writes(&mut self) {
        self.written = Some(WrittenSpans::new(self.buffer.area()));
        self.painted = Some(WrittenSpans::new(self.buffer.area()));
    }

    /// Record which cells change, for a frame painted in place over the last one.
    pub(crate) fn track_over(&mut self, storage: Option<OverChanges>) {
        let area = self.buffer.area();
        self.over =
            Some(storage.map_or_else(|| OverChanges::new(area), |storage| storage.reuse(area)));
        self.buffer.log_spill();
    }

    /// The changes since [`Self::track_over`].
    pub(crate) fn take_over(&mut self) -> Option<OverChanges> {
        self.mark_spill();
        self.buffer.stop_spill();
        self.over.take()
    }

    /// Mark the cells writes changed beside the ones they asked for.
    fn mark_spill(&mut self) {
        let spill = self.buffer.take_spill();
        if let Some(over) = self.over.as_mut() {
            for point in spill {
                over.mark(Rect::new(point.x, point.y, 1, 1));
            }
        }
    }

    /// The cells painted since [`Self::track_writes`]: those written, less the ones only moved.
    /// `None` when a move carried painted cells away from where they were painted.
    pub(crate) fn take_painted(&mut self) -> Option<WrittenSpans> {
        self.painted.take().filter(|_| !self.moved_painted)
    }

    /// The cells written since [`Self::track_writes`], or `None` when writes were not tracked.
    pub(crate) const fn take_written(&mut self) -> Option<WrittenSpans> {
        self.written.take()
    }

    fn note_written(&mut self, area: Rect) {
        if let Some(over) = self.over.as_mut() {
            over.mark(area);
        }
        self.note_moved(area);
        if let Some(painted) = self.painted.as_mut() {
            painted.note(area);
        }
    }

    fn note_moved(&mut self, area: Rect) {
        if let Some(written) = self.written.as_mut() {
            written.note(area);
        }
    }

    /// Set the presentation clock shared by every context painting this frame.
    pub const fn set_clock(&mut self, clock: Option<crate::clock::PresentationClock>) {
        self.clock = clock;
    }

    /// The presentation clock of this frame, when its driver supplied one.
    #[must_use]
    pub const fn clock(&self) -> Option<crate::clock::PresentationClock> {
        self.clock
    }

    /// Return the frame area.
    #[must_use]
    pub const fn area(&self) -> Rect {
        self.buffer.area()
    }

    /// Return the current cursor request.
    #[must_use]
    pub const fn cursor(&self) -> Option<Cursor> {
        self.cursor
    }

    /// Return an immutable view of the backing buffer.
    #[must_use]
    pub const fn buffer(&self) -> &Buffer {
        self.buffer
    }

    /// Return registered hit regions.
    #[must_use]
    pub const fn hits(&self) -> &HitMap {
        &self.hits
    }

    /// Return the active focus scope requested by this frame.
    #[must_use]
    pub const fn focus_scope(&self) -> Option<&FocusScopeId> {
        self.focus_scope.as_ref()
    }

    /// Return image lifecycle contributions registered for this frame.
    #[must_use]
    pub fn images(&self) -> &[ImageContribution] {
        &self.images
    }

    /// Regional producer metadata declarations in paint order.
    #[must_use]
    pub fn metadata_availability(&self) -> &[SurfaceMetadataAvailability] {
        &self.metadata_availability
    }

    pub(crate) fn declare_metadata_availability(
        &mut self,
        declaration: SurfaceMetadataAvailability,
    ) {
        if !declaration.area.is_empty() {
            self.metadata_availability.push(declaration);
        }
    }

    /// Staged opaque rectangles with their image paint-order boundary.
    #[must_use]
    pub fn occlusions(&self) -> &[(usize, Rect)] {
        &self.occlusions
    }

    /// Return selection metadata registered for this frame.
    #[must_use]
    pub const fn selection(&self) -> &SelectionScene {
        &self.selection
    }

    /// Return semantic regions registered for this frame.
    #[must_use]
    pub const fn semantics(&self) -> &SemanticScene {
        &self.semantics
    }

    /// Return bounded damage requested by rendered components.
    #[must_use]
    pub fn damage(&self, policy: DamagePolicy) -> Damage {
        Damage::regions(self.damage.iter().copied(), self.area(), policy)
    }

    /// The largest scroll a component reported for this frame (see
    /// [`crate::paint::PaintCx::hint_scroll`]): terminal rows whose content
    /// moved by the signed row count relative to the previous frame.
    #[must_use]
    pub fn scroll_hint(&self) -> Option<(Rect, i64)> {
        self.scroll_hints.first().copied()
    }

    /// Every scroll reported while painting, largest first; no two overlap.
    #[must_use]
    pub fn scroll_hints(&self) -> &[(Rect, i64)] {
        &self.scroll_hints
    }

    /// The cell moves painted ([`crate::paint::PaintCx::move_cells`]), in order.
    #[must_use]
    pub fn moves(&self) -> &[(Rect, i64)] {
        &self.moves
    }

    // Every mutator below is reachable only through `crate::paint::PaintCx`,
    // which translates and clips each contribution before staging it here.

    /// Subtract an already projected opaque rectangle from earlier metadata.
    /// Image occlusion is resolved by the retained image scene, not by deleting
    /// source contributions. Cells are cleared with complete wide-cell repair.
    pub(crate) fn occlude(&mut self, area: Rect) {
        let area = area.intersection(self.area());
        if area.is_empty() {
            return;
        }
        self.occlusions.push((self.images.len(), area));
        self.note_written(area);
        self.buffer.fill(area, " ", Style::new());
        occlude_metadata(
            area,
            &mut self.cursor,
            &mut self.hits,
            &mut self.selection,
            &mut self.semantics,
            &mut self.metadata_availability,
        );
    }

    /// The buffer, to write the cells of `area` (a projected, clipped rectangle).
    pub(crate) fn buffer_mut(&mut self, area: Rect) -> &mut Buffer {
        self.note_written(area);
        self.buffer
    }

    /// The buffer, for a write whose extent is known only once it is done; the caller reports it
    /// with [`Self::wrote`].
    pub(crate) const fn buffer_for_write(&mut self) -> &mut Buffer {
        self.buffer
    }

    /// Report the cells of `area` written through [`Self::buffer_for_write`].
    pub(crate) fn wrote(&mut self, area: Rect) {
        self.note_written(area);
    }

    pub(crate) const fn set_cursor(&mut self, cursor: Cursor) {
        self.cursor = Some(cursor);
    }

    pub(crate) fn push_hit(&mut self, mut region: HitRegion) {
        if let Some(scope) = self.focus_scope.as_ref()
            && region.focusable
            && region.focus_scope.is_none()
        {
            region.focus_scope = Some(scope.clone());
        }
        self.hits.push(region);
    }

    pub(crate) fn set_focus_scope(&mut self, scope: Option<FocusScopeId>) {
        if scope.is_some() && scope != self.focus_scope {
            // Selection also obeys scope exclusion. Previously painted scopes
            // remain logical metadata but cannot initiate or autoscroll behind
            // the foreground. Newly painted foreground scopes remain eligible.
            for prior in self.selection.scopes_mut() {
                prior.capture = crate::selection::SelectionCapture::Disabled;
                prior.auto_scroll = crate::selection::SelectionAutoScrollPolicy::disabled();
            }
        }
        self.focus_scope = scope;
    }

    pub(crate) fn push_image(&mut self, contribution: ImageContribution) {
        self.images.push(contribution);
    }

    pub(crate) fn push_semantic(&mut self, region: SemanticRegion) {
        self.semantics.push(region);
    }

    /// Keep scroll hints that do not overlap, largest first: a hint overlapping others replaces
    /// them only when it covers more cells than each of them.
    pub(crate) fn hint_scroll(&mut self, area: Rect, rows: i64) {
        let cells = |area: Rect| u32::from(area.width) * u32::from(area.height);
        if rows == 0 || rows.unsigned_abs() >= u64::from(area.height) {
            return;
        }
        let overlaps = |(other, _): &(Rect, i64)| !other.intersection(area).is_empty();
        if self
            .scroll_hints
            .iter()
            .filter(|hint| overlaps(hint))
            .any(|(other, _)| cells(*other) >= cells(area))
        {
            return;
        }
        self.scroll_hints.retain(|hint| !overlaps(hint));
        let at = self
            .scroll_hints
            .partition_point(|(other, _)| cells(*other) >= cells(area));
        self.scroll_hints.insert(at, (area, rows));
    }

    /// Move the cells of an already projected `area` by `rows` (positive up), blanking the rows
    /// it exposes, and report the move as a scroll hint.
    pub(crate) fn move_cells(&mut self, area: Rect, rows: i64) {
        let area = area.intersection(self.area());
        if area.is_empty() || rows == 0 {
            return;
        }
        // Spill logged so far is at positions this move may carry away: mark it first.
        self.mark_spill();
        if let Some(over) = self.over.as_mut() {
            // Margins can hold the move only if no grapheme straddles them on the terminal,
            // which shows what the buffer does wherever nothing was painted yet.
            let edges = [
                Rect::new(area.x.saturating_sub(1), area.y, 2, area.height),
                Rect::new(area.right().saturating_sub(1), area.y, 2, area.height),
            ];
            let clean_edges = !edges.iter().any(|edge| over.any(*edge))
                && self.buffer.band_has_clean_margins(area);
            for y in area.y..area.bottom() {
                let left = Point::new(area.x, y);
                let right = Point::new(area.right().saturating_sub(1), y);
                if self
                    .buffer
                    .get(left)
                    .is_some_and(Cell::is_wide_continuation)
                {
                    over.mark(Rect::new(left.x.saturating_sub(1), y, 2, 1));
                }
                if self.buffer.get(right).is_some_and(Cell::is_wide_leader) {
                    over.mark(Rect::new(right.x, y, 2, 1));
                }
            }
            over.shift(area, rows);
            over.moves.push(OverMove {
                area,
                rows,
                clean_edges,
            });
        }
        self.note_moved(area);
        let tracking_over = self.over.is_some();
        if self
            .painted
            .as_ref()
            .is_some_and(|painted| painted.intersects(area))
        {
            self.moved_painted = true;
        }
        // Wide graphemes the edges cut are blanked: painted, not moved.
        if let Some(painted) = self.painted.as_mut() {
            for y in area.y..area.bottom() {
                let left = Point::new(area.x, y);
                let right = Point::new(area.right().saturating_sub(1), y);
                if self
                    .buffer
                    .get(left)
                    .is_some_and(Cell::is_wide_continuation)
                {
                    painted.note(Rect::new(left.x, y, 1, 1));
                }
                if self.buffer.get(right).is_some_and(Cell::is_wide_leader) {
                    painted.note(Rect::new(right.x, y, 1, 1));
                }
            }
        }
        self.buffer.move_band(area, rows);
        if tracking_over {
            // The cells the move blanked at its edges were marked where they were, and moved
            // with the marks.
            let _ = self.buffer.take_spill();
        }
        self.moves.push((area, rows));
        self.hint_scroll(area, rows);
    }

    pub(crate) fn push_damage(&mut self, area: Rect) {
        if !area.is_empty() {
            self.damage.push(area);
        }
    }

    pub(crate) fn push_selection_scope(&mut self, scope: SelectionScope) {
        self.selection.push_scope(scope);
    }

    pub(crate) fn push_selection_fragment(&mut self, fragment: SelectionFragment) {
        self.selection.push_fragment(fragment);
    }

    pub(crate) fn push_selection_text_run(&mut self, run: SelectionTextRun) {
        self.selection.push_text_run(run);
    }

    pub(crate) fn paint_selection(&mut self, highlights: &[Rect], style: Style) {
        for highlight in highlights {
            self.note_written(*highlight);
        }
        paint_selection_highlights(self.buffer, highlights, style);
    }

    pub(crate) fn fill(&mut self, area: Rect, symbol: &str, style: Style) {
        self.note_written(area);
        self.buffer.fill(area, symbol, style);
    }

    pub(crate) fn write_line_with_fallback_style(&mut self, area: Rect, line: &Line, style: Style) {
        self.note_written(area);
        self.buffer
            .write_line_with_fallback_style(area, line, style);
    }
}

#[cfg(test)]
mod tests {
    use super::{Cursor, Frame};
    use crate::buffer::Buffer;
    use crate::geometry::{Point, Rect};
    use crate::image::{ImageContribution, ImageKey};

    #[test]
    fn occlusion_and_scope_exclusion_share_staged_metadata() {
        use crate::hit::HitRegion;
        use crate::selection::{SelectionCapture, SelectionScope};
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 4));
        let mut frame = Frame::new(&mut buffer);
        frame.push_hit(HitRegion::new("background", Rect::new(0, 0, 8, 4)).focusable(true));
        frame.push_selection_scope(SelectionScope::new(
            "background-selection",
            Rect::new(0, 0, 8, 4),
        ));
        frame.set_cursor(Cursor::visible(Point::new(3, 1)));
        frame.occlude(Rect::new(3, 1, 2, 2));
        assert!(frame.cursor().is_none());
        assert!(frame.hits().hit_test(Point::new(3, 1)).is_none());
        assert!(frame.hits().hit_test(Point::new(0, 0)).is_some());
        frame.set_focus_scope(Some("modal".into()));
        frame.push_selection_scope(SelectionScope::new(
            "foreground-selection",
            Rect::new(3, 1, 2, 2),
        ));
        assert_eq!(
            frame.selection().scopes()[0].capture,
            SelectionCapture::Disabled
        );
        assert_eq!(
            frame.selection().scopes()[1].capture,
            SelectionCapture::Capture
        );
        assert_eq!(frame.occlusions(), &[(0, Rect::new(3, 1, 2, 2))]);
    }

    #[test]
    fn frame_tracks_cursor_request() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 2));
        let mut frame = Frame::new(&mut buffer);

        frame.set_cursor(Cursor::visible(Point::new(1, 1)));

        assert_eq!(frame.cursor(), Some(Cursor::visible(Point::new(1, 1))));
    }

    #[test]
    fn frame_collects_image_contributions_in_render_order() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 2));
        let mut frame = Frame::new(&mut buffer);

        frame.push_image(ImageContribution::Remove(ImageKey::new("first")));
        frame.push_image(ImageContribution::Remove(ImageKey::new("second")));

        assert_eq!(
            frame.images(),
            [
                ImageContribution::Remove(ImageKey::new("first")),
                ImageContribution::Remove(ImageKey::new("second")),
            ]
        );
    }
}
