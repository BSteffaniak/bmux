//! Bounded normal-screen projections owned by one local attach view.
//!
//! This is source correspondence, not layout or input authority. Obtain local
//! cells from the committed retained compositor (`content_cell_at`) first. Stage
//! a replacement projection with the output and install it only after complete
//! presentation; failed output must retain the previous projection and mapping.
use bmux_terminal_grid::{
    ContentAnchor, ContentBudget, ContentCursor, ContentPosition, ContentProjection, ContentRows,
    ContentSelectionRow, GridMode, HistorySliceError, StylePalette, TerminalGrid,
};
use uuid::Uuid;

const MAX_SOURCE_CELLS: usize = 262_144;
const MAX_SOURCE_BYTES: usize = 8 * 1024 * 1024;
const MAX_COLUMNS: usize = 1024;
const MAX_ROWS: usize = 512;
const MAX_WINDOW_CELLS: usize = 262_144;

const fn budget() -> ContentBudget {
    ContentBudget {
        cells: MAX_SOURCE_CELLS,
        bytes: MAX_SOURCE_BYTES,
    }
}

/// Caller-verified bindings. A new execution/generation must get a new execution
/// identity even if pane, local view, or content revision numbers are reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PaneZoomSource {
    pub view: Uuid,
    pub pane: Uuid,
    pub execution: Uuid,
}

/// One immutable prepared view. Two views of one source own separate width
/// indices. All rows, palette, cursor, and mappings belong to this exact capture.
/// The caller owns admission/eviction and must bound its number of local views.
pub(super) struct PaneZoomProjection {
    pub source: PaneZoomSource,
    pub revision: u64,
    pub capture_id: Uuid,
    pub columns: usize,
    pub height: usize,
    /// First row of the prepared source represented by `content.rows[0]`.
    pub first_row: usize,
    pub content: ContentRows,
    pub palette: StylePalette,
    /// Window-local insertion position and the atomically captured cursor state.
    /// A final full row may have `column == columns`; the compositor/backend
    /// must treat that as an insertion edge, not another cell or invented row.
    pub cursor: Option<(ContentPosition, ContentCursor)>,
    projection: ContentProjection,
    execution: ContentProjection,
    execution_width: usize,
    execution_height: usize,
    mode: GridMode,
}

impl PaneZoomProjection {
    /// Capture visible main-screen source only. Widening never reveals hidden
    /// progress/history; a split first line remains a capture-local fragment.
    /// `capture_id` must be fresh whenever the source/revision is replaced.
    ///
    /// Work, allocations and local geometry have explicit fixed ceilings. A
    /// rejection leaves the caller's current committed projection untouched.
    pub fn capture(
        source: PaneZoomSource,
        capture_id: Uuid,
        grid: &TerminalGrid,
        columns: usize,
        height: usize,
    ) -> Result<Self, HistorySliceError> {
        if grid.mode() != GridMode::Main {
            return Err(HistorySliceError::Unavailable);
        }
        if columns == 0 || height == 0 || columns > MAX_COLUMNS || height > MAX_ROWS {
            return Err(HistorySliceError::InvalidOffset);
        }
        if columns
            .checked_mul(height)
            .is_none_or(|cells| cells > MAX_WINDOW_CELLS)
        {
            return Err(HistorySliceError::BudgetExhausted);
        }
        let mut projection = grid.capture_viewport(capture_id.as_u128(), budget())?;
        // Same source and local line boundaries, indexed at execution width.
        // Retaining this bounded index avoids replay, grid cloning, and guessing
        // execution coordinates from a reflowed row's physical cell number.
        let mut execution = grid.capture_viewport(capture_id.as_u128(), budget())?;
        projection.prepare(columns, budget())?;
        execution.prepare(grid.width(), budget())?;
        let total = projection
            .row_count()
            .ok_or(HistorySliceError::Unavailable)?;
        let first_row = total.saturating_sub(height);
        let content = projection.window(first_row..total, MAX_SOURCE_BYTES)?;
        let cursor = projection
            .captured_cursor()
            .map(|cursor| {
                projection
                    .resolve_position(cursor.anchor, budget())
                    .map(|position| (position, cursor))
            })
            .transpose()?
            .and_then(|(position, cursor)| {
                let row = position.row.checked_sub(first_row)?;
                (row < height).then_some((ContentPosition { row, ..position }, cursor))
            });
        Ok(Self {
            source,
            revision: grid.content_revision(),
            capture_id,
            columns,
            height,
            first_row,
            content,
            palette: grid.palette().clone(),
            cursor,
            projection,
            execution,
            execution_width: grid.width(),
            execution_height: grid.height(),
            mode: GridMode::Main,
        })
    }

    /// Positioned alternate-screen output: retain rows and execution geometry,
    /// never join soft wraps or reflow them. The caller requests execution resize
    /// separately, then captures the authoritative result here.
    pub fn capture_positioned(
        source: PaneZoomSource,
        capture_id: Uuid,
        grid: &TerminalGrid,
    ) -> Result<Self, HistorySliceError> {
        if grid.mode() != GridMode::Alternate
            || grid.width() > MAX_COLUMNS
            || grid.height() > MAX_ROWS
        {
            return Err(HistorySliceError::Unavailable);
        }
        if grid.width().saturating_mul(grid.height()) > MAX_WINDOW_CELLS {
            return Err(HistorySliceError::BudgetExhausted);
        }
        let rows = grid.screen_window(0..grid.width(), 0..grid.height(), budget())?;
        let cells = rows
            .iter()
            .map(|row| {
                row.cells()
                    .iter()
                    .filter(|cell| !cell.is_wide_continuation())
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let mut projection = ContentProjection::from_lines(
            capture_id.as_u128(),
            grid.content_revision(),
            cells.iter().map(|cells| (cells.as_slice(), false)),
            false,
            false,
            budget(),
        )?;
        let mut execution = ContentProjection::from_lines(
            capture_id.as_u128(),
            grid.content_revision(),
            cells.iter().map(|cells| (cells.as_slice(), false)),
            false,
            false,
            budget(),
        )?;
        projection.prepare(grid.width(), budget())?;
        execution.prepare(grid.width(), budget())?;
        let content = projection.window(0..grid.height(), MAX_SOURCE_BYTES)?;
        let cursor = grid.cursor();
        Ok(Self {
            source,
            revision: grid.content_revision(),
            capture_id,
            columns: grid.width(),
            height: grid.height(),
            first_row: 0,
            content,
            palette: grid.palette().clone(),
            cursor: Some((
                ContentPosition {
                    row: cursor.row,
                    column: cursor.col,
                },
                ContentCursor {
                    anchor: ContentAnchor {
                        capture: capture_id.as_u128(),
                        line: cursor.row,
                        column: cursor.col,
                    },
                    visible: cursor.visible,
                    pending_wrap: false,
                },
            )),
            projection,
            execution,
            execution_width: grid.width(),
            execution_height: grid.height(),
            mode: GridMode::Alternate,
        })
    }

    /// Return total capture-owned allocation used by the local admission gate.
    pub fn retained_bytes(&self) -> usize {
        self.projection
            .retained_bytes()
            .saturating_add(self.execution.retained_bytes())
            .saturating_add(
                self.content
                    .rows
                    .iter()
                    .map(|row| {
                        row.cells()
                            .iter()
                            .map(|cell| {
                                std::mem::size_of::<bmux_terminal_grid::Cell>() + cell.text().len()
                            })
                            .sum::<usize>()
                    })
                    .sum::<usize>(),
            )
    }

    /// Use before dispatching against a current replica. Capture identity scopes
    /// selection separately; revision alone is never an execution identity.
    pub fn matches_source(&self, source: PaneZoomSource, grid: &TerminalGrid) -> bool {
        self.source == source
            && self.revision == grid.content_revision()
            && grid.mode() == self.mode
            && self.execution_width == grid.width()
            && self.execution_height == grid.height()
    }

    /// Local cell supplied by the retained compositor -> logical selection
    /// anchor. Wide continuation slots name the complete glyph's start. Empty
    /// padding is not selectable source. No root-coordinate hit map lives here.
    pub fn source_at(&self, position: ContentPosition) -> Result<ContentAnchor, HistorySliceError> {
        if position.row >= self.content.rows.len() || position.column >= self.columns {
            return Err(HistorySliceError::InvalidOffset);
        }
        self.projection.source_at(
            ContentPosition {
                row: self.first_row + position.row,
                ..position
            },
            budget(),
        )
    }

    /// Reverse correspondence to the captured execution viewport. This must be
    /// gated by the committed retained hit and current verified source binding;
    /// stale output must not be used to address a replacement execution.
    pub fn execution_at(
        &self,
        source: PaneZoomSource,
        grid: &TerminalGrid,
        position: ContentPosition,
    ) -> Result<ContentPosition, HistorySliceError> {
        if !self.matches_source(source, grid) {
            return Err(HistorySliceError::StaleRevision);
        }
        let anchor = self.source_at(position)?;
        let position = self.execution.resolve_position(anchor, budget())?;
        if position.row >= self.execution_height || position.column >= self.execution_width {
            return Err(HistorySliceError::InvalidOffset);
        }
        Ok(position)
    }

    /// Build local-grid output using only winning clips and the transform from
    /// the retained compositor. The style callback applies the caller's existing
    /// appearance/selection effects to the captured palette, not the live one.
    /// This creates no hit map. Final insertion edges paint in the last cell;
    /// `cursor` retains the exact insertion position for source correspondence.
    #[cfg(test)]
    pub fn grid_fragment(
        &self,
        id: impl Into<String>,
        transform: bmux_tui::grid_fragment::GridTransform,
        winning_clips: Vec<bmux_tui::geometry::Rect>,
        style: impl FnMut(bmux_terminal_grid::Style) -> bmux_tui::style::Style,
    ) -> Option<bmux_tui::grid_fragment::PreparedGridFragment> {
        self.grid_fragment_with_selection(id, transform, winning_clips, style, |_| false, true)
    }

    pub fn grid_fragment_with_selection(
        &self,
        id: impl Into<String>,
        transform: bmux_tui::grid_fragment::GridTransform,
        winning_clips: Vec<bmux_tui::geometry::Rect>,
        mut style: impl FnMut(bmux_terminal_grid::Style) -> bmux_tui::style::Style,
        mut selected: impl FnMut(ContentAnchor) -> bool,
        cursor_owner: bool,
    ) -> Option<bmux_tui::grid_fragment::PreparedGridFragment> {
        use bmux_tui::geometry::{Point, Rect};
        let columns = u16::try_from(self.columns).ok()?;
        let height = u16::try_from(self.height).ok()?;
        let mut buffer = bmux_tui::buffer::Buffer::empty(Rect::new(0, 0, columns, height));
        for (y, row) in self.content.rows.iter().enumerate() {
            let mut anchor = self.content.sources.get(y)?.start;
            for (x, cell) in row.cells().iter().enumerate().take(self.columns) {
                if cell.is_wide_continuation() {
                    continue;
                }
                // TUI's ordinary buffer setter replaces a squeezed wide glyph
                // with a blank. Fail preparation instead of publishing text and
                // selection correspondence for a glyph that was never painted.
                if cell.width() == 2 && x + 1 >= self.columns {
                    return None;
                }
                let mut cell_style = style(self.palette.get(cell.style()));
                if selected(anchor) {
                    cell_style = cell_style.add_modifier(bmux_tui::style::Modifier::REVERSED);
                }
                buffer.set_cell(
                    Point::new(u16::try_from(x).ok()?, u16::try_from(y).ok()?),
                    cell.text(),
                    cell_style,
                );
                anchor.column += usize::from(cell.width()).max(1);
            }
        }
        let fragment = bmux_tui::grid_fragment::PreparedGridFragment::new(
            id,
            std::sync::Arc::new(buffer),
            transform,
            winning_clips,
        )?;
        if let Some((position, cursor)) = self.cursor.filter(|_| cursor_owner) {
            // A full-line insertion edge is drawn in its last cell, matching
            // the terminal's pending-wrap cursor. Keep the exact insertion
            // position separately for source mapping, never invent a row.
            fragment.with_cursor(bmux_tui::frame::Cursor {
                position: Point::new(
                    u16::try_from(position.column.min(self.columns.saturating_sub(1))).ok()?,
                    u16::try_from(position.row).ok()?,
                ),
                visible: cursor.visible,
            })
        } else {
            Some(fragment)
        }
    }

    /// Map an execution-owned image allocation through the same source anchors
    /// as text. Non-rectangular reflow is explicitly unavailable, never guessed.
    #[cfg(any(
        feature = "image-sixel",
        feature = "image-kitty",
        feature = "image-iterm2"
    ))]
    pub fn image_rect(&self, rect: bmux_tui::geometry::Rect) -> Option<bmux_tui::geometry::Rect> {
        let map = |row, column| {
            let anchor = self
                .execution
                .source_at(ContentPosition { row, column }, budget())
                .ok()?;
            let position = self.projection.resolve_position(anchor, budget()).ok()?;
            Some(ContentPosition {
                row: position.row.checked_sub(self.first_row)?,
                column: position.column,
            })
        };
        let origin = map(usize::from(rect.y), usize::from(rect.x))?;
        for y in 0..usize::from(rect.height) {
            for x in 0..usize::from(rect.width) {
                let position = map(usize::from(rect.y) + y, usize::from(rect.x) + x)?;
                if position.row != origin.row + y || position.column != origin.column + x {
                    return None;
                }
            }
        }
        Some(bmux_tui::geometry::Rect::new(
            u16::try_from(origin.column).ok()?,
            u16::try_from(origin.row).ok()?,
            rect.width,
            rect.height,
        ))
    }

    /// Matching UTF-8/display-cell correspondence for the projected window.
    pub fn selection_rows(&self) -> Result<Vec<ContentSelectionRow>, HistorySliceError> {
        self.projection.selection_window(
            self.first_row..self.first_row + self.content.rows.len(),
            budget(),
        )
    }

    /// Export one validated UTF-8 line interval; clipboard effects stay with the
    /// caller. Foreign/stale capture anchors are rejected by the projection.
    pub fn export_text(
        &self,
        capture: u128,
        line: usize,
        bytes: std::ops::Range<usize>,
    ) -> Result<String, HistorySliceError> {
        self.projection.export_text(capture, line, bytes, budget())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_terminal_grid::{GridLimits, TerminalGridStream};

    fn source(view: u128) -> PaneZoomSource {
        PaneZoomSource {
            view: Uuid::from_u128(view),
            pane: Uuid::from_u128(2),
            execution: Uuid::from_u128(3),
        }
    }

    fn stream() -> TerminalGridStream {
        let mut stream = TerminalGridStream::new(8, 3, GridLimits::default()).unwrap();
        stream.process("ab界cd\r\nnext".as_bytes());
        stream
    }

    #[test]
    fn two_views_reflow_without_mutation_and_reverse_wide_cells() {
        let stream = stream();
        let grid = stream.grid();
        let before = grid.snapshot(0, 3);
        let narrow =
            PaneZoomProjection::capture(source(1), Uuid::from_u128(10), grid, 3, 8).unwrap();
        let wide =
            PaneZoomProjection::capture(source(4), Uuid::from_u128(11), grid, 12, 8).unwrap();
        assert!(narrow.content.rows.len() > wide.content.rows.len());
        let local = ContentPosition { row: 1, column: 1 };
        assert_eq!(narrow.source_at(local).unwrap().column, 2);
        assert_eq!(
            narrow.execution_at(source(1), grid, local).unwrap(),
            ContentPosition { row: 0, column: 2 }
        );
        assert_eq!(grid.snapshot(0, 3), before);
        assert_eq!(narrow.selection_rows().unwrap()[1].cells[0].columns, 0..2);
        assert_eq!(narrow.export_text(10, 0, 2..5).unwrap(), "界");
        assert_eq!(
            narrow.export_text(11, 0, 2..5),
            Err(HistorySliceError::StaleRevision)
        );
    }

    #[test]
    #[cfg(any(
        feature = "image-sixel",
        feature = "image-kitty",
        feature = "image-iterm2"
    ))]
    fn image_rect_tracks_source_anchor_and_rejects_split_reflow() {
        use bmux_tui::geometry::Rect;
        let stream = stream();
        let wide = PaneZoomProjection::capture(source(1), Uuid::from_u128(19), stream.grid(), 8, 3)
            .unwrap();
        assert_eq!(
            wide.image_rect(Rect::new(0, 0, 2, 1)),
            Some(Rect::new(0, 0, 2, 1))
        );
        let narrow =
            PaneZoomProjection::capture(source(2), Uuid::from_u128(20), stream.grid(), 3, 5)
                .unwrap();
        // A rectangle spanning a reflow boundary is not a rectangular image.
        assert!(narrow.image_rect(Rect::new(0, 0, 4, 1)).is_none());
    }

    #[test]
    fn execution_input_rejects_replacement_and_stale_revision() {
        let mut stream = stream();
        let view = PaneZoomProjection::capture(source(1), Uuid::from_u128(12), stream.grid(), 4, 3)
            .unwrap();
        let local = ContentPosition { row: 0, column: 0 };
        let replacement = PaneZoomSource {
            execution: Uuid::from_u128(99),
            ..source(1)
        };
        assert_eq!(
            view.execution_at(replacement, stream.grid(), local),
            Err(HistorySliceError::StaleRevision)
        );
        stream.process(b"!");
        assert_eq!(
            view.execution_at(source(1), stream.grid(), local),
            Err(HistorySliceError::StaleRevision)
        );
        // The committed capture remains readable for local selection/export.
        assert!(view.source_at(local).is_ok());
    }

    #[test]
    fn fragment_uses_caller_transform_clips_and_captured_cells() {
        use bmux_tui::geometry::{Point, Rect};
        use bmux_tui::grid_fragment::GridTransform;
        let stream = stream();
        let view = PaneZoomProjection::capture(source(1), Uuid::from_u128(16), stream.grid(), 6, 3)
            .unwrap();
        let transform = GridTransform::new(Point::new(5, 7), 1500).unwrap();
        let clips = vec![Rect::new(5, 7, 3, 2)];
        let fragment = view
            .grid_fragment("view", transform, clips.clone(), |_| {
                bmux_tui::style::Style::new()
            })
            .unwrap();
        assert_eq!(fragment.transform(), transform);
        assert_eq!(fragment.visible_clips(), clips);
        assert_eq!(
            fragment.buffer().get(Point::new(2, 0)).unwrap().symbol,
            "界"
        );
        assert!(
            fragment
                .buffer()
                .get(Point::new(3, 0))
                .unwrap()
                .is_wide_continuation()
        );
        let squeezed =
            PaneZoomProjection::capture(source(1), Uuid::from_u128(17), stream.grid(), 1, 20)
                .unwrap();
        assert!(
            squeezed
                .grid_fragment("view", transform, clips, |_| bmux_tui::style::Style::new())
                .is_none()
        );
    }

    #[test]
    fn hidden_history_stays_hidden_when_widening() {
        let mut stream = TerminalGridStream::new(4, 2, GridLimits::default()).unwrap();
        stream.process(b"abcdefghijkl");
        let view =
            PaneZoomProjection::capture(source(1), Uuid::from_u128(18), stream.grid(), 20, 4)
                .unwrap();
        assert_eq!(view.export_text(18, 0, 0..8).unwrap(), "efghijkl");
        assert_eq!(
            view.execution_at(
                source(1),
                stream.grid(),
                ContentPosition { row: 0, column: 4 }
            )
            .unwrap(),
            ContentPosition { row: 1, column: 0 }
        );
        assert_eq!(
            view.source_at(ContentPosition { row: 0, column: 8 }),
            Err(HistorySliceError::InvalidOffset)
        );
    }

    #[test]
    fn alternate_projection_preserves_positioned_row_breaks() {
        let mut stream = TerminalGridStream::new(4, 3, GridLimits::default()).unwrap();
        stream.process(b"main\x1b[?1049habcdef");
        let before = stream.grid().snapshot(0, 3);
        let projection =
            PaneZoomProjection::capture_positioned(source(1), Uuid::from_u128(20), stream.grid())
                .unwrap();
        assert_eq!(projection.columns, 4);
        assert_eq!(projection.height, 3);
        assert_eq!(projection.content.rows.len(), 3);
        assert_eq!(
            projection
                .execution_at(
                    source(1),
                    stream.grid(),
                    ContentPosition { row: 1, column: 0 }
                )
                .unwrap(),
            ContentPosition { row: 1, column: 0 }
        );
        assert_eq!(stream.grid().snapshot(0, 3), before);
    }

    #[test]
    fn cursor_tail_and_admission_are_explicit() {
        let mut stream = TerminalGridStream::new(4, 2, GridLimits::default()).unwrap();
        stream.process(b"abcdefgh");
        let view = PaneZoomProjection::capture(source(1), Uuid::from_u128(13), stream.grid(), 2, 2)
            .unwrap();
        assert_eq!(view.first_row, 2);
        let (position, cursor) = view.cursor.unwrap();
        assert_eq!(position, ContentPosition { row: 1, column: 2 });
        assert!(cursor.pending_wrap);
        assert!(view.content.has_more_above);
        assert_eq!(
            view.execution_at(
                source(1),
                stream.grid(),
                ContentPosition { row: 0, column: 0 }
            )
            .unwrap(),
            ContentPosition { row: 1, column: 0 }
        );
        assert!(matches!(
            PaneZoomProjection::capture(source(1), Uuid::from_u128(14), stream.grid(), 1024, 512),
            Err(HistorySliceError::BudgetExhausted)
        ));
        stream.process(b"\x1b[?1049h");
        assert!(matches!(
            PaneZoomProjection::capture(source(1), Uuid::from_u128(15), stream.grid(), 4, 2),
            Err(HistorySliceError::Unavailable)
        ));
    }
}
