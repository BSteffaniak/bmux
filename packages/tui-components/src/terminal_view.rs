//! Live terminal output: caller-owned parsed terminal state and the component that shows it.
//!
//! [`TerminalState`] holds a terminal emulator (`bmux_terminal_grid`) that output is fed into
//! as it arrives, with its scroll position and the process status. It parses incrementally and
//! keeps a bounded scrollback; nothing is re-parsed when painting. [`TerminalViewComponent`]
//! paints it in one of two [`TerminalViewMode`]s:
//!
//! * [`TerminalViewMode::Screen`] shows the terminal as the program drew it: the screen
//!   (including the alternate screen and the cursor) with history above it, at the terminal's
//!   own width. Size the terminal to the view with [`TerminalState::requested_size`].
//! * [`TerminalViewMode::Reflow`] shows the output as a log: logical lines rewrapped to the
//!   view's width, without unused rows after the output.
//!
//! The view scrolls through a viewport laid out as [`terminal_viewport_id`], and
//! [`TerminalState`] implements [`ScrollSource`], so it works with
//! [`crate::pane::PaneComponent::border_scrollbar`] and [`ScrollExtent`]-based wheel
//! handling. It follows new output while scrolled to the bottom.
//!
//! Views that are not components (for example rows of a transcript) take the same output as
//! styled lines through [`TerminalState::window`] (reflowed output, with selection rows and
//! anchors that survive reflow) and [`TerminalState::screen_window`] (a crop of the screen).
//!
//! Running the process (for example with `bmux_pty`) is the caller's concern: feed what it
//! writes with [`TerminalState::feed`] and report its end with [`TerminalState::finish`].

use std::cell::{Cell, RefCell};
use std::hash::{Hash, Hasher};
use std::ops::Range;

pub use bmux_terminal_grid::{ContentAnchor, SelectionCell};
use bmux_terminal_grid::{
    ContentBudget, ContentProjection, GridLimits, GridMode, PhysicalRow, TerminalGrid,
    TerminalGridStream,
};
use bmux_tui::component::{
    ChildLayout, Component, ComponentRevision, Constraints, LayoutCx, LayoutId, LayoutNode,
    LogicalSize,
};
use bmux_tui::geometry::Rect;
use bmux_tui::paint::PaintCx;
use bmux_tui::prelude::Line;
use bmux_tui::style::{Modifier, Style};

use crate::scroll_view::{
    ScrollExtent, ScrollSource, ScrollViewComponent, ScrollViewState, TrackScrollbar,
};
use crate::scrollbar_layout::{ScrollbarAxisLayoutMode, ScrollbarLayoutPolicy, scrollbar_layout};
pub use crate::terminal_style::terminal_grid_line;
use crate::terminal_style::terminal_grid_style;

/// Work and memory allowed for one reflow projection.
const REFLOW_BUDGET: ContentBudget = ContentBudget {
    cells: 16_000_000,
    bytes: 64 * 1024 * 1024,
};

/// Whether the program is still running, or how it ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TerminalStatus {
    /// Output may still arrive.
    Running,
    /// The program ended, with its exit code when it exited normally.
    Exited(Option<i32>),
    /// A signal ended the program.
    Signaled(i32),
    /// The program was stopped because it ran out of time.
    TimedOut,
    /// The program was stopped on request.
    Cancelled,
}

impl TerminalStatus {
    /// Whether the program exited with code zero.
    #[must_use]
    pub const fn success(self) -> bool {
        matches!(self, Self::Exited(Some(0)))
    }
}

/// How [`TerminalViewComponent`] shows the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TerminalViewMode {
    /// The screen as drawn, with history above it, at the terminal's own width.
    #[default]
    Screen,
    /// Logical lines rewrapped to the view's width, like a log.
    Reflow,
}

/// Which rows of reflowed output a [`TerminalWindow`] holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalRows {
    /// The last `n` rows. Only the output they need is captured.
    Tail(usize),
    /// At most `count` rows from row `start`.
    Range {
        /// First row.
        start: usize,
        /// Most rows.
        count: usize,
    },
    /// At most `count` rows from the row holding `anchor` (from an earlier window), so a
    /// position survives reflow at another width. The tail when the anchor no longer
    /// resolves (the output changed since).
    From {
        /// Where the window starts.
        anchor: ContentAnchor,
        /// Most rows.
        count: usize,
    },
}

/// One row's selectable text and how its display columns map to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalSelectionRow {
    /// Identity of the source line or screen row, stable while the output does not change.
    pub identity: String,
    /// Where `text` starts in the source line or row, in bytes.
    pub byte_start: usize,
    /// The row's text.
    pub text: String,
    /// Display columns of each cell, and its bytes in the source line or row.
    pub cells: Vec<SelectionCell>,
}

/// Rows of terminal output as styled lines; see [`TerminalState::window`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalWindow {
    /// The rows.
    pub lines: Vec<Line>,
    /// Index of the first row among all rows.
    pub first: usize,
    /// All rows at this width.
    pub total: usize,
    /// Where each row starts in the output (reflowed windows only).
    pub anchors: Vec<ContentAnchor>,
    /// Selection correspondence for each row, in order.
    pub selection: Vec<TerminalSelectionRow>,
    /// Older output was dropped from history.
    pub history_truncated: bool,
}

/// Captured output reflowed at one width, kept until the terminal changes.
struct Reflowed {
    revision: u64,
    /// Logical lines captured from the end; `usize::MAX` for all.
    lines: usize,
    width: usize,
    projection: ContentProjection,
    rows: usize,
}

/// A terminal fed with a program's output: what it shows, where it is scrolled to, and
/// whether the program still runs. Owned by the caller across frames.
pub struct TerminalState {
    stream: TerminalGridStream,
    status: TerminalStatus,
    bytes: u64,
    scroll: Cell<ScrollViewState>,
    requested: Cell<Option<(u16, u16)>>,
    reflowed: RefCell<Option<Reflowed>>,
    captures: Cell<u128>,
}

impl TerminalState {
    /// A running terminal of `cols` by `rows` cells keeping at most `scrollback` rows of
    /// history (zero dimensions are raised to one).
    #[must_use]
    pub fn new(cols: u16, rows: u16, scrollback: usize) -> Self {
        let stream = TerminalGridStream::new(
            cols.max(1),
            rows.max(1),
            GridLimits {
                scrollback_rows: scrollback,
            },
        )
        .unwrap_or_else(|_| unreachable!("non-zero terminal dimensions are valid"));
        let mut scroll = ScrollViewState::new();
        scroll.set_follow_bottom(true);
        Self {
            stream,
            status: TerminalStatus::Running,
            bytes: 0,
            scroll: Cell::new(scroll),
            requested: Cell::new(None),
            reflowed: RefCell::new(None),
            captures: Cell::new(0),
        }
    }

    /// Interpret output the program wrote.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.bytes = self
            .bytes
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        self.stream.process(bytes);
    }

    /// Change the terminal size; history reflows to the new width. Resize the program's
    /// terminal to the same size so it redraws for it.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let _ = self.stream.resize(cols.max(1), rows.max(1));
        self.requested.set(None);
    }

    /// Terminal size as `(cols, rows)`.
    #[must_use]
    pub fn size(&self) -> (u16, u16) {
        let grid = self.stream.grid();
        (
            u16::try_from(grid.width()).unwrap_or(u16::MAX),
            u16::try_from(grid.height()).unwrap_or(u16::MAX),
        )
    }

    /// The size a [`TerminalViewMode::Screen`] view was last painted at, when it differs from
    /// the terminal's: resize the terminal and the program to it.
    #[must_use]
    pub fn requested_size(&self) -> Option<(u16, u16)> {
        self.requested.get().filter(|size| *size != self.size())
    }

    /// Record that the program ended.
    pub const fn finish(&mut self, status: TerminalStatus) {
        self.status = status;
    }

    /// Whether the program still runs, or how it ended.
    #[must_use]
    pub const fn status(&self) -> TerminalStatus {
        self.status
    }

    /// Bytes of output fed so far.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Whether no output was fed.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.bytes == 0
    }

    /// The emulated terminal.
    #[must_use]
    pub const fn grid(&self) -> &TerminalGrid {
        self.stream.grid()
    }

    /// Plain text of the output (see [`bmux_terminal_grid::content_text`]).
    #[must_use]
    pub fn text(&self) -> String {
        bmux_terminal_grid::content_text(self.grid())
    }

    /// Follow new output from now on, scrolled to the bottom.
    pub fn follow(&self) {
        let mut scroll = self.scroll.get();
        scroll.set_follow_bottom(true);
        self.scroll.set(scroll);
    }

    /// Rows of content for a view of `width` columns and `height` rows in `mode`.
    fn content_rows(&self, mode: TerminalViewMode, width: usize, height: usize) -> usize {
        let grid = self.grid();
        match effective(mode, grid) {
            TerminalViewMode::Screen => grid
                .max_scrollback_offset()
                .saturating_add(grid.height())
                .max(height),
            TerminalViewMode::Reflow => self
                .with_reflowed(width, |reflowed| reflowed.rows)
                .unwrap_or(0),
        }
    }

    /// Run `use_rows` on the output reflowed at `width`, projecting it when the terminal or
    /// width changed since the last projection. `None` when the output exceeds the budget.
    fn with_reflowed<T>(&self, width: usize, use_rows: impl FnOnce(&Reflowed) -> T) -> Option<T> {
        self.with_projection(width, usize::MAX, use_rows)
    }

    /// [`Self::with_reflowed`] needing only the last `lines` logical lines captured.
    fn with_projection<T>(
        &self,
        width: usize,
        lines: usize,
        use_rows: impl FnOnce(&Reflowed) -> T,
    ) -> Option<T> {
        let grid = self.grid();
        let width = width.max(1);
        let mut cache = self.reflowed.borrow_mut();
        let stale = cache.as_ref().is_none_or(|cached| {
            cached.revision != grid.content_revision() || cached.lines < lines
        });
        if stale {
            let capture = self.captures.get().wrapping_add(1);
            self.captures.set(capture);
            let all = grid.height().saturating_add(grid.max_scrollback_offset());
            *cache = grid
                .capture_content(capture, lines.min(all), REFLOW_BUDGET)
                .ok()
                .map(|projection| Reflowed {
                    revision: grid.content_revision(),
                    lines,
                    width: 0,
                    projection,
                    rows: 0,
                });
        }
        let reflowed = cache.as_mut()?;
        if reflowed.width != width {
            if reflowed.projection.prepare(width, REFLOW_BUDGET).is_err() {
                *cache = None;
                return None;
            }
            reflowed.width = width;
            reflowed.rows = reflowed.projection.row_count().unwrap_or(0);
        }
        cache.as_ref().map(use_rows)
    }

    /// Reflowed output at `width` columns as styled lines: the rows `rows` asks for, with
    /// where each starts and what selecting it copies. `None` when the output exceeds the
    /// projection budget. A [`TerminalRows::Tail`] captures only the output it shows, so
    /// following a running program costs no more than the rows on view.
    #[must_use]
    pub fn window(&self, width: usize, rows: TerminalRows) -> Option<TerminalWindow> {
        let lines = match rows {
            TerminalRows::Tail(count) => count,
            TerminalRows::Range { .. } | TerminalRows::From { .. } => usize::MAX,
        };
        let grid = self.grid();
        let truncated = grid.history_truncated();
        self.with_projection(width, lines, |reflowed| {
            let projection = &reflowed.projection;
            let total = reflowed.rows;
            let range = match rows {
                TerminalRows::Tail(count) => total.saturating_sub(count)..total,
                TerminalRows::Range { start, count } => {
                    start.min(total)..start.saturating_add(count).min(total)
                }
                TerminalRows::From { anchor, count } => projection.resolve(anchor).map_or_else(
                    || total.saturating_sub(count)..total,
                    |start| start..start.saturating_add(count).min(total),
                ),
            };
            let window = projection.window(range.clone(), REFLOW_BUDGET.bytes).ok()?;
            let mut selection = Vec::new();
            for row in projection
                .selection_window(range.clone(), REFLOW_BUDGET)
                .ok()?
            {
                let start = row.source.start;
                let text = projection
                    .export_text(start.capture, start.line, row.bytes.clone(), REFLOW_BUDGET)
                    .ok()?;
                selection.push(TerminalSelectionRow {
                    identity: format!("capture:{}:{}", start.capture, start.line),
                    byte_start: row.bytes.start,
                    text,
                    cells: row.cells,
                });
            }
            Some(TerminalWindow {
                lines: window
                    .rows
                    .iter()
                    .map(|row| terminal_grid_line(grid, row))
                    .collect(),
                first: range.start,
                total,
                anchors: window.anchors,
                selection,
                history_truncated: truncated || window.history_truncated,
            })
        })
        .flatten()
    }

    /// The screen cropped to `columns` and `rows`, as the program drew it (for a full-screen
    /// program, which cannot be reflowed), with what selecting each row copies. `None` when
    /// the crop exceeds the projection budget.
    #[must_use]
    pub fn screen_window(
        &self,
        columns: Range<usize>,
        rows: Range<usize>,
    ) -> Option<TerminalWindow> {
        let grid = self.grid();
        let revision = grid.content_revision();
        let lines = grid
            .screen_window(columns.clone(), rows.clone(), REFLOW_BUDGET)
            .ok()?;
        let selection = grid
            .screen_selection(revision, columns, rows.clone(), REFLOW_BUDGET)
            .ok()?
            .into_iter()
            .map(|row| TerminalSelectionRow {
                identity: format!("screen:{revision}:{}", row.source.row),
                byte_start: row.byte_start,
                text: row.text,
                cells: row.cells,
            })
            .collect();
        Some(TerminalWindow {
            lines: lines
                .iter()
                .map(|row| terminal_grid_line(grid, row))
                .collect(),
            first: rows.start.min(grid.height()),
            total: grid.height(),
            anchors: Vec::new(),
            selection,
            history_truncated: false,
        })
    }

    /// Rows `start..start + count` of the content in `mode` at `width`.
    fn rows(
        &self,
        mode: TerminalViewMode,
        width: usize,
        start: usize,
        count: usize,
    ) -> Vec<PhysicalRow> {
        let grid = self.grid();
        match effective(mode, grid) {
            TerminalViewMode::Screen => {
                let total = grid.max_scrollback_offset().saturating_add(grid.height());
                // `display_rows` counts back from the live bottom and returns at least a screen.
                let from_bottom = total.saturating_sub(start.saturating_add(count));
                let mut rows = grid.display_rows(from_bottom, count);
                let extra = rows.len().saturating_sub(count);
                rows.drain(..extra);
                rows
            }
            TerminalViewMode::Reflow => self
                .with_reflowed(width, |reflowed| {
                    reflowed
                        .projection
                        .window(
                            start..start.saturating_add(count).min(reflowed.rows),
                            usize::MAX,
                        )
                        .map(|window| window.rows)
                        .unwrap_or_default()
                })
                .unwrap_or_default(),
        }
    }
}

impl std::fmt::Debug for TerminalState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TerminalState")
            .field("size", &self.size())
            .field("status", &self.status)
            .field("bytes", &self.bytes)
            .field("scroll", &self.scroll.get())
            .finish_non_exhaustive()
    }
}

impl ScrollSource for TerminalState {
    fn scroll_state(&self) -> ScrollViewState {
        self.scroll.get()
    }

    fn set_scroll_state(&self, state: ScrollViewState) {
        self.scroll.set(state);
    }
}

/// A full-screen program's output only makes sense as a screen.
fn effective(mode: TerminalViewMode, grid: &TerminalGrid) -> TerminalViewMode {
    if grid.mode() == GridMode::Alternate {
        TerminalViewMode::Screen
    } else {
        mode
    }
}

/// Layout identity of the scroll viewport inside the terminal view laid out as `id`.
#[must_use]
pub fn terminal_viewport_id(id: &LayoutId) -> LayoutId {
    LayoutId::new(format!("{}.viewport", id.as_str()))
}

/// Paints a [`TerminalState`]. See the [module documentation](self).
pub struct TerminalViewComponent<'a> {
    id: LayoutId,
    state: &'a TerminalState,
    mode: TerminalViewMode,
    max_rows: Option<u64>,
    cursor: bool,
    scrollbar: ScrollbarAxisLayoutMode,
}

impl<'a> TerminalViewComponent<'a> {
    /// A view of `state` in [`TerminalViewMode::Screen`].
    #[must_use]
    pub fn new(id: impl Into<LayoutId>, state: &'a TerminalState) -> Self {
        Self {
            id: id.into(),
            state,
            mode: TerminalViewMode::Screen,
            max_rows: None,
            cursor: true,
            scrollbar: ScrollbarAxisLayoutMode::Hidden,
        }
    }

    /// Draw a scrollbar inside the view (default hidden; panes can carry one on their border
    /// instead). Its track is [`Self::scrollbar_area`] of the view's area, and
    /// [`TrackScrollbar`] drives it from input.
    #[must_use]
    pub const fn scrollbar(mut self, mode: ScrollbarAxisLayoutMode) -> Self {
        self.scrollbar = mode;
        self
    }

    /// Where a view of this configuration occupying `area` draws its scrollbar, if anywhere.
    #[must_use]
    pub const fn scrollbar_area(&self, area: Rect) -> Option<Rect> {
        scrollbar_layout(area, self.scrollbar_policy()).vertical_scrollbar
    }

    const fn scrollbar_policy(&self) -> ScrollbarLayoutPolicy {
        ScrollbarLayoutPolicy::new(self.scrollbar, ScrollbarAxisLayoutMode::Hidden)
    }

    /// Choose how the terminal is shown.
    #[must_use]
    pub const fn mode(mut self, mode: TerminalViewMode) -> Self {
        self.mode = mode;
        self
    }

    /// Without a height constraint, take at most `rows` rows, showing the end of the output.
    /// (By default the view takes as many rows as the output has.)
    #[must_use]
    pub const fn max_rows(mut self, rows: u64) -> Self {
        self.max_rows = Some(rows);
        self
    }

    /// Show the program's cursor while it runs (default true).
    #[must_use]
    pub const fn cursor(mut self, cursor: bool) -> Self {
        self.cursor = cursor;
        self
    }

    fn viewport_id(&self) -> LayoutId {
        terminal_viewport_id(&self.id)
    }

    /// The current first visible row, applying follow-bottom and clamping to `extent`.
    fn offset(&self, extent: ScrollExtent) -> u64 {
        let mut scroll = self.state.scroll.get();
        let offset = if scroll.follows_bottom() {
            extent.max_offset()
        } else {
            scroll.vertical_offset().min(extent.max_offset())
        };
        if offset != scroll.vertical_offset() {
            let follow = scroll.follows_bottom();
            scroll.set_vertical_offset(offset);
            scroll.set_follow_bottom(follow);
            self.state.scroll.set(scroll);
        }
        offset
    }

    fn paint_cursor(&self, first: usize, rows: usize, cx: &mut PaintCx<'_, '_>) {
        let grid = self.state.grid();
        let cursor = grid.cursor();
        if !self.cursor || self.state.status != TerminalStatus::Running || !cursor.visible {
            return;
        }
        // The cursor sits on the screen, which ends the content.
        let screen_top = grid.max_scrollback_offset();
        let row = screen_top.saturating_add(cursor.row);
        if row < first || row >= first.saturating_add(rows) {
            return;
        }
        let y = i64::try_from(row - first).unwrap_or(i64::MAX);
        let x = i32::try_from(cursor.col).unwrap_or(i32::MAX);
        let symbol = grid
            .display_rows(0, grid.height())
            .get(cursor.row)
            .and_then(|line| line.cells().get(cursor.col))
            .map_or(" ", |cell| {
                if cell.text().is_empty() {
                    " "
                } else {
                    cell.text()
                }
            })
            .to_owned();
        cx.set_cell(x, y, &symbol, Style::new().add_modifier(Modifier::REVERSED));
    }
}

impl Component for TerminalViewComponent<'_> {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        self.mode.hash(&mut layout);
        self.max_rows.hash(&mut layout);
        self.scrollbar.hash(&mut layout);
        self.state.grid().revision().hash(&mut layout);
        let mut paint = std::collections::hash_map::DefaultHasher::new();
        self.state.scroll.get().hash(&mut paint);
        self.state.status.hash(&mut paint);
        self.cursor.hash(&mut paint);
        self.scrollbar.hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    /// The node holds the viewport ([`terminal_viewport_id`]) over a content node as tall as
    /// the output, so scrollbars and wheel handling see the real extent.
    fn layout(&self, constraints: Constraints, _cx: &mut LayoutCx) -> LayoutNode {
        let width = constraints.max_width();
        // Gutter reservation depends only on the width.
        let probe = Rect::new(0, 0, u16::try_from(width).unwrap_or(u16::MAX), u16::MAX);
        let content_width = u64::from(
            scrollbar_layout(probe, self.scrollbar_policy())
                .content
                .width,
        );
        let columns = usize::try_from(content_width).unwrap_or(usize::MAX);
        let floor = usize::try_from(constraints.min_height()).unwrap_or(usize::MAX);
        let rows = self.state.content_rows(self.mode, columns, floor);
        let content = u64::try_from(rows).unwrap_or(u64::MAX);
        let natural = self.max_rows.map_or(content, |max| content.min(max));
        let size = constraints.constrain(LogicalSize::new(width, natural));
        let viewport = ScrollViewComponent::viewport_layout(
            self.viewport_id(),
            LogicalSize::new(content_width, size.height),
            LayoutNode::leaf(
                LayoutId::new(format!("{}.content", self.id.as_str())),
                LogicalSize::new(content_width, content.max(size.height)),
            ),
        );
        LayoutNode::with_children(
            self.id.clone(),
            size,
            vec![ChildLayout::new(0, 0, viewport)],
        )
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let area = Rect::new(
            0,
            0,
            u16::try_from(layout.size.width).unwrap_or(u16::MAX),
            u16::try_from(layout.size.height).unwrap_or(u16::MAX),
        );
        let content = scrollbar_layout(area, self.scrollbar_policy()).content;
        let (width, height) = (content.width, content.height);
        if width == 0 || height == 0 {
            return;
        }
        if effective(self.mode, self.state.grid()) == TerminalViewMode::Screen {
            self.state.requested.set(Some((width, height)));
        }
        let Some(viewport) = layout.find(&self.viewport_id()) else {
            return;
        };
        let extent = ScrollExtent::of(viewport);
        let first = usize::try_from(self.offset(extent)).unwrap_or(usize::MAX);
        let rows = self
            .state
            .rows(self.mode, usize::from(width), first, usize::from(height));
        let palette = self.state.grid().palette();
        for (y, row) in rows.iter().enumerate() {
            let y = i64::try_from(y).unwrap_or(i64::MAX);
            let mut x = 0i32;
            for cell in row.cells() {
                if cell.is_wide_continuation() {
                    continue;
                }
                if x >= i32::from(width) {
                    break;
                }
                let style = terminal_grid_style(palette.get(cell.style()));
                let symbol = if cell.text().is_empty() {
                    " "
                } else {
                    cell.text()
                };
                cx.set_cell(x, y, symbol, style);
                x = x.saturating_add(i32::from(cell.width().max(1)));
            }
        }
        if effective(self.mode, self.state.grid()) == TerminalViewMode::Screen {
            self.paint_cursor(first, usize::from(height), cx);
        }
        if let Some(track) = self.scrollbar_area(area) {
            TrackScrollbar::new().paint(track, extent, &self.state.scroll.get(), cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::component::{Component, Constraints, LayoutCx};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect, Size};
    use bmux_tui::paint::PaintCx;
    use bmux_tui::style::{Color, Modifier};

    use super::{
        ScrollSource, TerminalRows, TerminalState, TerminalStatus, TerminalViewComponent,
        TerminalViewMode, terminal_viewport_id,
    };
    use crate::scroll_view::{ScrollExtent, ScrollViewState};

    fn paint(view: &TerminalViewComponent<'_>, constraints: Constraints) -> Buffer {
        let layout = view.layout(constraints, &mut LayoutCx::new());
        let size = layout.size;
        let area = Rect::new(
            0,
            0,
            u16::try_from(size.width).unwrap(),
            u16::try_from(size.height).unwrap(),
        );
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        view.paint(&layout, &mut PaintCx::new(&mut frame));
        drop(frame);
        buffer
    }

    fn rows(buffer: &Buffer) -> Vec<String> {
        (0..buffer.area().height)
            .map(|y| buffer.row_symbols(y).unwrap().trim_end().to_owned())
            .collect()
    }

    fn tight(width: u16, height: u16) -> Constraints {
        Constraints::tight(Size::new(width, height))
    }

    fn state(cols: u16, rows: u16, bytes: &[u8]) -> TerminalState {
        let mut state = TerminalState::new(cols, rows, 100);
        state.feed(bytes);
        state
    }

    #[test]
    fn screen_shows_the_terminal_with_colors_and_cursor() {
        let state = state(10, 3, b"hello\r\n\x1b[31mred\x1b[0m");
        let buffer = paint(&TerminalViewComponent::new("t", &state), tight(10, 3));
        assert_eq!(rows(&buffer), ["hello", "red", ""]);
        let red = buffer.get(Point::new(0, 1)).unwrap();
        assert_eq!(red.style.fg, Some(Color::Red));
        // The running program's cursor, after "red".
        let cursor = buffer.get(Point::new(3, 1)).unwrap();
        assert!(cursor.style.modifiers.contains(Modifier::REVERSED));
        let mut finished = state;
        finished.finish(TerminalStatus::Exited(Some(0)));
        let buffer = paint(&TerminalViewComponent::new("t", &finished), tight(10, 3));
        let cell = buffer.get(Point::new(3, 1)).unwrap();
        assert!(!cell.style.modifiers.contains(Modifier::REVERSED));
    }

    #[test]
    fn screen_scrolls_through_history_and_follows_new_output() {
        let bytes: Vec<u8> = (1..=10)
            .flat_map(|n| format!("line {n}\r\n").into_bytes())
            .collect();
        let mut state = state(10, 3, &bytes);
        let view = || TerminalViewComponent::new("t", &state);
        // Following the bottom: the last screen.
        assert_eq!(
            rows(&paint(&view(), tight(10, 3))),
            ["line 9", "line 10", ""]
        );
        let layout = view().layout(tight(10, 3), &mut LayoutCx::new());
        let extent = ScrollExtent::of(layout.find(&terminal_viewport_id(&"t".into())).unwrap());
        // Ten lines, then the empty line the cursor moved to.
        assert_eq!(extent.content, 11);
        let mut scroll = ScrollViewState::new();
        scroll.set_vertical_offset(0);
        state.set_scroll_state(scroll);
        assert_eq!(
            rows(&paint(&view(), tight(10, 3))),
            ["line 1", "line 2", "line 3"]
        );
        // Scrolled up, new output does not move the view.
        state.feed(b"line 11\r\n");
        assert_eq!(
            rows(&paint(
                &TerminalViewComponent::new("t", &state),
                tight(10, 3)
            ))[0],
            "line 1"
        );
        state.follow();
        let shown = rows(&paint(
            &TerminalViewComponent::new("t", &state),
            tight(10, 3),
        ));
        assert_eq!(shown, ["line 10", "line 11", ""]);
    }

    #[test]
    fn reflow_rewraps_to_the_view_and_shows_the_tail() {
        let state = state(20, 4, b"abcdefghijkl\r\nend\r\n");
        let view = TerminalViewComponent::new("t", &state).mode(TerminalViewMode::Reflow);
        // Unbounded height: as tall as the output, without the terminal's unused rows.
        let buffer = paint(&view, Constraints::for_width(5));
        assert_eq!(rows(&buffer), ["abcde", "fghij", "kl", "end"]);
        let tail = TerminalViewComponent::new("t", &state)
            .mode(TerminalViewMode::Reflow)
            .max_rows(2);
        assert_eq!(
            rows(&paint(&tail, Constraints::for_width(5))),
            ["kl", "end"]
        );
        // A full-screen program is shown as its screen.
        let full = super::TerminalState::new(6, 2, 10);
        let mut full = full;
        full.feed(b"log\r\n\x1b[?1049h\x1b[Hscreen");
        let view = TerminalViewComponent::new("t", &full).mode(TerminalViewMode::Reflow);
        assert_eq!(rows(&paint(&view, tight(6, 2))), ["screen", ""]);
    }

    #[test]
    fn screen_requests_the_size_it_is_shown_at() {
        let mut state = state(10, 3, b"");
        paint(&TerminalViewComponent::new("t", &state), tight(30, 5));
        assert_eq!(state.requested_size(), Some((30, 5)));
        state.resize(30, 5);
        assert_eq!(state.requested_size(), None);
        assert_eq!(state.size(), (30, 5));
        // A reflowed view fits any terminal size.
        paint(
            &TerminalViewComponent::new("t", &state).mode(TerminalViewMode::Reflow),
            tight(12, 4),
        );
        assert_eq!(state.requested_size(), None);
    }

    #[test]
    fn wide_characters_keep_their_columns() {
        let state = state(10, 2, "日本x\r\nab".as_bytes());
        let buffer = paint(
            &TerminalViewComponent::new("t", &state).cursor(false),
            tight(10, 2),
        );
        assert_eq!(buffer.get(Point::new(0, 0)).unwrap().symbol, "日");
        assert_eq!(buffer.get(Point::new(2, 0)).unwrap().symbol, "本");
        assert_eq!(buffer.get(Point::new(4, 0)).unwrap().symbol, "x");
        // Narrower than the terminal: clipped, not wrapped.
        let narrow = paint(&TerminalViewComponent::new("t", &state), tight(3, 2));
        assert_eq!(rows(&narrow)[1], "ab");
    }

    #[test]
    fn text_and_status_describe_the_output() {
        let mut state = state(20, 3, b"10%\r100%\r\n");
        assert_eq!(state.text(), "100%");
        assert_eq!(state.bytes(), 10);
        assert!(!state.is_empty());
        assert_eq!(state.status(), TerminalStatus::Running);
        state.finish(TerminalStatus::Exited(Some(0)));
        assert!(state.status().success());
        assert!(!TerminalStatus::Exited(None).success());
    }

    #[test]
    fn an_inner_scrollbar_appears_on_overflow() {
        use crate::scrollbar_layout::ScrollbarAxisLayoutMode;

        let bytes: Vec<u8> = (1..=9)
            .flat_map(|n| format!("{n}\r\n").into_bytes())
            .collect();
        let long = state(6, 3, &bytes);
        let view = TerminalViewComponent::new("t", &long)
            .cursor(false)
            .scrollbar(ScrollbarAxisLayoutMode::OverlayOnOverflow);
        assert_eq!(
            view.scrollbar_area(Rect::new(0, 0, 6, 3)),
            Some(Rect::new(5, 0, 1, 3))
        );
        let buffer = paint(&view, tight(6, 3));
        assert_eq!(buffer.get(Point::new(5, 2)).unwrap().symbol, "█");
        let short = state(6, 3, b"one");
        let view = TerminalViewComponent::new("t", &short)
            .cursor(false)
            .scrollbar(ScrollbarAxisLayoutMode::OverlayOnOverflow);
        assert_eq!(rows(&paint(&view, tight(6, 3))), ["one", "", ""]);
        // A gutter narrows what the terminal is asked to be.
        let gutter =
            TerminalViewComponent::new("t", &short).scrollbar(ScrollbarAxisLayoutMode::Gutter);
        paint(&gutter, tight(6, 3));
        assert_eq!(short.requested_size(), Some((5, 3)));
    }

    #[cfg(feature = "pane")]
    #[test]
    fn panes_show_a_border_scrollbar_for_it() {
        use std::cell::Cell;

        use crate::pane::{Pane, PaneComponent, PaneState};

        let bytes: Vec<u8> = (1..=20)
            .flat_map(|n| format!("{n}\r\n").into_bytes())
            .collect();
        let state = state(8, 3, &bytes);
        let pane_state = Cell::new(PaneState::new(Rect::new(0, 0, 10, 5)));
        let pane = PaneComponent::new(
            "pane",
            Pane::new(),
            &pane_state,
            TerminalViewComponent::new("t", &state),
        )
        .border_scrollbar(terminal_viewport_id(&"t".into()), &state);
        let layout = pane.layout(tight(10, 5), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 5));
        let mut frame = Frame::new(&mut buffer);
        pane.paint(&layout, &mut PaintCx::new(&mut frame));
        assert!(
            frame
                .hits()
                .regions()
                .iter()
                .any(|hit| hit.id.as_str() == "pane.scrollbar")
        );
        drop(frame);
        // Following the bottom: the thumb is at the end of the track.
        assert_eq!(buffer.get(Point::new(9, 3)).unwrap().symbol, "┃");
    }

    fn text(line: &bmux_tui::prelude::Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_str())
            .collect()
    }

    #[test]
    fn windows_reflow_output_as_styled_lines_with_selection() {
        let state = state(20, 3, b"one\r\n\x1b[32mabcdefghij\x1b[0m\r\nthree\r\n");
        let window = state.window(5, TerminalRows::Tail(10)).unwrap();
        let lines: Vec<String> = window.lines.iter().map(text).collect();
        // "abcdefghij" wraps at five columns; no row follows the output.
        assert_eq!(lines, ["one", "abcde", "fghij", "three"]);
        assert_eq!((window.first, window.total), (0, 4));
        assert_eq!(window.lines[1].spans[0].style.fg, Some(Color::Green));
        // Selection follows the source line across the wrap.
        assert_eq!(window.selection.len(), 4);
        assert_eq!(window.selection[1].text, "abcde");
        assert_eq!(window.selection[2].text, "fghij");
        assert_eq!(window.selection[2].byte_start, 5);
        assert_eq!(window.selection[1].identity, window.selection[2].identity);
        assert_eq!(window.anchors.len(), 4);
    }

    #[test]
    fn tail_windows_show_the_end_and_anchors_survive_reflow() {
        let bytes: Vec<u8> = (1..=30)
            .flat_map(|n| format!("line number {n}\r\n").into_bytes())
            .collect();
        let state = state(40, 5, &bytes);
        let tail = state.window(40, TerminalRows::Tail(2)).unwrap();
        let lines: Vec<String> = tail.lines.iter().map(text).collect();
        assert_eq!(lines, ["line number 29", "line number 30"]);

        let all = state
            .window(
                40,
                TerminalRows::Range {
                    start: 10,
                    count: 2,
                },
            )
            .unwrap();
        assert_eq!(text(&all.lines[0]), "line number 11");
        assert_eq!(all.total, 30);
        // The same source position at a narrower width, where every line wraps.
        let anchor = all.anchors[0];
        let narrow = state
            .window(8, TerminalRows::From { anchor, count: 2 })
            .unwrap();
        assert_eq!(text(&narrow.lines[0]), "line num");
        assert_eq!(text(&narrow.lines[1]), "ber 11");
        assert_eq!(narrow.total, 60);
    }

    #[test]
    fn stale_anchors_fall_back_to_the_tail() {
        let mut state = state(20, 3, b"a\r\nb\r\n");
        let anchor = state
            .window(20, TerminalRows::Range { start: 0, count: 1 })
            .unwrap()
            .anchors[0];
        state.feed(b"c\r\n");
        let window = state
            .window(20, TerminalRows::From { anchor, count: 1 })
            .unwrap();
        assert_eq!(text(&window.lines[0]), "c");
    }

    #[test]
    fn screen_windows_crop_full_screen_programs() {
        let state = state(10, 3, b"\x1b[?1049h\x1b[Habcdefghij\r\nsecond");
        let window = state.screen_window(2..6, 0..2).unwrap();
        let lines: Vec<String> = window.lines.iter().map(text).collect();
        assert_eq!(lines, ["cdef", "cond"]);
        assert_eq!(window.selection[0].text, "cdef");
        assert_eq!(window.total, 3);
    }

    #[test]
    fn statuses_say_how_the_program_ended() {
        assert!(TerminalStatus::Exited(Some(0)).success());
        for status in [
            TerminalStatus::Exited(Some(1)),
            TerminalStatus::Signaled(9),
            TerminalStatus::TimedOut,
            TerminalStatus::Cancelled,
            TerminalStatus::Running,
        ] {
            assert!(!status.success(), "{status:?}");
        }
    }
}
