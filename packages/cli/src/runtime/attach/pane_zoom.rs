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

/// Captured history is already projected by the typed history service. Keep its
/// palette and images together with the exact checked source rows. Never use
/// this mapping for execution input: frozen history is not an execution screen.
pub(super) struct PaneZoomHistoryProjection {
    pub source: PaneZoomSource,
    pub pin: bmux_attach_pipeline::ScrollbackPin,
    pub content: bmux_terminal_grid::FrozenContentRows,
    pub palette: StylePalette,
    pub images: Vec<bmux_attach_image_protocol::AttachPaneImage>,
    top: bmux_attach_pipeline::CapturedHistoryAnchor,
    retained_bytes: usize,
}

/// Position policy is explicit: a top source origin is stable across width
/// changes; bottom-follow intentionally tracks the frozen capture's tail.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PaneZoomHistoryOrigin {
    Top(bmux_attach_pipeline::CapturedHistoryAnchor),
    BottomFollow,
}

#[cfg(test)]
impl PaneZoomHistoryOrigin {
    /// Adapt top anchoring to the existing history query's bottom-anchor and
    /// signed projected-row delta contract. Pass numeric offset zero for follow.
    pub fn query(
        self,
        columns: usize,
        height: usize,
    ) -> Result<
        (
            usize,
            Option<bmux_attach_pipeline::CapturedHistoryAnchor>,
            isize,
        ),
        HistorySliceError,
    > {
        if columns == 0
            || columns > MAX_COLUMNS
            || height == 0
            || height > 256
            || columns.saturating_mul(height) > MAX_WINDOW_CELLS
        {
            return Err(HistorySliceError::BudgetExhausted);
        }
        Ok(match self {
            Self::Top(anchor) => (
                columns,
                Some(anchor),
                -isize::try_from(height - 1).map_err(|_| HistorySliceError::InvalidOffset)?,
            ),
            Self::BottomFollow => (columns, None, 0),
        })
    }
}

impl PaneZoomHistoryProjection {
    /// Bounded immutable reply copy. Admission precedes row/image duplication.
    pub fn from_borrowed_window(
        source: PaneZoomSource,
        pin: bmux_attach_pipeline::ScrollbackPin,
        window: &bmux_attach_pipeline::PaneScrollbackWindow,
    ) -> Result<Self, HistorySliceError> {
        if window.projection_width == 0
            || window.projection_width > MAX_COLUMNS
            || window.rows.len() > 256
            || window.row_anchors.len() != window.rows.len()
            || window.images.len() > 4096
        {
            return Err(HistorySliceError::BudgetExhausted);
        }
        let mut bytes = 0usize;
        let mut cells = 0usize;
        for row in &window.rows {
            cells = cells.saturating_add(window.projection_width.max(row.cells().len()));
            if cells > MAX_SOURCE_CELLS {
                return Err(HistorySliceError::BudgetExhausted);
            }
            for cell in row.cells() {
                bytes = bytes.saturating_add(
                    std::mem::size_of::<bmux_terminal_grid::Cell>() + cell.text().len(),
                );
                if bytes > MAX_SOURCE_BYTES {
                    return Err(HistorySliceError::BudgetExhausted);
                }
            }
        }
        for image in &window.images {
            bytes = bytes.saturating_add(image.raw_data.len());
            if bytes > MAX_SOURCE_BYTES {
                return Err(HistorySliceError::BudgetExhausted);
            }
        }
        Self::from_window(
            source,
            pin,
            bmux_attach_pipeline::PaneScrollbackWindow {
                images: window.images.clone(),
                projection_width: window.projection_width,
                row_anchors: window.row_anchors.clone(),
                palette: window.palette.clone(),
                scrollback_offset: window.scrollback_offset,
                max_scrollback_offset: window.max_scrollback_offset,
                total_scrolled_rows: window.total_scrolled_rows,
                rows: window.rows.clone(),
            },
        )
    }

    /// Render the already projected immutable rows directly. Source origins and
    /// palette belong to this same capture; never feed a flattened buffer back
    /// into a terminal parser. A history fragment never claims an input cursor.
    pub fn grid_fragment_with_selection(
        &self,
        id: impl Into<String>,
        transform: bmux_tui::grid_fragment::GridTransform,
        clips: Vec<bmux_tui::geometry::Rect>,
        height: usize,
        mut style: impl FnMut(bmux_terminal_grid::Style) -> bmux_tui::style::Style,
        mut selected: impl FnMut(ContentAnchor) -> bool,
    ) -> Option<bmux_tui::grid_fragment::PreparedGridFragment> {
        use bmux_tui::{
            buffer::Buffer,
            geometry::{Point, Rect},
        };
        if height == 0 || height > 256 || self.content.rows().len() > height {
            return None;
        }
        let mut buffer = Buffer::empty(Rect::new(
            0,
            0,
            u16::try_from(self.content.width()).ok()?,
            u16::try_from(height).ok()?,
        ));
        for (y, row) in self.content.rows().iter().enumerate() {
            let mut anchor = self.content.anchors()[y];
            for (x, cell) in row.cells().iter().enumerate() {
                if cell.is_wide_continuation() {
                    continue;
                }
                if cell.width() == 2 && x + 1 >= self.content.width() {
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
            let end = self.content.row_end(y)?;
            for x in row.cells().len()..self.content.width() {
                if anchor.column >= end.column {
                    break;
                }
                if selected(anchor) {
                    buffer.set_cell(
                        Point::new(u16::try_from(x).ok()?, u16::try_from(y).ok()?),
                        " ",
                        style(bmux_terminal_grid::Style::default())
                            .add_modifier(bmux_tui::style::Modifier::REVERSED),
                    );
                }
                anchor.column += 1;
            }
        }
        bmux_tui::grid_fragment::PreparedGridFragment::new(
            id,
            std::sync::Arc::new(buffer),
            transform,
            clips,
        )
    }

    /// Consume a complete immutable history reply at the requested local width.
    /// Legacy windows without source origins are deliberately unsupported.
    /// Caller must stage this alongside output and publish only after success.
    pub fn from_window(
        source: PaneZoomSource,
        pin: bmux_attach_pipeline::ScrollbackPin,
        window: bmux_attach_pipeline::PaneScrollbackWindow,
    ) -> Result<Self, HistorySliceError> {
        let capture = pin.capture.ok_or(HistorySliceError::Unavailable)?;
        if window.projection_width == 0
            || window.projection_width > MAX_COLUMNS
            || window.rows.len() > MAX_ROWS
            || window.total_scrolled_rows != pin.total_scrolled_rows
            || window.row_anchors.len() != window.rows.len()
            || window.row_anchors.iter().any(|anchor| {
                anchor.capture_id != capture.identity
                    || u64::from(anchor.line_index)
                        >= capture.lines.saturating_add(u64::from(capture.height))
            })
        {
            return Err(HistorySliceError::InvalidOffset);
        }
        if window.images.capacity()
            > MAX_SOURCE_BYTES / std::mem::size_of::<bmux_attach_image_protocol::AttachPaneImage>()
        {
            return Err(HistorySliceError::BudgetExhausted);
        }
        let image_bytes = window
            .images
            .iter()
            .try_fold(
                window.images.capacity().saturating_mul(std::mem::size_of::<
                    bmux_attach_image_protocol::AttachPaneImage,
                >()),
                |bytes, image| bytes.checked_add(image.raw_data.capacity()),
            )
            .ok_or(HistorySliceError::BudgetExhausted)?;
        let palette_bytes = window
            .palette
            .styles()
            .len()
            .saturating_mul(std::mem::size_of::<bmux_terminal_grid::Style>());
        let remaining = MAX_SOURCE_BYTES
            .checked_sub(image_bytes)
            .and_then(|bytes| bytes.checked_sub(palette_bytes))
            .ok_or(HistorySliceError::BudgetExhausted)?;
        let top = *window
            .row_anchors
            .first()
            .ok_or(HistorySliceError::InvalidOffset)?;
        let mut anchors = Vec::new();
        anchors
            .try_reserve_exact(window.row_anchors.len())
            .map_err(|_| HistorySliceError::BudgetExhausted)?;
        anchors.extend(window.row_anchors.iter().map(|anchor| ContentAnchor {
            capture: anchor.capture_id.as_u128(),
            line: anchor.line_index as usize,
            column: anchor.column,
        }));
        let content = bmux_terminal_grid::FrozenContentRows::new(
            window.projection_width,
            window.rows,
            anchors,
            ContentBudget {
                cells: MAX_SOURCE_CELLS,
                bytes: remaining,
            },
        )?;
        let retained_bytes = content
            .retained_bytes()
            .saturating_add(image_bytes)
            .saturating_add(palette_bytes);
        Ok(Self {
            source,
            pin,
            content,
            palette: window.palette,
            images: window.images,
            top,
            retained_bytes,
        })
    }

    /// Frozen revisions intentionally survive live output. Rebinding a view,
    /// execution, or immutable pin invalidates all source endpoints instead.
    pub fn matches_source(
        &self,
        source: PaneZoomSource,
        pin: bmux_attach_pipeline::ScrollbackPin,
    ) -> bool {
        self.source == source && self.pin == pin
    }

    /// Top logical origin to preserve when changing width. Feed it to
    /// `PaneZoomHistoryOrigin::Top(...).query()` for the existing bottom-anchor
    /// history service. Near the tail that service may backfill above it.
    pub const fn top_anchor(&self) -> bmux_attach_pipeline::CapturedHistoryAnchor {
        self.top
    }

    /// Source metadata in projected local cells. Byte coordinates are relative
    /// to each visible logical-line segment; its identity includes the segment's
    /// first source column so missing history prefixes cannot impersonate byte 0.
    pub fn source_metadata(
        &self,
    ) -> Result<bmux_plugin::surface::PluginSurfaceMetadata, HistorySliceError> {
        let rows = self.content.selection_rows(budget())?;
        let mut starts = std::collections::BTreeMap::new();
        let mut ends = std::collections::BTreeMap::new();
        for row in &rows {
            starts
                .entry(row.source.start.line)
                .or_insert(row.source.start);
            ends.insert(row.source.start.line, row.source.end);
        }
        source_metadata(
            self.source,
            self.pin.stream_end,
            self.content.width(),
            &rows,
            |line, _bytes, _origin| {
                let start = starts[&line];
                self.content
                    .export_text(start, ends[&line], MAX_SOURCE_BYTES)
            },
        )
    }

    pub const fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }
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
        )?
        .with_content_layout(match self.mode {
            GridMode::Main => bmux_tui::grid_fragment::GridContentLayout::Flow,
            GridMode::Alternate => bmux_tui::grid_fragment::GridContentLayout::Positioned,
        });
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

    /// Selection, semantic descriptions and text for the exact projected source
    /// cells, before compositor transforms or occlusion. UTF-8 ranges address the
    /// captured logical line, never flattened buffer text. Stage with this view.
    pub fn source_metadata(
        &self,
    ) -> Result<bmux_plugin::surface::PluginSurfaceMetadata, HistorySliceError> {
        let rows = self.selection_rows()?;
        source_metadata(
            self.source,
            self.revision,
            self.columns,
            &rows,
            |line, bytes, _origin| {
                self.projection
                    .export_text(self.capture_id.as_u128(), line, bytes, budget())
            },
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

/// Materialize descriptions only from admitted source correspondence and source
/// text. Each logical line is a distinct content item; visible segments are
/// explicitly identified and all fragment byte ranges are rebased to that item.
fn source_metadata(
    source: PaneZoomSource,
    revision: u64,
    columns: usize,
    rows: &[ContentSelectionRow],
    mut text_for: impl FnMut(
        usize,
        std::ops::Range<usize>,
        ContentAnchor,
    ) -> Result<String, HistorySliceError>,
) -> Result<bmux_plugin::surface::PluginSurfaceMetadata, HistorySliceError> {
    use bmux_tui::{
        geometry::Rect,
        selection::{SelectionContentId, SelectionScene, SelectionScope},
        semantic::SemanticScene,
    };
    let width = u16::try_from(columns).map_err(|_| HistorySliceError::InvalidOffset)?;
    let height = u16::try_from(rows.len()).map_err(|_| HistorySliceError::InvalidOffset)?;
    let scope = format!("zoom:{}:{}", source.view, source.execution);
    let mut selection = SelectionScene::new();
    selection.push_scope(
        SelectionScope::new(scope.clone(), Rect::new(0, 0, width, height)).revision(revision),
    );
    let mut semantics = SemanticScene::new();
    let mut text = std::collections::BTreeMap::new();
    let mut ranges = std::collections::BTreeMap::new();
    for row in rows {
        let entry = ranges
            .entry(row.source.start.line)
            .or_insert_with(|| (row.bytes.clone(), row.source.start));
        entry.0.end = row.bytes.end;
    }
    let mut remaining = MAX_SOURCE_BYTES;
    let mut work = MAX_SOURCE_CELLS;
    for (line, (range, origin)) in &ranges {
        let value = text_for(*line, range.clone(), *origin)?;
        if value.len() != range.end - range.start {
            return Err(HistorySliceError::InvalidOffset);
        }
        remaining = remaining
            .checked_sub(value.len().saturating_mul(2))
            .ok_or(HistorySliceError::BudgetExhausted)?;
        let id = SelectionContentId::new(format!(
            "{scope}:{}:{line}:{}",
            origin.capture, origin.column
        ));
        text.insert(id, value);
    }
    let mut order = 0u64;
    for (y, row) in rows.iter().enumerate() {
        let (range, origin) = &ranges[&row.source.start.line];
        let id = SelectionContentId::new(format!(
            "{scope}:{}:{}:{}",
            origin.capture, origin.line, origin.column
        ));
        let value = &text[&id];
        let visible = row.bytes.start - range.start..row.bytes.end - range.start;
        let row_text = value.get(visible).ok_or(HistorySliceError::InvalidOffset)?;
        let y = u16::try_from(y).map_err(|_| HistorySliceError::InvalidOffset)?;
        semantics.push(metadata_semantic_row(&scope, y, width, row_text));
        for cell in &row.cells {
            work = work
                .checked_sub(1)
                .ok_or(HistorySliceError::BudgetExhausted)?;
            remaining = remaining
                .checked_sub(512)
                .ok_or(HistorySliceError::BudgetExhausted)?;
            selection.push_fragment(metadata_fragment(
                cell,
                range,
                value,
                (columns, y),
                (&scope, &id, revision),
                order,
            )?);
            order += 1;
        }
    }
    Ok(bmux_plugin::surface::PluginSurfaceMetadata {
        selection: Some(selection),
        semantics: Some(semantics),
        text,
    })
}

fn metadata_semantic_row(
    scope: &str,
    y: u16,
    width: u16,
    text: &str,
) -> bmux_tui::semantic::SemanticRegion {
    let mut region = bmux_tui::semantic::SemanticRegion::new(
        format!("{scope}:row:{y}"),
        bmux_tui::geometry::Rect::new(0, y, width, 1),
        "terminal-row",
    );
    region.accessibility = Some(bmux_tui::semantic::SemanticAccessibility {
        text: Some(text.to_owned()),
        ..bmux_tui::semantic::SemanticAccessibility::default()
    });
    region
}

fn metadata_fragment(
    cell: &bmux_terminal_grid::SelectionCell,
    range: &std::ops::Range<usize>,
    value: &str,
    geometry: (usize, u16),
    identity: (&str, &bmux_tui::selection::SelectionContentId, u64),
    order: u64,
) -> Result<bmux_tui::selection::SelectionFragment, HistorySliceError> {
    let bytes = cell
        .bytes
        .start
        .checked_sub(range.start)
        .ok_or(HistorySliceError::InvalidOffset)?
        ..cell
            .bytes
            .end
            .checked_sub(range.start)
            .ok_or(HistorySliceError::InvalidOffset)?;
    if value.get(bytes.clone()).is_none()
        || cell.columns.end > geometry.0
        || cell.columns.start >= cell.columns.end
    {
        return Err(HistorySliceError::InvalidOffset);
    }
    let x = u16::try_from(cell.columns.start).map_err(|_| HistorySliceError::InvalidOffset)?;
    let width = u16::try_from(cell.columns.len()).map_err(|_| HistorySliceError::InvalidOffset)?;
    Ok(bmux_tui::selection::SelectionFragment::new(
        identity.0,
        identity.1.clone(),
        bmux_tui::geometry::Rect::new(x, geometry.1, width, 1),
        order,
        bytes,
    )
    .revision(identity.2)
    .line_range(0..value.len()))
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
    fn projected_metadata_uses_exact_unicode_source_segments() {
        let terminal = stream();
        let view =
            PaneZoomProjection::capture(source(1), Uuid::from_u128(98), terminal.grid(), 3, 8)
                .unwrap();
        let metadata = view.source_metadata().unwrap();
        let selection = metadata.selection.as_ref().unwrap();
        assert_ne!(selection.fragments().len(), 0);
        let mut saw_wide = false;
        for fragment in selection.fragments() {
            let text = &metadata.text[&fragment.content_id];
            let glyph = text.get(fragment.source_range.clone()).unwrap();
            if glyph == "界" {
                saw_wide = true;
                assert_eq!(fragment.area.width, 2);
            }
            assert!(fragment.area.x + fragment.area.width <= 3);
            assert!(fragment.area.y < 8);
        }
        assert!(saw_wide);
        let semantics = metadata.semantics.unwrap();
        assert_eq!(semantics.regions().len(), view.content.rows.len());
        assert!(
            semantics.regions().iter().all(|region| region
                .accessibility
                .as_ref()
                .unwrap()
                .text
                .is_some())
        );
    }

    #[test]
    fn frozen_history_admission_and_navigation_preserve_identity() {
        let terminal = stream();
        let id = Uuid::from_u128(88);
        let capture = PaneZoomProjection::capture(source(1), id, terminal.grid(), 8, 3).unwrap();
        let pin = bmux_attach_pipeline::ScrollbackPin {
            capture: Some(bmux_attach_pipeline::ScrollbackCapture {
                identity: id,
                lines: 100,
                truncated: false,
                width: 8,
                height: 3,
            }),
            pin_id: 1,
            total_scrolled_rows: 10,
            max_scrollback_offset: 10,
            stream_end: 100,
            created_epoch_secs: 0,
        };
        let window = bmux_attach_pipeline::PaneScrollbackWindow {
            images: Vec::new(),
            projection_width: 8,
            row_anchors: capture
                .content
                .anchors
                .iter()
                .map(|anchor| bmux_attach_pipeline::CapturedHistoryAnchor {
                    capture_id: id,
                    line_index: u32::try_from(anchor.line).unwrap(),
                    column: anchor.column,
                })
                .collect(),
            palette: capture.palette,
            scrollback_offset: 0,
            max_scrollback_offset: 10,
            total_scrolled_rows: 10,
            rows: capture.content.rows,
        };
        let mut window = window;
        let original_anchors = window.row_anchors.clone();
        let mut screen_pin = pin;
        screen_pin.capture.as_mut().unwrap().lines = 0;
        for (index, anchor) in window.row_anchors.iter_mut().enumerate() {
            anchor.line_index = u32::try_from(index).unwrap();
        }
        assert!(
            PaneZoomHistoryProjection::from_borrowed_window(source(1), screen_pin, &window).is_ok()
        );
        window.row_anchors[0].line_index = 3;
        assert!(
            PaneZoomHistoryProjection::from_borrowed_window(source(1), screen_pin, &window)
                .is_err()
        );
        window.row_anchors = original_anchors;
        let frozen = PaneZoomHistoryProjection::from_window(source(1), pin, window).unwrap();
        assert!(frozen.matches_source(source(1), pin));
        assert!(!frozen.matches_source(source(2), pin));
        assert!(!frozen.matches_source(
            source(1),
            bmux_attach_pipeline::ScrollbackPin { pin_id: 2, ..pin }
        ));
        let top = frozen.top_anchor();
        assert_eq!(
            PaneZoomHistoryOrigin::Top(top).query(4, 3).unwrap(),
            (4, Some(top), -2)
        );
        assert_eq!(
            PaneZoomHistoryOrigin::BottomFollow.query(4, 3).unwrap(),
            (4, None, 0)
        );
        assert!(PaneZoomHistoryOrigin::BottomFollow.query(4, 257).is_err());
        let fragment = frozen
            .grid_fragment_with_selection(
                "frozen",
                bmux_tui::grid_fragment::GridTransform::new(
                    bmux_tui::geometry::Point::new(0, 0),
                    1500,
                )
                .unwrap(),
                vec![bmux_tui::geometry::Rect::new(0, 0, 12, 6)],
                3,
                |_| bmux_tui::style::Style::default(),
                |_| false,
            )
            .unwrap();
        assert_eq!(fragment.buffer().area().width, 8);
        assert_eq!(fragment.buffer().area().height, 3);
        assert!(fragment.cursor().is_none());
        assert_eq!(frozen.palette.styles(), terminal.grid().palette().styles());
        assert_eq!(frozen.images.len(), 0);
        let metadata = frozen.source_metadata().unwrap();
        for fragment in metadata.selection.as_ref().unwrap().fragments() {
            assert!(
                metadata.text[&fragment.content_id]
                    .get(fragment.source_range.clone())
                    .is_some()
            );
        }
        assert!(frozen.retained_bytes() > 0);
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
