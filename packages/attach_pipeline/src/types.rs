use bmux_attach_layout_protocol::{AttachInputModeState, AttachMouseProtocolState};
use bmux_plugin::{
    ExtensionRect, RenderDamage, RenderExtensionLayer, TerminalGraphicFill, TerminalRgba,
};
use bmux_terminal_grid::{GridLimits, PhysicalRow, TerminalGridStream, TerminalProtocolTracker};
use std::collections::BTreeMap;
use std::sync::Mutex;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaneRect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AttachCursorState {
    pub x: u16,
    pub y: u16,
    pub visible: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttachScrollbackCursor {
    pub row: usize,
    pub col: usize,
}

/// A position in a pane's scrollback, addressed by absolute history line.
///
/// `line` counts rows from the very first row ever scrolled into this pane's
/// history, in the numbering established by
/// [`bmux_terminal_grid::TerminalGrid::total_scrolled_rows`]. It is therefore
/// stable while the view scrolls and while new output arrives, which is what
/// makes it usable as a selection endpoint. Converting to and from a viewport
/// row always goes through [`ScrollbackViewportBase`]; never add a scrollback
/// offset to a viewport row directly, because those two axes count in opposite
/// directions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct AttachScrollbackPosition {
    pub line: u64,
    pub col: usize,
}

/// Mapping between absolute history lines and viewport rows for one rendered
/// pane window.
///
/// `top_line` is the absolute history line drawn on the window's first row.
/// Both the renderer's selection highlight and the copy path must derive their
/// row identity from the same base, or they will disagree about what is
/// selected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScrollbackViewportBase {
    top_line: u64,
}

impl ScrollbackViewportBase {
    /// Base for a window showing `scrollback_offset` rows back from the live
    /// bottom of a grid that has scrolled `total_scrolled_rows` rows into
    /// history.
    #[must_use]
    pub fn from_scrolled_rows(total_scrolled_rows: u64, scrollback_offset: usize) -> Self {
        Self {
            top_line: total_scrolled_rows
                .saturating_sub(u64::try_from(scrollback_offset).unwrap_or(u64::MAX)),
        }
    }

    /// Absolute history line drawn on the window's first row.
    #[must_use]
    pub const fn top_line(self) -> u64 {
        self.top_line
    }

    /// Absolute history line drawn on viewport row `row`.
    #[must_use]
    pub fn line_for_viewport_row(self, row: usize) -> u64 {
        self.top_line
            .saturating_add(u64::try_from(row).unwrap_or(u64::MAX))
    }

    /// Viewport row drawing absolute history line `line`, if it is within
    /// `height` rows of this window's top.
    #[must_use]
    pub fn viewport_row_for_line(self, line: u64, height: usize) -> Option<usize> {
        if line < self.top_line {
            return None;
        }
        let row = usize::try_from(line.saturating_sub(self.top_line)).ok()?;
        (row < height).then_some(row)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScrollbackCapture {
    pub identity: Uuid,
    pub lines: u64,
    pub truncated: bool,
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScrollbackPin {
    pub capture: Option<ScrollbackCapture>,
    pub pin_id: u64,
    pub total_scrolled_rows: u64,
    pub max_scrollback_offset: usize,
    pub stream_end: u64,
    pub created_epoch_secs: u64,
}

/// Per-pane scrollback view position.
///
/// Scrollback history itself is owned by the server (the pane-runtime
/// plugin's `TerminalGrid`); this type only records *where* one client is
/// looking into that history for one pane. The presence of an entry in
/// [`PaneScrollbackViews`] is what makes a pane "in scrollback" — there is
/// deliberately no separate active flag, so the state cannot follow focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaneScrollbackView {
    /// Rows scrolled back from the live viewport bottom.
    pub offset: usize,
    /// Viewport-relative selection/navigation cursor.
    pub cursor: AttachScrollbackCursor,
    /// Selection anchor as an absolute history line, when selecting.
    pub selection_anchor: Option<AttachScrollbackPosition>,
    /// Logical endpoint and its last physical projection. The physical value
    /// detects selection replacement by input handlers; identity survives eviction
    /// from the bounded display window.
    pub captured_selection: Option<(CapturedHistoryAnchor, AttachScrollbackPosition)>,
    /// Immutable server-side history pin when this pane uses frozen scrollback.
    pub pin: Option<ScrollbackPin>,
}

impl PaneScrollbackView {
    /// Absolute history position of this view's cursor, in `base`'s numbering.
    #[must_use]
    pub fn cursor_position(&self, base: ScrollbackViewportBase) -> AttachScrollbackPosition {
        AttachScrollbackPosition {
            line: base.line_for_viewport_row(self.cursor.row),
            col: self.cursor.col,
        }
    }

    /// Ordered selection bounds, when a selection anchor is set.
    #[must_use]
    pub fn selection_bounds(
        &self,
        base: ScrollbackViewportBase,
    ) -> Option<(AttachScrollbackPosition, AttachScrollbackPosition)> {
        let anchor = self.selection_anchor?;
        let head = self.cursor_position(base);
        Some(if anchor <= head {
            (anchor, head)
        } else {
            (head, anchor)
        })
    }

    /// Shift the selection anchor into a new line numbering.
    ///
    /// Used when the base a selection was created against is replaced by a
    /// different one (for example when the first server scrollback window
    /// arrives for a pane whose local grid started counting from zero).
    pub fn rebase_selection_anchor(
        &mut self,
        from: ScrollbackViewportBase,
        to: ScrollbackViewportBase,
    ) {
        if from == to {
            return;
        }
        if let Some(anchor) = self.selection_anchor.as_mut() {
            anchor.line = if anchor.line >= from.top_line() {
                to.top_line().saturating_add(anchor.line - from.top_line())
            } else {
                to.top_line().saturating_sub(from.top_line() - anchor.line)
            };
        }
    }
}

/// Scrollback view positions keyed by pane id.
///
/// A pane is in scrollback if and only if it has an entry here.
pub type PaneScrollbackViews = BTreeMap<Uuid, PaneScrollbackView>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionRenderCacheEntry {
    pub surface_id: Uuid,
    pub surface_rect: ExtensionRect,
    pub damage: RenderDamage,
    pub revision: u64,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionLayerSnapshotCacheEntry {
    pub surface_id: Uuid,
    pub surface_rect: ExtensionRect,
    pub layer: RenderExtensionLayer,
    pub emitted_damage: RenderDamage,
    pub full_snapshot_damage: RenderDamage,
    pub revision: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionRetainedItemCacheEntry {
    pub key: String,
    pub z: i16,
    pub bounds: ExtensionRect,
    pub content_fingerprint: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionRetainedLayerCacheEntry {
    pub surface_id: Uuid,
    pub surface_rect: ExtensionRect,
    pub layer: RenderExtensionLayer,
    pub revision: Option<u64>,
    pub items: Vec<ExtensionRetainedItemCacheEntry>,
}

pub struct PaneRenderBuffer {
    pub terminal_grid: TerminalGridStream,
    pub protocol_tracker: TerminalProtocolTracker,
    pub prev_rows: Vec<String>,
    pub sync_update_in_progress: bool,
    pub expected_stream_start: Option<u64>,
    pub scrollback_window: Option<PaneScrollbackWindow>,
    pub extension_render_cache: BTreeMap<(String, Uuid), ExtensionRenderCacheEntry>,
    pub extension_layer_snapshot_cache: BTreeMap<(String, Uuid), ExtensionLayerSnapshotCacheEntry>,
    pub extension_retained_layer_cache: BTreeMap<(String, Uuid), ExtensionRetainedLayerCacheEntry>,
    pub visual_row_fingerprints: PaneVisualRowFingerprintCache,
}

pub type TerminalGraphicsCache = BTreeMap<u64, TerminalGraphicCacheEntry>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalGraphicCacheEntry {
    pub pane_id: Uuid,
    pub surface_id: Uuid,
    pub source: TerminalGraphicSourceSignature,
    pub placement: Option<TerminalGraphicPlacementSignature>,
    pub host_image_id: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TerminalGraphicSourceSignature {
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub color: TerminalRgba,
    pub fill: TerminalGraphicFill,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TerminalGraphicPlacementSignature {
    pub cell_rect: ExtensionRect,
    pub z_index: i16,
}

#[derive(Default)]
pub struct PaneVisualRowFingerprintCache {
    inner: Mutex<PaneVisualRowFingerprintState>,
}

#[derive(Default)]
struct PaneVisualRowFingerprintState {
    width: u16,
    height: u16,
    content_revision: Option<u64>,
    rows: Vec<Option<u64>>,
}

impl PaneVisualRowFingerprintCache {
    pub fn clear(&self) {
        if let Ok(mut guard) = self.inner.lock() {
            *guard = PaneVisualRowFingerprintState::default();
        }
    }

    pub fn invalidate_rows(&self, reset_rows: bool, content_revision: u64, row_updates: &[u32]) {
        let Ok(mut guard) = self.inner.lock() else {
            return;
        };
        if reset_rows {
            *guard = PaneVisualRowFingerprintState {
                content_revision: Some(content_revision),
                ..PaneVisualRowFingerprintState::default()
            };
            return;
        }
        guard.content_revision = Some(content_revision);
        for row in row_updates {
            let index = usize::try_from(*row).unwrap_or(usize::MAX);
            if let Some(fingerprint) = guard.rows.get_mut(index) {
                *fingerprint = None;
            }
        }
    }

    pub fn get_or_compute(
        &self,
        width: u16,
        height: u16,
        content_revision: u64,
        row: u16,
        compute: impl FnOnce() -> Option<u64>,
    ) -> Option<u64> {
        if row >= height {
            return None;
        }
        if let Ok(mut guard) = self.inner.lock() {
            guard.ensure_dimensions(width, height, content_revision);
            if let Some(Some(fingerprint)) = guard.rows.get(usize::from(row)) {
                return Some(*fingerprint);
            }
        }

        let fingerprint = compute()?;
        if let Ok(mut guard) = self.inner.lock() {
            guard.ensure_dimensions(width, height, content_revision);
            if let Some(slot) = guard.rows.get_mut(usize::from(row)) {
                *slot = Some(fingerprint);
            }
        }
        Some(fingerprint)
    }
}

impl PaneVisualRowFingerprintState {
    fn ensure_dimensions(&mut self, width: u16, height: u16, content_revision: u64) {
        if self.width == width
            && self.height == height
            && self.content_revision == Some(content_revision)
            && self.rows.len() == usize::from(height)
        {
            return;
        }
        self.width = width;
        self.height = height;
        self.content_revision = Some(content_revision);
        self.rows = vec![None; usize::from(height)];
    }
}

/// A content position within one immutable history capture, independent of width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapturedHistoryAnchor {
    pub capture_id: Uuid,
    pub line_index: u32,
    pub column: usize,
}

pub struct PaneScrollbackWindow {
    /// Local projection width, or zero for legacy snapshot windows.
    pub projection_width: usize,
    /// Per-row content origins; empty for legacy physical snapshot windows.
    pub row_anchors: Vec<CapturedHistoryAnchor>,
    /// Palette captured with these rows, independent of live-grid style IDs.
    pub palette: bmux_terminal_grid::StylePalette,
    pub scrollback_offset: usize,
    pub max_scrollback_offset: usize,
    pub total_scrolled_rows: u64,
    pub rows: Vec<PhysicalRow>,
}

impl PaneScrollbackWindow {
    /// Resolve selection against this projection, clipping an off-window logical
    /// endpoint by content order rather than its obsolete physical row number.
    #[must_use]
    pub fn selection_bounds(
        &self,
        view: &PaneScrollbackView,
    ) -> Option<(AttachScrollbackPosition, AttachScrollbackPosition)> {
        let base = ScrollbackViewportBase::from_scrolled_rows(
            self.total_scrolled_rows,
            self.scrollback_offset,
        );
        let Some((anchor, last_position)) = view
            .captured_selection
            .filter(|(_, position)| Some(*position) == view.selection_anchor)
        else {
            return view.selection_bounds(base);
        };
        let first = *self.row_anchors.first()?;
        if anchor.capture_id != first.capture_id {
            return None;
        }
        let position = self.position_for_anchor(anchor).or_else(|| {
            let key = (anchor.line_index, anchor.column);
            if key < (first.line_index, first.column) {
                Some(AttachScrollbackPosition {
                    line: base.top_line(),
                    col: 0,
                })
            } else {
                let last = *self.row_anchors.last()?;
                let row = self.rows.len().checked_sub(1)?;
                let col = self.rows.get(row)?.cells().len().saturating_sub(1);
                (key > (last.line_index, last.column.saturating_add(col))).then_some(
                    AttachScrollbackPosition {
                        line: base.line_for_viewport_row(row),
                        col,
                    },
                )
            }
        })?;
        let mut projected = *view;
        // The saved projection is only a change detector, never a row identity.
        debug_assert_eq!(view.selection_anchor, Some(last_position));
        projected.selection_anchor = Some(position);
        projected.selection_bounds(base)
    }
    /// Locate an anchor in this bounded projection; never clamp missing content.
    #[must_use]
    pub fn position_for_anchor(
        &self,
        anchor: CapturedHistoryAnchor,
    ) -> Option<AttachScrollbackPosition> {
        let base = ScrollbackViewportBase::from_scrolled_rows(
            self.total_scrolled_rows,
            self.scrollback_offset,
        );
        self.row_anchors
            .iter()
            .enumerate()
            .find_map(|(row, origin)| {
                if origin.capture_id != anchor.capture_id || origin.line_index != anchor.line_index
                {
                    return None;
                }
                let col = anchor.column.checked_sub(origin.column)?;
                (self.content_anchor(row, col)? == anchor).then_some(AttachScrollbackPosition {
                    line: base.line_for_viewport_row(row),
                    col,
                })
            })
    }

    /// Preserve visible content endpoints when replacing a frozen projection.
    /// Off-window endpoints retain their existing physical numbering until a
    /// complete capture-wide index is available. Scrolling intentionally keeps
    /// the navigation cursor viewport-relative.
    pub fn remap_view_from(&self, previous: &Self, view: &mut PaneScrollbackView) {
        // Snapshot and logical projections do not share row identity. Until
        // the captured live tail carries logical origins, fail closed rather
        // than highlighting/copying unrelated text with stale physical bounds.
        if self.row_anchors.is_empty() != previous.row_anchors.is_empty() {
            view.captured_selection = None;
            view.selection_anchor = None;
            return;
        }
        let old_base = ScrollbackViewportBase::from_scrolled_rows(
            previous.total_scrolled_rows,
            previous.scrollback_offset,
        );
        if view
            .captured_selection
            .is_some_and(|(_, position)| Some(position) != view.selection_anchor)
        {
            view.captured_selection = None;
        }
        if view.captured_selection.is_none()
            && let Some(position) = view.selection_anchor
            && let Some(row) = position
                .line
                .checked_sub(old_base.top_line())
                .and_then(|row| usize::try_from(row).ok())
            && let Some(anchor) = previous.content_anchor(row, position.col)
        {
            view.captured_selection = Some((anchor, position));
        }
        if let Some((anchor, _)) = view.captured_selection {
            if self
                .row_anchors
                .first()
                .is_some_and(|origin| origin.capture_id != anchor.capture_id)
            {
                view.captured_selection = None;
                view.selection_anchor = None;
            } else if let Some(position) = self.position_for_anchor(anchor) {
                view.selection_anchor = Some(position);
                view.captured_selection = Some((anchor, position));
            }
        }
        if previous.scrollback_offset == self.scrollback_offset
            && let Some(anchor) = previous.content_anchor(view.cursor.row, view.cursor.col)
            && let Some(position) = self.position_for_anchor(anchor)
        {
            let base = ScrollbackViewportBase::from_scrolled_rows(
                self.total_scrolled_rows,
                self.scrollback_offset,
            );
            if let Some(row) = position
                .line
                .checked_sub(base.top_line())
                .and_then(|row| usize::try_from(row).ok())
            {
                view.cursor.row = row;
                view.cursor.col = position.col;
            }
        }
    }

    /// Translate a displayed cell into capture coordinates. Padding and wide
    /// continuation cells are not independent selectable content anchors.
    #[must_use]
    pub fn content_anchor(&self, row: usize, column: usize) -> Option<CapturedHistoryAnchor> {
        let mut anchor = *self.row_anchors.get(row)?;
        let cell = self.rows.get(row)?.cells().get(column)?;
        if cell.is_wide_continuation() {
            return None;
        }
        anchor.column = anchor.column.checked_add(column)?;
        Some(anchor)
    }
}

impl PaneRenderBuffer {
    /// Selection bounds in the same numbering as the currently displayed rows.
    #[must_use]
    pub fn scrollback_selection_bounds(
        &self,
        view: &PaneScrollbackView,
    ) -> Option<(AttachScrollbackPosition, AttachScrollbackPosition)> {
        self.scrollback_window
            .as_ref()
            .filter(|window| {
                window.scrollback_offset == view.offset && !window.row_anchors.is_empty()
            })
            .map_or_else(
                || view.selection_bounds(self.scrollback_viewport_base(Some(view))),
                |window| window.selection_bounds(view),
            )
    }

    /// Use a captured palette only when its window is also the displayed one.
    #[must_use]
    pub fn scrollback_palette(
        &self,
        view: Option<&PaneScrollbackView>,
    ) -> &bmux_terminal_grid::StylePalette {
        self.scrollback_window
            .as_ref()
            .filter(|window| view.is_some_and(|view| view.offset == window.scrollback_offset))
            .map_or_else(
                || self.terminal_grid.grid().palette(),
                |window| &window.palette,
            )
    }

    /// Rows to draw for this pane, plus the line numbering they are drawn in.
    ///
    /// This is the single source of truth for "what is currently on screen for
    /// this pane". Both the renderer's selection highlight and the selection
    /// copy path must go through it, otherwise they can disagree about which
    /// history line each viewport row holds.
    ///
    /// The server-provided window is authoritative when it matches the view's
    /// offset. Otherwise the client's own grid is projected at that offset: it
    /// mirrors the same output stream, so it is a far better approximation than
    /// blank rows while the window request is in flight — which matters most at
    /// offset 0, where a mouse selection enters scrollback with no window yet.
    #[must_use]
    pub fn scrollback_render_window(
        &self,
        view: Option<&PaneScrollbackView>,
        height: usize,
    ) -> (Vec<PhysicalRow>, ScrollbackViewportBase) {
        let grid = self.terminal_grid.grid();
        let offset = view.map_or(0, |view| view.offset);
        self.scrollback_window
            .as_ref()
            .filter(|window| view.is_some() && window.scrollback_offset == offset)
            .map_or_else(
                || {
                    (
                        grid.display_rows(offset, height),
                        ScrollbackViewportBase::from_scrolled_rows(
                            grid.total_scrolled_rows(),
                            offset,
                        ),
                    )
                },
                |window| {
                    (
                        window.rows.clone(),
                        ScrollbackViewportBase::from_scrolled_rows(
                            window.total_scrolled_rows,
                            window.scrollback_offset,
                        ),
                    )
                },
            )
    }

    /// Line numbering currently on screen for this pane.
    #[must_use]
    pub fn scrollback_viewport_base(
        &self,
        view: Option<&PaneScrollbackView>,
    ) -> ScrollbackViewportBase {
        let grid = self.terminal_grid.grid();
        let offset = view.map_or(0, |view| view.offset);
        self.scrollback_window
            .as_ref()
            .filter(|window| view.is_some() && window.scrollback_offset == offset)
            .map_or_else(
                || ScrollbackViewportBase::from_scrolled_rows(grid.total_scrolled_rows(), offset),
                |window| {
                    ScrollbackViewportBase::from_scrolled_rows(
                        window.total_scrolled_rows,
                        window.scrollback_offset,
                    )
                },
            )
    }
}

impl Default for PaneRenderBuffer {
    fn default() -> Self {
        Self {
            terminal_grid: TerminalGridStream::new(80, 24, GridLimits::default())
                .expect("default pane render grid dimensions are valid"),
            protocol_tracker: TerminalProtocolTracker::new(),
            prev_rows: Vec::new(),
            sync_update_in_progress: false,
            expected_stream_start: None,
            scrollback_window: None,
            extension_render_cache: BTreeMap::new(),
            extension_layer_snapshot_cache: BTreeMap::new(),
            extension_retained_layer_cache: BTreeMap::new(),
            visual_row_fingerprints: PaneVisualRowFingerprintCache::default(),
        }
    }
}

#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct AttachPaneMouseProtocolHints {
    pub mode_hints: BTreeMap<Uuid, AttachMouseProtocolState>,
    pub input_mode_hints: BTreeMap<Uuid, AttachInputModeState>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn captured_window_remaps_selection_and_cursor_across_widths() {
        use bmux_terminal_grid::{GridLimits, TerminalGridStream};
        let capture_id = Uuid::new_v4();
        let window = |width: usize| PaneScrollbackWindow {
            projection_width: width,
            row_anchors: (0..6 / width)
                .map(|row| CapturedHistoryAnchor {
                    capture_id,
                    line_index: 0,
                    column: row * width,
                })
                .collect(),
            rows: (0..6 / width)
                .map(|_| {
                    let mut stream = TerminalGridStream::new(
                        u16::try_from(width).unwrap(),
                        1,
                        GridLimits::default(),
                    )
                    .unwrap();
                    stream.process("x".repeat(width).as_bytes());
                    stream.grid().viewport_rows()[0].clone()
                })
                .collect(),
            palette: bmux_terminal_grid::StylePalette::default(),
            scrollback_offset: 1,
            max_scrollback_offset: 10,
            total_scrolled_rows: 10,
        };
        let wide = window(3);
        let narrow = window(2);
        let mut view = PaneScrollbackView {
            captured_selection: None,
            offset: 1,
            cursor: AttachScrollbackCursor { row: 1, col: 2 },
            selection_anchor: Some(AttachScrollbackPosition { line: 10, col: 1 }),
            pin: None,
        };
        narrow.remap_view_from(&wide, &mut view);
        assert_eq!(
            view.selection_anchor,
            Some(AttachScrollbackPosition { line: 11, col: 0 })
        );
        assert_eq!(view.cursor, AttachScrollbackCursor { row: 2, col: 1 });
        wide.remap_view_from(&narrow, &mut view);
        assert_eq!(
            view.selection_anchor,
            Some(AttachScrollbackPosition { line: 10, col: 1 })
        );
        assert_eq!(view.cursor, AttachScrollbackCursor { row: 1, col: 2 });
        let mut absent = window(3);
        for origin in &mut absent.row_anchors {
            origin.line_index = 1;
        }
        absent.remap_view_from(&wide, &mut view);
        assert_eq!(view.captured_selection.unwrap().0.line_index, 0);
        assert_eq!(
            absent.selection_bounds(&view).unwrap().0,
            AttachScrollbackPosition { line: 9, col: 0 }
        );
        narrow.remap_view_from(&absent, &mut view);
        assert_eq!(
            view.selection_anchor,
            Some(AttachScrollbackPosition { line: 11, col: 0 })
        );
        // A snapshot transition cannot reuse logical projection row numbers.
        let mut snapshot = window(2);
        snapshot.row_anchors.clear();
        view.selection_anchor = Some(AttachScrollbackPosition { line: 11, col: 0 });
        snapshot.remap_view_from(&narrow, &mut view);
        assert!(view.selection_anchor.is_none());
        assert!(view.captured_selection.is_none());
        view.selection_anchor = Some(AttachScrollbackPosition { line: 10, col: 0 });
        narrow.remap_view_from(&snapshot, &mut view);
        assert!(view.selection_anchor.is_none());
        // Explicitly clearing selection must not resurrect its saved endpoint.
        view.selection_anchor = None;
        wide.remap_view_from(&narrow, &mut view);
        assert!(view.captured_selection.is_none());
        assert!(view.selection_anchor.is_none());
    }

    #[test]
    fn selection_rebase_preserves_offsets_on_both_sides_of_viewport() {
        for (line, expected) in [(7, 97), (10, 100), (14, 104)] {
            let mut view = PaneScrollbackView {
                captured_selection: None,
                selection_anchor: Some(AttachScrollbackPosition { line, col: 5 }),
                offset: 0,
                cursor: AttachScrollbackCursor { row: 0, col: 0 },
                pin: None,
            };
            view.rebase_selection_anchor(
                ScrollbackViewportBase::from_scrolled_rows(10, 0),
                ScrollbackViewportBase::from_scrolled_rows(100, 0),
            );
            assert_eq!(
                view.selection_anchor,
                Some(AttachScrollbackPosition {
                    line: expected,
                    col: 5,
                })
            );
        }
    }

    #[test]
    fn selection_rebase_saturates_at_history_numbering_bounds() {
        for (line, new_top, expected) in [(0, 2, 0), (20, u64::MAX, u64::MAX)] {
            let mut view = PaneScrollbackView {
                captured_selection: None,
                selection_anchor: Some(AttachScrollbackPosition { line, col: 3 }),
                offset: 0,
                cursor: AttachScrollbackCursor { row: 0, col: 0 },
                pin: None,
            };
            view.rebase_selection_anchor(
                ScrollbackViewportBase::from_scrolled_rows(10, 0),
                ScrollbackViewportBase::from_scrolled_rows(new_top, 0),
            );
            assert_eq!(
                view.selection_anchor,
                Some(AttachScrollbackPosition {
                    line: expected,
                    col: 3,
                })
            );
        }
    }

    #[test]
    fn visual_row_fingerprint_cache_reuses_rows_and_invalidates_precisely() {
        let cache = PaneVisualRowFingerprintCache::default();
        let calls = AtomicUsize::new(0);

        let first = cache.get_or_compute(80, 24, 1, 3, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Some(10)
        });
        let cached = cache.get_or_compute(80, 24, 1, 3, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Some(11)
        });

        assert_eq!(first, Some(10));
        assert_eq!(cached, Some(10));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        cache.invalidate_rows(false, 2, &[4]);
        let still_cached = cache.get_or_compute(80, 24, 2, 3, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Some(12)
        });
        assert_eq!(still_cached, Some(10));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        cache.invalidate_rows(false, 3, &[3]);
        let recomputed = cache.get_or_compute(80, 24, 3, 3, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Some(13)
        });
        assert_eq!(recomputed, Some(13));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn visual_row_fingerprint_cache_recomputes_on_untracked_revision_change() {
        let cache = PaneVisualRowFingerprintCache::default();
        let calls = AtomicUsize::new(0);

        assert_eq!(
            cache.get_or_compute(80, 24, 1, 0, || {
                calls.fetch_add(1, Ordering::SeqCst);
                Some(1)
            }),
            Some(1)
        );
        assert_eq!(
            cache.get_or_compute(80, 24, 2, 0, || {
                calls.fetch_add(1, Ordering::SeqCst);
                Some(2)
            }),
            Some(2)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn visual_row_fingerprint_cache_resets_on_dimension_change() {
        let cache = PaneVisualRowFingerprintCache::default();
        let calls = AtomicUsize::new(0);

        assert_eq!(
            cache.get_or_compute(80, 24, 1, 0, || {
                calls.fetch_add(1, Ordering::SeqCst);
                Some(1)
            }),
            Some(1)
        );
        assert_eq!(
            cache.get_or_compute(81, 24, 1, 0, || {
                calls.fetch_add(1, Ordering::SeqCst);
                Some(2)
            }),
            Some(2)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
