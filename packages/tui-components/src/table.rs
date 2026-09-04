//! Generic table display and selectable table component.

use std::cell::RefCell;
use std::hash::{Hash, Hasher};

use bmux_keyboard::KeyCode;
use bmux_tui::component::{
    ChildLayout, Component, ComponentRevision, Constraints, EventCx, LayoutCx, LayoutId,
    LayoutMetadata, LayoutNode, LogicalSize,
};
use bmux_tui::event::{Event, EventOutcome, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::hit::{HitRegion as SceneRegion, HitRole};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span};
use bmux_tui::semantic::SemanticRegion;
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui::text::{line_viewport, truncate_line_to_display_width};
use bmux_tui::text_width::display_width;

use crate::common::{ComponentMousePolicy, InteractionState, local_rect, u16_saturating};
use crate::hit_test::{HitRegion, hit_region_at};
use crate::scroll_view::{
    ScrollView, ScrollViewComponent, ScrollViewOutcome, ScrollViewPolicy, ScrollViewState,
};
use crate::scrollbar::{ScrollbarPolicy, ScrollbarStyles};
use crate::scrollbar_layout::ScrollbarAxisLayoutMode;

/// Horizontal cell alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TableAlign {
    /// Left align cell text.
    #[default]
    Left,
    /// Center cell text.
    Center,
    /// Right align cell text.
    Right,
}

/// Column width allocation policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableWidth {
    /// Fixed width in cells.
    Fixed(u16),
    /// Minimum content width, then participates in flexible allocation.
    Min(u16),
    /// Maximum content width cap.
    Max(u16),
    /// Percentage of available table width.
    Percentage(u16),
    /// Ratio of available table width.
    Ratio(u16, u16),
    /// Weighted flexible width.
    Flex(u16),
}

/// Sort indicator direction for caller-owned table sorting state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableSortDirection {
    /// Column is sorted ascending.
    Ascending,
    /// Column is sorted descending.
    Descending,
}

/// One table column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableColumn<'a> {
    /// Column title.
    pub title: &'a str,
    /// Column width policy.
    pub width: TableWidth,
    /// Cell alignment.
    pub align: TableAlign,
    /// Whether cell content truncates to column width.
    pub truncate: bool,
    /// Optional caller-provided sort indicator state.
    pub sort: Option<TableSortDirection>,
}

impl<'a> TableColumn<'a> {
    /// Create a flexible left-aligned column.
    #[must_use]
    pub const fn new(title: &'a str) -> Self {
        Self {
            title,
            width: TableWidth::Flex(1),
            align: TableAlign::Left,
            truncate: true,
            sort: None,
        }
    }

    /// Return this column with fixed width.
    #[must_use]
    pub const fn fixed(mut self, width: u16) -> Self {
        self.width = TableWidth::Fixed(width);
        self
    }

    /// Return this column with minimum width.
    #[must_use]
    pub const fn min(mut self, width: u16) -> Self {
        self.width = TableWidth::Min(width);
        self
    }

    /// Return this column with maximum width.
    #[must_use]
    pub const fn max(mut self, width: u16) -> Self {
        self.width = TableWidth::Max(width);
        self
    }

    /// Return this column with percentage width.
    #[must_use]
    pub const fn percentage(mut self, percent: u16) -> Self {
        self.width = TableWidth::Percentage(percent);
        self
    }

    /// Return this column with ratio width.
    #[must_use]
    pub const fn ratio(mut self, numerator: u16, denominator: u16) -> Self {
        self.width = TableWidth::Ratio(numerator, denominator);
        self
    }

    /// Return this column with flex weight.
    #[must_use]
    pub const fn flex(mut self, weight: u16) -> Self {
        self.width = TableWidth::Flex(weight);
        self
    }

    /// Return this column with alignment.
    #[must_use]
    pub const fn align(mut self, align: TableAlign) -> Self {
        self.align = align;
        self
    }
    /// Return this column with sort indicator state.
    #[must_use]
    pub const fn sort(mut self, sort: Option<TableSortDirection>) -> Self {
        self.sort = sort;
        self
    }
}

/// One table row with caller-owned cell content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRow {
    /// Row cells.
    pub cells: Vec<Vec<Line>>,
    /// Whether this row cannot be selected.
    pub disabled: bool,
}

impl TableRow {
    /// Create a row from borrowed cell labels.
    #[must_use]
    pub fn new<'a>(cells: impl Into<Vec<&'a str>>) -> Self {
        Self {
            cells: cells
                .into()
                .into_iter()
                .map(|cell| vec![Line::from(cell)])
                .collect(),
            disabled: false,
        }
    }

    /// Create a row from rich single-line cell content.
    #[must_use]
    pub fn rich(cells: impl Into<Vec<Line>>) -> Self {
        Self {
            cells: cells.into().into_iter().map(|cell| vec![cell]).collect(),
            disabled: false,
        }
    }

    /// Create a row from rich multiline cell content.
    #[must_use]
    pub fn multiline(cells: impl Into<Vec<Vec<Line>>>) -> Self {
        Self {
            cells: cells.into(),
            disabled: false,
        }
    }

    /// Return row height in rendered lines.
    #[must_use]
    pub fn height(&self) -> usize {
        self.cells.iter().map(Vec::len).max().unwrap_or(1).max(1)
    }

    /// Return plain text for one cell, joining multiline content with newlines.
    #[must_use]
    pub fn cell_plain_text(&self, index: usize) -> String {
        self.cells
            .get(index)
            .map(|cell| {
                cell.iter()
                    .map(Line::plain_text)
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }

    /// Return this row with disabled state set.
    #[must_use]
    pub const fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

/// Runtime selectable table state.
///
/// Scroll offsets are owned by the shared [`ScrollViewState`] so the table
/// scrolls through the same controller as every other viewport. Vertical
/// offsets are logical body rows; horizontal offsets are content cells past
/// the sticky columns.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TableState {
    selected: Option<usize>,
    selected_column: Option<usize>,
    hovered: Option<usize>,
    /// Shared body viewport scroll state.
    pub scroll: ScrollViewState,
    /// Generic interaction flags.
    pub interaction: InteractionState,
}

impl TableState {
    /// Create table state with a selected row.
    #[must_use]
    pub const fn new(selected: Option<usize>) -> Self {
        Self {
            selected,
            selected_column: None,
            hovered: None,
            scroll: ScrollViewState::new(),
            interaction: InteractionState::new(),
        }
    }

    /// Set whether this composite currently owns keyboard focus.
    pub const fn set_focused(&mut self, focused: bool) {
        self.interaction.focused = focused;
    }

    /// Return selected source row.
    #[must_use]
    pub const fn selected(&self) -> Option<usize> {
        self.selected
    }

    /// Set selected source row.
    pub const fn set_selected(&mut self, selected: Option<usize>) {
        self.selected = selected;
    }

    /// Return selected source column.
    #[must_use]
    pub const fn selected_column(&self) -> Option<usize> {
        self.selected_column
    }

    /// Set selected source column.
    pub const fn set_selected_column(&mut self, selected_column: Option<usize>) {
        self.selected_column = selected_column;
    }

    /// Return horizontal scroll offset in content cells.
    #[must_use]
    pub const fn horizontal_scroll(&self) -> usize {
        self.scroll.horizontal_offset()
    }

    /// Set horizontal scroll offset in content cells.
    pub const fn set_horizontal_scroll(&mut self, horizontal_scroll: usize) {
        self.scroll.set_horizontal_offset(horizontal_scroll);
    }

    /// Return the logical body-row scroll offset.
    #[must_use]
    pub const fn scroll(&self) -> usize {
        self.scroll.vertical_offset()
    }

    /// Set the logical body-row scroll offset.
    pub const fn set_scroll(&mut self, scroll: usize) {
        self.scroll.set_vertical_offset(scroll);
    }
}

/// Table behavior policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct TablePolicy {
    /// Render header row.
    pub header: bool,
    /// Render separator row after header.
    pub header_separator: bool,
    /// Keyboard row navigation enabled.
    pub keyboard: bool,
    /// Mouse behavior.
    pub mouse: ComponentMousePolicy,
    /// Keep selected row visible while rendering.
    pub auto_scroll_selected: bool,
    /// Separator between cells.
    pub cell_separator: &'static str,
    /// Number of left columns kept visible while horizontally scrolling.
    pub sticky_left_columns: usize,
    /// Ascending sort indicator symbol.
    pub sort_ascending_symbol: &'static str,
    /// Descending sort indicator symbol.
    pub sort_descending_symbol: &'static str,
    /// Integrated vertical scrollbar mode.
    pub vertical_scrollbar: ScrollbarAxisLayoutMode,
    /// Integrated vertical scrollbar policy.
    pub vertical_scrollbar_policy: ScrollbarPolicy,
    /// Integrated horizontal scrollbar mode.
    pub horizontal_scrollbar: ScrollbarAxisLayoutMode,
    /// Integrated horizontal scrollbar policy.
    pub horizontal_scrollbar_policy: ScrollbarPolicy,
}

impl TablePolicy {
    /// Render-only table.
    #[must_use]
    pub const fn bare() -> Self {
        Self {
            header: true,
            header_separator: false,
            keyboard: false,
            mouse: ComponentMousePolicy::disabled(),
            auto_scroll_selected: false,
            cell_separator: " ",
            sticky_left_columns: 0,
            sort_ascending_symbol: "↑",
            sort_descending_symbol: "↓",
            vertical_scrollbar: ScrollbarAxisLayoutMode::Hidden,
            vertical_scrollbar_policy: ScrollbarPolicy::vertical(),
            horizontal_scrollbar: ScrollbarAxisLayoutMode::Hidden,
            horizontal_scrollbar_policy: ScrollbarPolicy::horizontal(),
        }
    }

    /// Interactive selectable table.
    #[must_use]
    pub const fn interactive() -> Self {
        Self {
            header: true,
            header_separator: false,
            keyboard: true,
            mouse: ComponentMousePolicy::button(),
            auto_scroll_selected: true,
            cell_separator: " ",
            sticky_left_columns: 0,
            sort_ascending_symbol: "↑",
            sort_descending_symbol: "↓",
            vertical_scrollbar: ScrollbarAxisLayoutMode::Hidden,
            vertical_scrollbar_policy: ScrollbarPolicy::vertical(),
            horizontal_scrollbar: ScrollbarAxisLayoutMode::Hidden,
            horizontal_scrollbar_policy: ScrollbarPolicy::horizontal(),
        }
    }
    /// Return policy with sticky left column count set.
    #[must_use]
    pub const fn sticky_left_columns(mut self, columns: usize) -> Self {
        self.sticky_left_columns = columns;
        self
    }

    /// Return policy with sort indicator symbols set.
    #[must_use]
    pub const fn sort_symbols(mut self, ascending: &'static str, descending: &'static str) -> Self {
        self.sort_ascending_symbol = ascending;
        self.sort_descending_symbol = descending;
        self
    }

    /// Return policy with vertical scrollbar mode set.
    #[must_use]
    pub const fn vertical_scrollbar(mut self, mode: ScrollbarAxisLayoutMode) -> Self {
        self.vertical_scrollbar = mode;
        self
    }

    /// Return policy with vertical scrollbar rendering policy set.
    #[must_use]
    pub const fn vertical_scrollbar_policy(mut self, policy: ScrollbarPolicy) -> Self {
        self.vertical_scrollbar_policy = policy;
        self
    }

    /// Return policy with horizontal scrollbar mode set.
    #[must_use]
    pub const fn horizontal_scrollbar(mut self, mode: ScrollbarAxisLayoutMode) -> Self {
        self.horizontal_scrollbar = mode;
        self
    }

    /// Return policy with horizontal scrollbar rendering policy set.
    #[must_use]
    pub const fn horizontal_scrollbar_policy(mut self, policy: ScrollbarPolicy) -> Self {
        self.horizontal_scrollbar_policy = policy;
        self
    }

    /// Shared scroll-view policy derived from this table policy.
    ///
    /// Row and column navigation keys are owned by the table, so the shared
    /// controller handles wheel and scrollbar input only.
    #[must_use]
    pub const fn scroll_view_policy(self) -> ScrollViewPolicy {
        ScrollViewPolicy {
            keyboard: false,
            mouse_wheel: self.mouse.enabled,
            vertical_scrollbar: self.vertical_scrollbar,
            horizontal_scrollbar: self.horizontal_scrollbar,
            wheel_rows: 1,
        }
    }
}

impl Default for TablePolicy {
    fn default() -> Self {
        Self::interactive()
    }
}

/// Visual styles for [`Table`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableStyles {
    /// Header row style.
    pub header: Style,
    /// Normal row style.
    pub row: Style,
    /// Selected row style.
    pub selected: Style,
    /// Selected column style.
    pub selected_column: Style,
    /// Selected cell style.
    pub selected_cell: Style,
    /// Hovered row style.
    pub hovered: Style,
    /// Disabled row style.
    pub disabled: Style,
    /// Separator style.
    pub separator: Style,
    /// Empty table style.
    pub empty: Style,
    /// Integrated scrollbar styles.
    pub scrollbar: ScrollbarStyles,
}

impl Default for TableStyles {
    fn default() -> Self {
        Self {
            header: Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            row: Style::new().fg(Color::White),
            selected: Style::new().fg(Color::Black).bg(Color::Cyan),
            selected_column: Style::new().bg(Color::Blue),
            selected_cell: Style::new().fg(Color::Black).bg(Color::Yellow),
            hovered: Style::new().fg(Color::BrightWhite),
            disabled: Style::new().fg(Color::BrightBlack),
            separator: Style::new().fg(Color::BrightBlack),
            empty: Style::new().fg(Color::BrightBlack),
            scrollbar: ScrollbarStyles::default(),
        }
    }
}

/// Table hit-test target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableHit {
    /// Header cell was hit.
    Header { column: usize },
    /// Body cell was hit.
    Cell { row: usize, column: usize },
    /// Body row outside any visible cell was hit.
    Row { row: usize },
}

/// Table input outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableOutcome {
    /// Event was ignored.
    Ignored,
    /// Visual state changed.
    Redraw,
    /// Row focus changed.
    Focused(usize),
    /// Row was selected.
    Selected(usize),
}

/// Table layout details.
///
/// All rectangles are expressed in the coordinate space of the area the table
/// was laid out for. The body is the shared scroll viewport after reserving
/// header rows and integrated scrollbar gutters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableLayout {
    /// Resolved column widths.
    pub column_widths: Vec<u16>,
    /// Header area if visible.
    pub header: Option<Rect>,
    /// Header separator area if visible.
    pub header_separator: Option<Rect>,
    /// Body area.
    pub body: Rect,
    /// Vertical scrollbar area if enabled.
    pub vertical_scrollbar: Option<Rect>,
    /// Horizontal scrollbar area if enabled.
    pub horizontal_scrollbar: Option<Rect>,
    /// Corner cell reserved when both gutter scrollbars are enabled.
    pub scrollbar_corner: Option<Rect>,
    /// Authoritative shared scroll viewport over the stacked body rows.
    ///
    /// The viewport's single child is the measured body content whose height
    /// is the exact sum of row heights and whose width is the scrollable
    /// content width past the sticky columns.
    pub viewport: LayoutNode,
}

/// Generic table component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table<'a> {
    columns: &'a [TableColumn<'a>],
    rows: &'a [TableRow],
    policy: TablePolicy,
    styles: TableStyles,
    empty: &'a str,
}

impl<'a> Table<'a> {
    /// Create a table over caller-owned columns and rows.
    #[must_use]
    pub const fn new(columns: &'a [TableColumn<'a>], rows: &'a [TableRow]) -> Self {
        Self {
            columns,
            rows,
            policy: TablePolicy {
                header: true,
                header_separator: false,
                keyboard: true,
                mouse: ComponentMousePolicy {
                    enabled: true,
                    hover: true,
                    click: true,
                },
                auto_scroll_selected: true,
                cell_separator: " ",
                sticky_left_columns: 0,
                sort_ascending_symbol: "↑",
                sort_descending_symbol: "↓",
                vertical_scrollbar: ScrollbarAxisLayoutMode::Hidden,
                vertical_scrollbar_policy: ScrollbarPolicy::vertical(),
                horizontal_scrollbar: ScrollbarAxisLayoutMode::Hidden,
                horizontal_scrollbar_policy: ScrollbarPolicy::horizontal(),
            },
            styles: TableStyles {
                header: Style::new(),
                row: Style::new(),
                selected: Style::new(),
                selected_column: Style::new(),
                selected_cell: Style::new(),
                hovered: Style::new(),
                disabled: Style::new(),
                separator: Style::new(),
                empty: Style::new(),
                scrollbar: ScrollbarStyles::new(),
            },
            empty: "No rows",
        }
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: TablePolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: TableStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Set empty message.
    #[must_use]
    pub const fn empty(mut self, empty: &'a str) -> Self {
        self.empty = empty;
        self
    }

    /// Shared scroll controller configured from this table's policy and styles.
    #[must_use]
    pub const fn scroll_view(&self) -> ScrollView {
        ScrollView::new()
            .policy(self.policy.scroll_view_policy())
            .scrollbar_styles(self.styles.scrollbar)
            .scrollbar_policies(
                self.policy.vertical_scrollbar_policy,
                self.policy.horizontal_scrollbar_policy,
            )
    }

    /// Compute table layout for one area.
    #[must_use]
    pub fn layout(&self, area: Rect) -> TableLayout {
        self.layout_with_id(&LayoutId::new("table"), area)
    }

    /// Compute table layout for one area with a stable viewport identity.
    #[must_use]
    pub fn layout_with_id(&self, id: &LayoutId, area: Rect) -> TableLayout {
        let header_rows = u16::from(self.policy.header && area.height > 0);
        let separator_rows = u16::from(
            self.policy.header && self.policy.header_separator && area.height > header_rows,
        );
        let header = (header_rows > 0).then_some(Rect::new(area.x, area.y, area.width, 1));
        let header_separator = (separator_rows > 0).then_some(Rect::new(
            area.x,
            area.y.saturating_add(header_rows),
            area.width,
            1,
        ));
        let body_y = area
            .y
            .saturating_add(header_rows)
            .saturating_add(separator_rows);
        let scroll_area = Rect::new(
            area.x,
            body_y,
            area.width,
            area.height
                .saturating_sub(header_rows)
                .saturating_sub(separator_rows),
        );
        let scroll_view = self.scroll_view();
        let body = scroll_view.content_area(scroll_area);
        let column_widths =
            resolve_column_widths(self.columns, body.width, self.policy.cell_separator);
        let viewport = ScrollViewComponent::viewport_layout(
            viewport_id(id),
            LogicalSize::new(body.width, usize::from(body.height)),
            LayoutNode::leaf(
                content_id(id),
                LogicalSize::new(
                    self.horizontal_content_width(&column_widths, body.width),
                    self.body_height(),
                ),
            ),
        );
        TableLayout {
            column_widths,
            header,
            header_separator,
            body,
            vertical_scrollbar: scroll_view.scrollbar_area(scroll_area),
            horizontal_scrollbar: scroll_view.horizontal_scrollbar_area(scroll_area),
            scrollbar_corner: scroll_view.scrollbar_corner(scroll_area),
            viewport,
        }
    }

    /// Natural content size: the preferred column width plus the header and
    /// every row at their exact heights, before viewport constraints apply.
    #[must_use]
    pub fn size(&self) -> (u16, u16) {
        let header_rows = usize::from(self.policy.header);
        let separator_rows = usize::from(self.policy.header && self.policy.header_separator);
        let gutter = u16::from(matches!(
            self.policy.vertical_scrollbar,
            ScrollbarAxisLayoutMode::Gutter
        ));
        let horizontal_gutter = usize::from(matches!(
            self.policy.horizontal_scrollbar,
            ScrollbarAxisLayoutMode::Gutter
        ));
        (
            u16_saturating(self.preferred_content_width()).saturating_add(gutter),
            u16_saturating(
                header_rows
                    .saturating_add(separator_rows)
                    .saturating_add(self.body_height())
                    .saturating_add(horizontal_gutter),
            ),
        )
    }

    /// Clamp caller-owned scroll state to valid bounds for this area, applying
    /// selected-row auto-scroll when the policy requests it.
    pub fn reconcile(&self, area: Rect, state: &mut TableState) {
        let layout = self.layout(area);
        self.reconcile_layout(&layout, state);
    }

    fn reconcile_layout(&self, layout: &TableLayout, state: &mut TableState) {
        let scroll_view = self.scroll_view();
        if self.policy.auto_scroll_selected
            && let Some(selected) = state.selected
            && let Some(start) = self.row_start(selected)
        {
            let height = self.rows.get(selected).map_or(0, TableRow::height);
            let _ = scroll_view.ensure_visible(&layout.viewport, &mut state.scroll, start, height);
        }
        scroll_view.reconcile(&layout.viewport, &mut state.scroll);
    }

    /// Paint the header, visible rows, integrated scrollbars, and corner
    /// through a scoped local-coordinate context whose origin is this
    /// table's top-left corner.
    pub fn paint(&self, area: Rect, state: &TableState, cx: &mut PaintCx<'_, '_>) {
        if area.is_empty() {
            return;
        }
        let layout = self.layout(area);
        self.paint_layout(&layout, area, &self.reconciled(&layout, state), cx);
    }

    /// Paint against an already resolved layout.
    ///
    /// The supplied `state` must already be reconciled against `layout`; the
    /// component lifecycle does this before painting so the painted offset and
    /// the offset used for interaction registration agree exactly.
    fn paint_layout(
        &self,
        layout: &TableLayout,
        area: Rect,
        state: &TableState,
        cx: &mut PaintCx<'_, '_>,
    ) {
        if let Some(header) = layout.header {
            let line = self.row_line(
                &layout.column_widths,
                self.columns
                    .iter()
                    .map(|column| self.header_cell_line(column)),
                true,
                None,
                state,
            );
            cx.write_line_with_fallback_style(
                LocalRect::terminal(header),
                &self.visible_line(&line, &layout.column_widths, state, header.width),
                self.styles.header,
            );
        }
        if let Some(separator) = layout.header_separator {
            let line = self.separator_line(&layout.column_widths);
            cx.write_line_with_fallback_style(
                LocalRect::terminal(separator),
                &self.visible_line(&line, &layout.column_widths, state, separator.width),
                self.styles.separator,
            );
        }
        if self.rows.is_empty() {
            cx.write_line_with_fallback_style(
                LocalRect::terminal(layout.body),
                &Line::from(self.empty),
                self.styles.empty,
            );
        } else {
            let body = layout.body;
            cx.with_child(
                i32::from(body.x),
                i64::from(body.y),
                LocalRect::new(0, 0, body.width, body.height),
                |cx| {
                    ScrollViewComponent::new(
                        layout.viewport.id.clone(),
                        layout.viewport.size,
                        // Horizontal projection is applied per line by
                        // `visible_line` so sticky columns stay pinned; the
                        // viewport translates rows only.
                        vertical_only(state.scroll),
                        BodyRows {
                            table: self,
                            layout,
                            state,
                        },
                    )
                    .paint(&layout.viewport, cx);
                },
            );
        }
        let scroll_area = local_rect(area, scroll_area_of(layout, area));
        cx.with_child(
            i32::from(scroll_area.x),
            i64::from(scroll_area.y),
            LocalRect::new(0, 0, scroll_area.width, scroll_area.height),
            |cx| {
                self.scroll_view().paint_scrollbars(
                    Rect::new(0, 0, scroll_area.width, scroll_area.height),
                    &layout.viewport,
                    &state.scroll,
                    cx,
                );
            },
        );
    }

    /// Hit-test a point against visible table regions.
    #[must_use]
    pub fn hit_test(&self, area: Rect, state: &TableState, position: Point) -> Option<TableHit> {
        let layout = self.layout(area);
        let state = self.reconciled(&layout, state);
        if let Some(header) = layout.header
            && header.contains(position)
        {
            return self
                .column_at(&layout, header, &state, position)
                .map(|column| TableHit::Header { column });
        }
        let row = self.row_at_layout(&layout, &state, position)?;
        self.column_at(&layout, layout.body, &state, position)
            .map_or(Some(TableHit::Row { row }), |column| {
                Some(TableHit::Cell { row, column })
            })
    }

    /// Handle one event.
    pub fn handle_event(&self, area: Rect, state: &mut TableState, event: &Event) -> TableOutcome {
        let layout = self.layout(area);
        self.handle_event_with_layout(&layout, area, state, event)
    }

    fn handle_event_with_layout(
        &self,
        layout: &TableLayout,
        area: Rect,
        state: &mut TableState,
        event: &Event,
    ) -> TableOutcome {
        if state.interaction.disabled {
            return TableOutcome::Ignored;
        }
        self.reconcile_layout(layout, state);
        match event {
            Event::Key(stroke) if self.policy.keyboard && stroke.modifiers.is_empty() => {
                match stroke.key {
                    KeyCode::Up => self.move_selection(layout, state, -1),
                    KeyCode::Down => self.move_selection(layout, state, 1),
                    KeyCode::Home => self.select_index(layout, state, 0),
                    KeyCode::End => {
                        self.select_index(layout, state, self.rows.len().saturating_sub(1))
                    }
                    KeyCode::Left => self.scroll_horizontal(layout, state, -1),
                    KeyCode::Right => self.scroll_horizontal(layout, state, 1),
                    KeyCode::Enter => state
                        .selected
                        .map_or(TableOutcome::Ignored, TableOutcome::Selected),
                    _ => TableOutcome::Ignored,
                }
            }
            Event::Mouse(mouse) if self.policy.mouse.enabled => {
                self.handle_mouse(layout, area, state, *mouse)
            }
            Event::Key(_)
            | Event::Mouse(_)
            | Event::Resize(_)
            | Event::Paste(_)
            | Event::Focus(_)
            | Event::Tick
            | Event::User(_) => TableOutcome::Ignored,
        }
    }

    fn handle_mouse(
        &self,
        layout: &TableLayout,
        area: Rect,
        state: &mut TableState,
        mouse: MouseEvent,
    ) -> TableOutcome {
        let scroll_view = self.scroll_view();
        let event = Event::Mouse(mouse);
        let scroll_area = scroll_area_of(layout, area);
        let scrollbar = scroll_view.handle_scrollbar_event(
            scroll_area,
            &layout.viewport,
            &mut state.scroll,
            &event,
        );
        if scrollbar != ScrollViewOutcome::Ignored || state.scroll.dragging_scrollbar() {
            return TableOutcome::Redraw;
        }
        if matches!(
            mouse.kind,
            MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight
        ) {
            return scroll_outcome(scroll_view.handle_event(
                area,
                &layout.viewport,
                &mut state.scroll,
                &event,
            ));
        }
        match mouse.kind {
            MouseEventKind::Move if self.policy.mouse.hover => {
                let hovered = self.row_at_layout(layout, state, mouse.position);
                if hovered == state.hovered {
                    TableOutcome::Ignored
                } else {
                    state.hovered = hovered;
                    TableOutcome::Redraw
                }
            }
            MouseEventKind::Down(MouseButton::Left) if self.policy.mouse.click => self
                .row_at_layout(layout, state, mouse.position)
                .map_or(TableOutcome::Ignored, |row| {
                    self.select_index(layout, state, row)
                }),
            MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight
            | MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::Move => TableOutcome::Ignored,
        }
    }

    fn column_at(
        &self,
        layout: &TableLayout,
        area: Rect,
        state: &TableState,
        position: Point,
    ) -> Option<usize> {
        if !area.contains(position) {
            return None;
        }
        let separator_width = display_width(self.policy.cell_separator);
        let sticky = self
            .policy
            .sticky_left_columns
            .min(layout.column_widths.len());
        let sticky_width = layout
            .column_widths
            .iter()
            .take(sticky)
            .map(|width| usize::from(*width))
            .sum::<usize>()
            .saturating_add(separator_width.saturating_mul(sticky.saturating_sub(1)));
        let local_x = usize::from(position.x.saturating_sub(area.x));
        let horizontal = state.horizontal_scroll();
        let content_x = if sticky > 0 && local_x < sticky_width {
            local_x
        } else if sticky > 0 {
            local_x
                .saturating_add(horizontal)
                .saturating_add(sticky_width)
                .saturating_add(separator_width)
        } else {
            local_x.saturating_add(horizontal)
        };
        let mut start = 0usize;
        for (index, width) in layout.column_widths.iter().copied().enumerate() {
            let end = start.saturating_add(usize::from(width));
            if content_x >= start && content_x < end {
                return Some(index);
            }
            start = end.saturating_add(separator_width);
        }
        None
    }

    /// Return visible body-row hit regions for tests and semantic inspection.
    ///
    /// Regions are derived from the reconciled logical scroll offset: rows
    /// above the offset are skipped exactly and the final visible row is
    /// clipped to the viewport bottom.
    #[must_use]
    pub fn row_hit_regions(&self, area: Rect, state: &TableState) -> Vec<HitRegion<usize>> {
        let layout = self.layout(area);
        self.row_hit_regions_layout(&layout, &self.reconciled(&layout, state))
    }

    /// Visible row regions for an already reconciled `state`.
    fn row_hit_regions_layout(
        &self,
        layout: &TableLayout,
        state: &TableState,
    ) -> Vec<HitRegion<usize>> {
        let body = layout.body;
        let offset = state.scroll.vertical_offset();
        let viewport_end = offset.saturating_add(usize::from(body.height));
        let mut start = 0usize;
        let mut regions = Vec::new();
        for (index, row) in self.rows.iter().enumerate() {
            let end = start.saturating_add(row.height());
            if end <= offset {
                start = end;
                continue;
            }
            if start >= viewport_end {
                break;
            }
            let visible_start = start.max(offset);
            let visible_end = end.min(viewport_end);
            let height = visible_end.saturating_sub(visible_start);
            if height > 0 {
                regions.push(HitRegion::new(
                    index,
                    Rect::new(
                        body.x,
                        body.y
                            .saturating_add(u16_saturating(visible_start.saturating_sub(offset))),
                        body.width,
                        u16_saturating(height),
                    ),
                ));
            }
            start = end;
        }
        regions
    }

    /// Return visible body-row index at a point, if any.
    #[must_use]
    pub fn row_at(&self, area: Rect, state: &TableState, position: Point) -> Option<usize> {
        let layout = self.layout(area);
        self.row_at_layout(&layout, &self.reconciled(&layout, state), position)
    }

    /// Visible row at a point for an already reconciled `state`.
    fn row_at_layout(
        &self,
        layout: &TableLayout,
        state: &TableState,
        position: Point,
    ) -> Option<usize> {
        hit_region_at(&self.row_hit_regions_layout(layout, state), position)
            .map(|region| region.key)
    }

    /// Exact stacked body height in logical rows.
    fn body_height(&self) -> usize {
        self.rows.iter().map(TableRow::height).sum()
    }

    /// Logical start row of one source row.
    fn row_start(&self, index: usize) -> Option<usize> {
        if index >= self.rows.len() {
            return None;
        }
        Some(self.rows.iter().take(index).map(TableRow::height).sum())
    }

    fn preferred_content_width(&self) -> usize {
        let width = self
            .columns
            .iter()
            .map(|column| match column.width {
                TableWidth::Fixed(width)
                | TableWidth::Min(width)
                | TableWidth::Max(width)
                | TableWidth::Flex(width) => usize::from(width.max(1)),
                TableWidth::Percentage(percent) => usize::from(percent.max(1)),
                TableWidth::Ratio(numerator, denominator) if denominator > 0 => {
                    usize::from(numerator.max(1))
                }
                TableWidth::Ratio(_, _) => 1,
            })
            .sum::<usize>();
        width.saturating_add(
            display_width(self.policy.cell_separator)
                .saturating_mul(self.columns.len().saturating_sub(1)),
        )
    }

    /// Logical content width of the body viewport.
    ///
    /// The viewport is the full body width so sticky columns stay inside the
    /// clip; the content extends past it by exactly the cells that horizontal
    /// scrolling can reveal. With sticky columns the scrolled region directly
    /// follows the pinned cells, so the separator after the last sticky
    /// column never renders and is excluded from the scrollable extent.
    fn horizontal_content_width(&self, widths: &[u16], body_width: u16) -> u16 {
        let sticky = self.policy.sticky_left_columns.min(widths.len());
        let total = self.preferred_content_width();
        let scrollable = if sticky == 0 {
            total
        } else {
            total.saturating_sub(display_width(self.policy.cell_separator))
        };
        u16_saturating(scrollable).max(body_width)
    }

    fn move_selection(
        &self,
        layout: &TableLayout,
        state: &mut TableState,
        delta: i32,
    ) -> TableOutcome {
        if self.rows.is_empty() {
            return TableOutcome::Ignored;
        }
        let current = state
            .selected
            .unwrap_or(0)
            .min(self.rows.len().saturating_sub(1));
        let next = if delta.is_negative() {
            current.saturating_sub(1)
        } else {
            current
                .saturating_add(1)
                .min(self.rows.len().saturating_sub(1))
        };
        if next == current {
            return TableOutcome::Ignored;
        }
        self.select_index(layout, state, next)
    }

    fn scroll_horizontal(
        &self,
        layout: &TableLayout,
        state: &mut TableState,
        delta: i32,
    ) -> TableOutcome {
        let before_column = state.selected_column;
        let scrolled = ScrollView::scroll_horizontal_by(
            &layout.viewport,
            &mut state.scroll,
            isize::try_from(delta).unwrap_or(isize::MAX),
        );
        let _ = self.move_column(state, delta);
        if scrolled == ScrollViewOutcome::Ignored && state.selected_column == before_column {
            TableOutcome::Ignored
        } else {
            TableOutcome::Redraw
        }
    }

    fn move_column(&self, state: &mut TableState, delta: i32) -> TableOutcome {
        if self.columns.is_empty() {
            return TableOutcome::Ignored;
        }
        let current = state
            .selected_column
            .unwrap_or(0)
            .min(self.columns.len().saturating_sub(1));
        let next = if delta.is_negative() {
            current.saturating_sub(1)
        } else {
            current
                .saturating_add(1)
                .min(self.columns.len().saturating_sub(1))
        };
        if next == current && state.selected_column == Some(current) {
            return TableOutcome::Ignored;
        }
        state.selected_column = Some(next);
        TableOutcome::Redraw
    }

    fn select_index(
        &self,
        layout: &TableLayout,
        state: &mut TableState,
        index: usize,
    ) -> TableOutcome {
        if self.rows.get(index).is_none_or(|row| row.disabled) {
            return TableOutcome::Ignored;
        }
        state.selected = Some(index);
        self.reconcile_layout(layout, state);
        TableOutcome::Focused(index)
    }

    /// Copy of `state` reconciled against `layout` without mutating the caller.
    fn reconciled(&self, layout: &TableLayout, state: &TableState) -> TableState {
        let mut reconciled = state.clone();
        self.reconcile_layout(layout, &mut reconciled);
        reconciled
    }

    fn visible_line(&self, line: &Line, widths: &[u16], state: &TableState, width: u16) -> Line {
        let horizontal = state.horizontal_scroll();
        let sticky = self.policy.sticky_left_columns.min(widths.len());
        if sticky == 0 {
            return line_viewport(line, horizontal, usize::from(width));
        }
        let separator_width = display_width(self.policy.cell_separator);
        let sticky_width = widths
            .iter()
            .take(sticky)
            .map(|width| usize::from(*width))
            .sum::<usize>()
            .saturating_add(separator_width.saturating_mul(sticky.saturating_sub(1)))
            .min(usize::from(width));
        let mut left = line_viewport(line, 0, sticky_width);
        let remaining = usize::from(width).saturating_sub(sticky_width);
        if remaining > 0 {
            let mut right = line_viewport(
                line,
                sticky_width
                    .saturating_add(separator_width)
                    .saturating_add(horizontal),
                remaining,
            );
            left.spans.append(&mut right.spans);
        }
        left
    }

    fn header_cell_line(&self, column: &TableColumn<'_>) -> Line {
        let Some(sort) = column.sort else {
            return Line::from(column.title);
        };
        let symbol = match sort {
            TableSortDirection::Ascending => self.policy.sort_ascending_symbol,
            TableSortDirection::Descending => self.policy.sort_descending_symbol,
        };
        Line::from(format!("{} {}", column.title, symbol))
    }

    fn row_style(&self, index: usize, row: &TableRow, state: &TableState) -> Style {
        if row.disabled {
            self.styles.disabled
        } else if state.selected == Some(index) {
            self.styles.selected
        } else if state.hovered == Some(index) {
            self.styles.hovered
        } else {
            self.styles.row
        }
    }

    fn separator_line(&self, widths: &[u16]) -> Line {
        let cells = widths
            .iter()
            .map(|width| Line::from("─".repeat(usize::from(*width))));
        self.row_line(widths, cells, true, None, &TableState::default())
    }

    fn row_line(
        &self,
        widths: &[u16],
        cells: impl Iterator<Item = Line>,
        header: bool,
        row_index: Option<usize>,
        state: &TableState,
    ) -> Line {
        let mut spans = Vec::new();
        for (index, (cell, width)) in cells.zip(widths.iter().copied()).enumerate() {
            if index > 0 {
                spans.push(Span::styled(
                    self.policy.cell_separator,
                    self.styles.separator,
                ));
            }
            let align = self
                .columns
                .get(index)
                .map_or(TableAlign::Left, |column| column.align);
            let line = format_cell_line(
                &cell,
                width,
                align,
                self.columns.get(index).is_none_or(|column| column.truncate),
            );
            let base = if header {
                self.styles.header
            } else if row_index == state.selected && state.selected_column == Some(index) {
                self.styles.selected_cell
            } else if state.selected_column == Some(index) {
                self.styles.selected_column
            } else {
                self.styles.row
            };
            spans.extend(
                line.spans
                    .into_iter()
                    .map(|span| Span::styled(span.content, base.patch(span.style))),
            );
        }
        Line::from_spans(spans)
    }
}

/// Stacked body rows painted as the single child of the shared viewport.
///
/// The viewport translation and clip decide which rows reach the buffer, so
/// painting is bounded to rows intersecting the viewport. Horizontal
/// projection stays per line so sticky columns remain pinned.
struct BodyRows<'a, 'table> {
    table: &'table Table<'a>,
    layout: &'table TableLayout,
    state: &'table TableState,
}

impl Component for BodyRows<'_, '_> {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        LayoutNode::leaf(
            self.layout.viewport.children.first().map_or_else(
                || LayoutId::new("table.content"),
                |child| child.node.id.clone(),
            ),
            LogicalSize::new(constraints.max_width(), self.table.body_height()),
        )
    }

    fn paint(&self, _layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let body = self.layout.body;
        let offset = self.state.scroll.vertical_offset();
        let end = offset.saturating_add(usize::from(body.height));
        let mut row = 0usize;
        for (source, table_row) in self.table.rows.iter().enumerate() {
            if row >= end {
                break;
            }
            for line_index in 0..table_row.height() {
                if row >= offset && row < end {
                    let line = self.table.row_line(
                        &self.layout.column_widths,
                        table_row.cells.iter().map(|cell| {
                            cell.get(line_index)
                                .cloned()
                                .unwrap_or_else(|| Line::from(""))
                        }),
                        false,
                        Some(source),
                        self.state,
                    );
                    cx.write_line_with_fallback_style(
                        LocalRect::new(0, i64::try_from(row).unwrap_or(i64::MAX), body.width, 1),
                        &self.table.visible_line(
                            &line,
                            &self.layout.column_widths,
                            self.state,
                            body.width,
                        ),
                        self.table.row_style(source, table_row, self.state),
                    );
                }
                row = row.saturating_add(1);
            }
        }
    }
}

/// The complete scroll area (body plus gutters) for one laid-out table.
const fn scroll_area_of(layout: &TableLayout, area: Rect) -> Rect {
    let top = layout.body.y;
    Rect::new(area.x, top, area.width, area.bottom().saturating_sub(top))
}

/// Scroll state with the horizontal offset cleared.
///
/// The body viewport translates rows only; horizontal projection is applied
/// per line so sticky columns stay pinned.
const fn vertical_only(mut state: ScrollViewState) -> ScrollViewState {
    state.set_horizontal_offset(0);
    state
}

fn viewport_id(id: &LayoutId) -> LayoutId {
    LayoutId::new(format!("{}.viewport", id.as_str()))
}

fn content_id(id: &LayoutId) -> LayoutId {
    LayoutId::new(format!("{}.content", id.as_str()))
}

const fn scroll_outcome(outcome: ScrollViewOutcome) -> TableOutcome {
    match outcome {
        ScrollViewOutcome::Ignored => TableOutcome::Ignored,
        ScrollViewOutcome::Scrolled { .. } | ScrollViewOutcome::HorizontalScrolled { .. } => {
            TableOutcome::Redraw
        }
    }
}

fn format_cell_line(line: &Line, width: u16, align: TableAlign, truncate: bool) -> Line {
    let width = usize::from(width);
    let line_width = line.width();
    if align == TableAlign::Left && (!truncate || line_width <= width) {
        let mut line = line.clone();
        if line_width < width {
            line.push_span(Span::raw(" ".repeat(width.saturating_sub(line_width))));
        }
        return line;
    }
    let line = if truncate && line_width > width {
        truncate_line_to_display_width(line, width)
    } else {
        line.clone()
    };
    let line_width = line.width();
    let padding = width.saturating_sub(line_width);
    let (left, right) = match align {
        TableAlign::Left => (0, padding),
        TableAlign::Center => (padding / 2, padding.saturating_sub(padding / 2)),
        TableAlign::Right => (padding, 0),
    };
    let mut spans = Vec::new();
    if left > 0 {
        spans.push(Span::raw(" ".repeat(left)));
    }
    spans.extend(line.spans);
    if right > 0 {
        spans.push(Span::raw(" ".repeat(right)));
    }
    Line::from_spans(spans)
}

fn resolve_column_widths(
    columns: &[TableColumn<'_>],
    total_width: u16,
    separator: &str,
) -> Vec<u16> {
    if columns.is_empty() {
        return Vec::new();
    }
    let separator_total =
        u16_saturating(display_width(separator).saturating_mul(columns.len().saturating_sub(1)));
    let available = total_width.saturating_sub(separator_total);
    let fixed: u16 = columns
        .iter()
        .map(|column| match column.width {
            TableWidth::Fixed(width) | TableWidth::Min(width) | TableWidth::Max(width) => width,
            TableWidth::Percentage(percent) => {
                u16::try_from((u32::from(available) * u32::from(percent.min(100))) / 100)
                    .unwrap_or(available)
            }
            TableWidth::Ratio(numerator, denominator) if denominator > 0 => u16::try_from(
                (u32::from(available) * u32::from(numerator)) / u32::from(denominator),
            )
            .unwrap_or(available),
            TableWidth::Ratio(_, _) | TableWidth::Flex(_) => 0,
        })
        .sum();
    let flex_weight: u16 = columns
        .iter()
        .map(|column| match column.width {
            TableWidth::Flex(weight) | TableWidth::Min(weight) => weight.max(1),
            TableWidth::Fixed(_)
            | TableWidth::Max(_)
            | TableWidth::Percentage(_)
            | TableWidth::Ratio(_, _) => 0,
        })
        .sum();
    let flex_available = available.saturating_sub(fixed);
    columns
        .iter()
        .map(|column| match column.width {
            TableWidth::Fixed(width) | TableWidth::Max(width) => width.min(available),
            TableWidth::Min(minimum) if flex_weight > 0 => minimum.saturating_add(
                u16::try_from(
                    (u32::from(flex_available) * u32::from(minimum.max(1)))
                        / u32::from(flex_weight),
                )
                .unwrap_or(flex_available),
            ),
            TableWidth::Min(minimum) => minimum,
            TableWidth::Percentage(percent) => {
                u16::try_from((u32::from(available) * u32::from(percent.min(100))) / 100)
                    .unwrap_or(available)
                    .max(1)
            }
            TableWidth::Ratio(numerator, denominator) if denominator > 0 => u16::try_from(
                (u32::from(available) * u32::from(numerator)) / u32::from(denominator),
            )
            .unwrap_or(available)
            .max(1),
            TableWidth::Ratio(_, _) => 1,
            TableWidth::Flex(weight) if flex_weight > 0 => u16::try_from(
                (u32::from(flex_available) * u32::from(weight.max(1))) / u32::from(flex_weight),
            )
            .unwrap_or(flex_available)
            .max(1),
            TableWidth::Flex(weight) => weight.max(1),
        })
        .collect()
}

impl crate::theme::ComponentTheme {
    /// Convert this semantic component theme into [`TableStyles`].
    #[must_use]
    pub fn table_styles(self) -> TableStyles {
        TableStyles::from(self)
    }
}

impl From<crate::theme::ComponentTheme> for TableStyles {
    fn from(theme: crate::theme::ComponentTheme) -> Self {
        let theme = theme.for_surface(crate::theme::ComponentSurfaceDepth::Normal);
        Self {
            header: theme.info.add_modifier(bmux_tui::style::Modifier::BOLD),
            row: theme.text,
            selected: theme.selected,
            selected_column: theme.focused,
            selected_cell: theme.warning,
            hovered: theme.info,
            disabled: theme.disabled,
            separator: theme.border,
            empty: theme.muted,
            scrollbar: theme.scrollbar_styles(),
        }
    }
}

#[cfg(test)]
fn format_cell(text: &str, width: u16, align: TableAlign, truncate: bool) -> String {
    let line = Line::from(text);
    format_cell_line(&line, width, align, truncate).plain_text()
}

/// Canonical component-lifecycle table.
///
/// The component measures the table's natural content size, paints header,
/// visible rows, and integrated scrollbars through the scoped paint context,
/// registers one composite roving-focus region plus one visible region per
/// row, and routes events through the same resolved layout. Table state
/// remains caller-owned through an interior-mutable `RefCell`.
pub struct TableComponent<'a, 'state> {
    id: LayoutId,
    table: Table<'a>,
    state: &'state RefCell<TableState>,
}

impl<'a, 'state> TableComponent<'a, 'state> {
    /// Create a table with stable identity and caller-owned state.
    #[must_use]
    pub fn new(
        id: impl Into<LayoutId>,
        columns: &'a [TableColumn<'a>],
        rows: &'a [TableRow],
        state: &'state RefCell<TableState>,
    ) -> Self {
        Self {
            id: id.into(),
            table: Table::new(columns, rows),
            state,
        }
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: TablePolicy) -> Self {
        self.table.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: TableStyles) -> Self {
        self.table.styles = styles;
        self
    }

    /// Set the text shown when the table has no rows.
    #[must_use]
    pub const fn empty(mut self, empty: &'a str) -> Self {
        self.table.empty = empty;
        self
    }
}

impl Component for TableComponent<'_, '_> {
    fn revision(&self) -> ComponentRevision {
        let state = self.state.borrow();
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        for column in self.table.columns {
            column.title.hash(&mut layout);
            format!("{:?}", column.width).hash(&mut layout);
        }
        for row in self.table.rows {
            row.height().hash(&mut layout);
            for cell in &row.cells {
                for line in cell {
                    format!("{line:?}").hash(&mut layout);
                }
            }
        }
        self.table.empty.hash(&mut layout);
        self.table.policy.header.hash(&mut layout);
        self.table.policy.header_separator.hash(&mut layout);
        self.table.policy.cell_separator.hash(&mut layout);
        format!("{:?}", self.table.policy.vertical_scrollbar).hash(&mut layout);
        format!("{:?}", self.table.policy.horizontal_scrollbar).hash(&mut layout);

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        for row in self.table.rows {
            row.disabled.hash(&mut paint);
        }
        for column in self.table.columns {
            format!("{:?}", column.sort).hash(&mut paint);
            format!("{:?}", column.align).hash(&mut paint);
        }
        format!("{:?}", self.table.styles).hash(&mut paint);
        format!("{:?}", *state).hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let (width, height) = self.table.size();
        let size = constraints.constrain(LogicalSize::new(width, usize::from(height)));
        let area = Rect::new(0, 0, size.width, u16_saturating(size.height));
        let table_layout = self.table.layout_with_id(&self.id, area);
        LayoutNode::with_children(
            self.id.clone(),
            size,
            vec![ChildLayout::new(
                table_layout.body.x,
                usize::from(table_layout.body.y),
                table_layout.viewport,
            )],
        )
        .with_metadata(LayoutMetadata::new().semantic("table"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let area = Rect::new(0, 0, layout.size.width, u16_saturating(layout.size.height));
        if area.is_empty() {
            return;
        }
        let table_layout = self.table.layout_with_id(&self.id, area);
        // Reconcile caller-owned scroll state against the authoritative
        // viewport before painting so the painted offset, hit regions, and
        // subsequent input routing agree exactly.
        self.table
            .reconcile_layout(&table_layout, &mut self.state.borrow_mut());
        let state = self.state.borrow();
        cx.push_hit(
            SceneRegion::new(self.id.as_str(), area)
                .role(HitRole::ListItem)
                .hoverable(self.table.policy.mouse.hover)
                .focusable(true)
                .enabled(!state.interaction.disabled),
        );
        for region in self.table.row_hit_regions_layout(&table_layout, &state) {
            let disabled = self
                .table
                .rows
                .get(region.key)
                .is_none_or(|row| row.disabled);
            let row_id = format!("{}.row.{}", self.id.as_str(), region.key);
            cx.push_hit(
                SceneRegion::new(row_id.clone(), region.rect)
                    .role(HitRole::ListItem)
                    .hoverable(self.table.policy.mouse.hover)
                    .enabled(!state.interaction.disabled && !disabled),
            );
            cx.push_semantic(SemanticRegion::new(row_id, region.rect, "row"));
        }
        self.table.paint_layout(&table_layout, area, &state, cx);
        cx.push_semantic(SemanticRegion::new(self.id.as_str(), area, "table"));
        cx.push_damage(LocalRect::new(0, 0, area.width, area.height));
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        let Some(area) = cx.find_rect(&layout.id) else {
            return EventOutcome::Ignored;
        };
        let table_layout = self.table.layout_with_id(&self.id, area);
        let outcome = self.table.handle_event_with_layout(
            &table_layout,
            area,
            &mut self.state.borrow_mut(),
            event,
        );
        match outcome {
            TableOutcome::Ignored => EventOutcome::Ignored,
            TableOutcome::Redraw | TableOutcome::Focused(_) | TableOutcome::Selected(_) => {
                EventOutcome::Redraw
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use bmux_keyboard::{KeyCode, KeyStroke};
    use bmux_tui::buffer::Buffer;
    use bmux_tui::component::{Component, Constraints, EventCx, LayoutCx, LayoutId, LogicalSize};
    use bmux_tui::event::{Event, EventOutcome, MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};
    use bmux_tui::hit::HitRole;
    use bmux_tui::paint::{LocalRect, PaintCx};
    use bmux_tui::prelude::{Line, Span};
    use bmux_tui::style::{Color, Style};

    use crate::scroll_view::ScrollView;
    use crate::scrollbar_layout::ScrollbarAxisLayoutMode;

    use super::{
        Table, TableAlign, TableColumn, TableComponent, TableHit, TableOutcome, TablePolicy,
        TableRow, TableSortDirection, TableState, TableStyles, format_cell,
    };

    fn render_component(component: &TableComponent<'_, '_>, area: Rect, frame: &mut Frame<'_>) {
        let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
        PaintCx::new(frame).with_child(
            i32::from(area.x),
            i64::from(area.y),
            LocalRect::new(0, 0, area.width, area.height),
            |cx| component.paint(&layout, cx),
        );
    }

    trait TableTestRender {
        fn render(&self, area: Rect, state: &TableState, frame: &mut Frame<'_>);
    }

    impl TableTestRender for Table<'_> {
        fn render(&self, area: Rect, state: &TableState, frame: &mut Frame<'_>) {
            let state = RefCell::new(state.clone());
            let component = TableComponent {
                id: "test.table".into(),
                table: self.clone(),
                state: &state,
            };
            render_component(&component, area, frame);
        }
    }

    #[test]
    fn component_measures_natural_size_and_registers_composite_and_row_geometry() {
        let columns = [
            TableColumn::new("Name").fixed(6),
            TableColumn::new("Size").fixed(4),
        ];
        let rows = [
            TableRow::new(vec!["one", "1"]),
            TableRow::multiline(vec![
                vec![Line::from("two"), Line::from("more")],
                vec![Line::from("2")],
            ]),
            TableRow::new(vec!["three", "3"]).disabled(true),
        ];
        let state = RefCell::new(TableState::new(Some(0)));
        let component = TableComponent::new("grid", &columns, &rows, &state);
        let mut cx = LayoutCx::new();
        let layout = component.layout(Constraints::for_width(11), &mut cx);
        assert_eq!(layout.size.height, 5, "header plus exact row heights");
        assert_eq!(cx.measured_nodes(), 1);

        let mut buffer = Buffer::empty(Rect::new(0, 0, 14, 8));
        let mut frame = Frame::new(&mut buffer);
        render_component(&component, Rect::new(2, 1, 11, 4), &mut frame);

        let regions = frame.hits().regions();
        assert_eq!(regions[0].id.as_str(), "grid");
        assert_eq!(regions[0].area, Rect::new(2, 1, 11, 4));
        assert_eq!(regions[0].role, HitRole::ListItem);
        assert!(regions[0].focusable);
        assert_eq!(regions[1].id.as_str(), "grid.row.0");
        assert_eq!(regions[1].area, Rect::new(2, 2, 11, 1));
        assert!(regions[1].enabled);
        assert!(!regions[1].focusable);
        assert_eq!(regions[2].id.as_str(), "grid.row.1");
        assert_eq!(regions[2].area, Rect::new(2, 3, 11, 2));
        assert_eq!(
            regions.len(),
            3,
            "the clipped disabled row does not register"
        );
        assert_eq!(frame.hits().focus_targets(None).len(), 1);
        assert!(
            frame
                .semantics()
                .regions()
                .iter()
                .any(|region| region.id == "grid.row.1" && region.role == "row")
        );
        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("  Name   Size ")
        );
    }

    #[test]
    fn component_routes_events_through_resolved_layout_and_updates_caller_state() {
        let columns = [TableColumn::new("Name").fixed(6)];
        let rows = [TableRow::new(vec!["one"]), TableRow::new(vec!["two"])];
        let mut initial = TableState::new(Some(0));
        initial.set_focused(true);
        let state = RefCell::new(initial);
        let component = TableComponent::new("grid", &columns, &rows, &state);
        let layout = component.layout(
            Constraints::tight(Rect::new(0, 0, 6, 3).size()),
            &mut LayoutCx::new(),
        );

        assert_eq!(
            component.event(
                &Event::Key(KeyStroke::simple(KeyCode::Down)),
                &layout,
                &mut EventCx::new(&layout),
            ),
            EventOutcome::Redraw
        );
        assert_eq!(state.borrow().selected(), Some(1));
        assert_eq!(
            component.event(&Event::Tick, &layout, &mut EventCx::new(&layout)),
            EventOutcome::Ignored
        );
    }

    #[test]
    fn component_revision_separates_layout_and_paint_changes() {
        let columns = [TableColumn::new("Name").fixed(6)];
        let rows = [TableRow::new(vec!["one"])];
        let state = RefCell::new(TableState::new(None));
        let component = TableComponent::new("grid", &columns, &rows, &state);
        let before = component.revision();

        state.borrow_mut().set_selected(Some(0));
        let paint_only = component.revision();
        assert_eq!(before.layout, paint_only.layout);
        assert_ne!(before.paint, paint_only.paint);

        let more = [TableRow::new(vec!["one"]), TableRow::new(vec!["two"])];
        let relayout = TableComponent::new("grid", &columns, &more, &state).revision();
        assert_ne!(before.layout, relayout.layout);
    }

    #[test]
    fn resolves_fixed_and_flex_columns() {
        let columns = [
            TableColumn::new("A").fixed(4),
            TableColumn::new("B").flex(1),
        ];
        let rows = [TableRow::new(vec!["one", "two"])];
        let table = Table::new(&columns, &rows);

        assert_eq!(
            table.layout(Rect::new(0, 0, 12, 3)).column_widths,
            vec![4, 7]
        );
    }

    #[test]
    fn resolves_fixed_min_max_percentage_ratio_and_flex_columns() {
        let columns = [
            TableColumn::new("Fixed").fixed(4),
            TableColumn::new("Pct").percentage(25),
            TableColumn::new("Ratio").ratio(1, 4),
            TableColumn::new("Min").min(2),
            TableColumn::new("Max").max(3),
            TableColumn::new("Flex").flex(1),
        ];
        let rows = [TableRow::new(vec!["a", "b", "c", "d", "e", "f"])];
        let table = Table::new(&columns, &rows);

        assert_eq!(
            table.layout(Rect::new(0, 0, 41, 3)).column_widths,
            vec![4, 9, 9, 8, 3, 3]
        );
    }

    #[test]
    fn formats_aligned_and_truncated_cells() {
        assert_eq!(format_cell("abcdef", 4, TableAlign::Left, true), "abc…");
        assert_eq!(format_cell("x", 3, TableAlign::Right, true), "  x");
        assert_eq!(format_cell("x", 3, TableAlign::Center, true), " x ");
    }

    #[test]
    fn renders_rich_cell_content_preserving_span_style() {
        let columns = [TableColumn::new("Name").fixed(8)];
        let cell_style = Style::new().fg(Color::Yellow);
        let rows = [TableRow::rich(vec![Line::from_spans([
            Span::raw("a"),
            Span::styled("b", cell_style),
        ])])];
        let state = TableState::new(None);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 2));
        let mut frame = Frame::new(&mut buffer);

        Table::new(&columns, &rows).render(Rect::new(0, 0, 8, 2), &state, &mut frame);

        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("ab      "));
        assert_eq!(
            frame.buffer().get(Point::new(1, 1)).map(|cell| cell.style),
            Some(TableStyles::default().row.patch(cell_style))
        );
    }

    #[test]
    fn exposes_row_hit_regions_without_rendering() {
        let columns = [TableColumn::new("Name").fixed(8)];
        let rows = [
            TableRow::new(vec!["one"]),
            TableRow::multiline(vec![vec![Line::from("two"), Line::from("details")]]),
        ];
        let table = Table::new(&columns, &rows);
        let state = TableState::new(Some(0));

        let regions = table.row_hit_regions(Rect::new(1, 2, 8, 4), &state);

        assert_eq!(regions[0].key, 0);
        assert_eq!(regions[0].rect, Rect::new(1, 3, 8, 1));
        assert_eq!(regions[1].key, 1);
        assert_eq!(regions[1].rect, Rect::new(1, 4, 8, 2));
        assert_eq!(
            table.row_at(Rect::new(1, 2, 8, 4), &state, Point::new(2, 5)),
            Some(1)
        );
    }

    #[test]
    fn tiny_area_does_not_panic() {
        let columns = [TableColumn::new("Name").fixed(4)];
        let rows = [TableRow::multiline(vec![vec![
            Line::from("one"),
            Line::from("two"),
        ]])];
        let state = TableState::new(Some(0));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 0, 0));
        let mut frame = Frame::new(&mut buffer);

        Table::new(&columns, &rows).render(Rect::new(0, 0, 0, 0), &state, &mut frame);

        assert_eq!(
            Table::new(&columns, &rows)
                .layout(Rect::new(0, 0, 0, 0))
                .body
                .height,
            0
        );
    }

    #[test]
    fn header_remains_sticky_when_body_scrolls() {
        let columns = [TableColumn::new("Name").fixed(8)];
        let rows = [
            TableRow::new(vec!["one"]),
            TableRow::new(vec!["two"]),
            TableRow::new(vec!["three"]),
        ];
        let mut state = TableState::new(None);
        state.set_scroll(1);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 2));
        let mut frame = Frame::new(&mut buffer);

        Table::new(&columns, &rows).render(Rect::new(0, 0, 8, 2), &state, &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("Name    "));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("two     "));
    }

    #[test]
    fn horizontal_scroll_offsets_rendered_columns_and_keys_mark_focus_column() {
        let columns = [
            TableColumn::new("A").fixed(4),
            TableColumn::new("B").fixed(4),
        ];
        let rows = [TableRow::new(vec!["abcd", "efgh"])];
        let mut state = TableState::new(Some(0));
        state.set_focused(true);
        state.set_horizontal_scroll(2);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 2));
        let mut frame = Frame::new(&mut buffer);

        Table::new(&columns, &rows)
            .policy(TablePolicy {
                header: false,
                ..TablePolicy::default()
            })
            .render(Rect::new(0, 0, 5, 2), &state, &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("cd ef"));

        assert_eq!(
            Table::new(&columns, &rows).handle_event(
                Rect::new(0, 0, 5, 2),
                &mut state,
                &Event::Key(KeyStroke::simple(KeyCode::Right)),
            ),
            TableOutcome::Redraw
        );
        assert_eq!(state.horizontal_scroll(), 3);
        assert_eq!(state.selected_column(), Some(1));
    }

    #[test]
    fn sticky_header_renders_with_scrollbar_gutters_and_corner() {
        let columns = [TableColumn::new("Name").fixed(10)];
        let rows = [
            TableRow::new(vec!["one"]),
            TableRow::new(vec!["two"]),
            TableRow::new(vec!["three"]),
        ];
        let mut state = TableState::new(None);
        state.set_scroll(1);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 4));
        let mut frame = Frame::new(&mut buffer);

        Table::new(&columns, &rows)
            .policy(
                TablePolicy::bare()
                    .vertical_scrollbar(ScrollbarAxisLayoutMode::Gutter)
                    .horizontal_scrollbar(ScrollbarAxisLayoutMode::Gutter),
            )
            .render(Rect::new(0, 0, 6, 4), &state, &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("Name  "));
        assert_eq!(frame.buffer().row_symbols(3).as_deref(), Some("██─── "));
    }

    #[test]
    fn renders_integrated_vertical_scrollbar() {
        let columns = [TableColumn::new("Name").fixed(4)];
        let rows = [
            TableRow::new(vec!["one"]),
            TableRow::new(vec!["two"]),
            TableRow::new(vec!["three"]),
        ];
        let mut state = TableState::new(None);
        state.set_scroll(1);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 3));
        let mut frame = Frame::new(&mut buffer);

        Table::new(&columns, &rows)
            .policy(TablePolicy::bare().vertical_scrollbar(ScrollbarAxisLayoutMode::Gutter))
            .render(Rect::new(0, 0, 6, 3), &state, &mut frame);

        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("two  │"));
        assert_eq!(frame.buffer().row_symbols(2).as_deref(), Some("thr… █"));
    }

    #[test]
    fn vertical_scrollbar_mouse_updates_scroll() {
        let columns = [TableColumn::new("Name").fixed(4)];
        let rows = [
            TableRow::new(vec!["one"]),
            TableRow::new(vec!["two"]),
            TableRow::new(vec!["three"]),
        ];
        let mut state = TableState::new(None);

        let outcome = Table::new(&columns, &rows)
            .policy(TablePolicy::interactive().vertical_scrollbar(ScrollbarAxisLayoutMode::Gutter))
            .handle_event(
                Rect::new(0, 0, 6, 3),
                &mut state,
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Down(MouseButton::Left),
                    Point::new(5, 2),
                )),
            );

        assert_eq!(outcome, TableOutcome::Redraw);
        assert!(state.scroll() > 0);
    }

    #[test]
    fn renders_integrated_horizontal_scrollbar() {
        let columns = [TableColumn::new("Name").fixed(10)];
        let rows = [TableRow::new(vec!["abcdefghi"])];
        let state = TableState::new(Some(0));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 2));
        let mut frame = Frame::new(&mut buffer);

        Table::new(&columns, &rows)
            .policy(TablePolicy::bare().horizontal_scrollbar(ScrollbarAxisLayoutMode::Gutter))
            .render(Rect::new(0, 0, 5, 2), &state, &mut frame);

        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("██───"));
    }

    #[test]
    fn horizontal_scrollbar_mouse_updates_scroll() {
        let columns = [TableColumn::new("Name").fixed(10)];
        let rows = [TableRow::new(vec!["abcdefghi"])];
        let mut state = TableState::new(Some(0));

        let outcome = Table::new(&columns, &rows)
            .policy(
                TablePolicy::interactive().horizontal_scrollbar(ScrollbarAxisLayoutMode::Gutter),
            )
            .handle_event(
                Rect::new(0, 0, 5, 2),
                &mut state,
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Down(MouseButton::Left),
                    Point::new(4, 1),
                )),
            );

        assert_eq!(outcome, TableOutcome::Redraw);
        assert!(state.horizontal_scroll() > 0);
    }

    #[test]
    fn sticky_left_column_stays_visible_while_horizontally_scrolling() {
        let columns = [
            TableColumn::new("ID").fixed(2),
            TableColumn::new("Name").fixed(6),
            TableColumn::new("State").fixed(6),
        ];
        let rows = [TableRow::new(vec!["01", "abcdef", "ready"])];
        let mut state = TableState::new(None);
        state.set_horizontal_scroll(4);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 2));
        let mut frame = Frame::new(&mut buffer);

        Table::new(&columns, &rows)
            .policy(TablePolicy::bare().sticky_left_columns(1))
            .render(Rect::new(0, 0, 10, 2), &state, &mut frame);

        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("01ef ready"));
    }

    #[test]
    fn hit_tests_sticky_left_column_and_scrolled_columns() {
        let columns = [
            TableColumn::new("ID").fixed(2),
            TableColumn::new("Name").fixed(6),
            TableColumn::new("State").fixed(6),
        ];
        let rows = [TableRow::new(vec!["01", "abcdef", "ready"])];
        let mut state = TableState::new(None);
        state.set_horizontal_scroll(4);
        let table = Table::new(&columns, &rows).policy(TablePolicy::bare().sticky_left_columns(1));

        assert_eq!(
            table.hit_test(Rect::new(0, 0, 10, 2), &state, Point::new(1, 1)),
            Some(TableHit::Cell { row: 0, column: 0 })
        );
        assert_eq!(
            table.hit_test(Rect::new(0, 0, 10, 2), &state, Point::new(3, 1)),
            Some(TableHit::Cell { row: 0, column: 2 })
        );
    }

    #[test]
    fn hit_tests_row_column_cell_and_header() {
        let columns = [
            TableColumn::new("A").fixed(4),
            TableColumn::new("B").fixed(4),
        ];
        let rows = [
            TableRow::new(vec!["abcd", "efgh"]),
            TableRow::new(vec!["ijkl", "mnop"]),
        ];
        let state = TableState::new(None);
        let table = Table::new(&columns, &rows).policy(TablePolicy::bare());
        let area = Rect::new(0, 0, 9, 3);

        assert_eq!(
            table.hit_test(area, &state, Point::new(5, 0)),
            Some(TableHit::Header { column: 1 })
        );
        assert_eq!(
            table.hit_test(area, &state, Point::new(1, 1)),
            Some(TableHit::Cell { row: 0, column: 0 })
        );
        assert_eq!(
            table.hit_test(area, &state, Point::new(8, 2)),
            Some(TableHit::Cell { row: 1, column: 1 })
        );
    }

    #[test]
    fn hit_tests_visible_cells_after_horizontal_scroll() {
        let columns = [
            TableColumn::new("A").fixed(4),
            TableColumn::new("B").fixed(4),
        ];
        let rows = [TableRow::new(vec!["abcd", "efgh"])];
        let mut state = TableState::new(None);
        state.set_horizontal_scroll(5);
        let table = Table::new(&columns, &rows).policy(TablePolicy::bare());

        assert_eq!(
            table.hit_test(Rect::new(0, 0, 4, 2), &state, Point::new(0, 1)),
            Some(TableHit::Cell { row: 0, column: 1 })
        );
    }

    #[test]
    fn hit_tests_multiline_row_height() {
        let columns = [TableColumn::new("A").fixed(4)];
        let rows = [TableRow::multiline(vec![vec![
            Line::from("one"),
            Line::from("two"),
        ]])];
        let table = Table::new(&columns, &rows).policy(TablePolicy::bare());

        assert_eq!(
            table.hit_test(
                Rect::new(0, 0, 4, 3),
                &TableState::new(None),
                Point::new(1, 2)
            ),
            Some(TableHit::Cell { row: 0, column: 0 })
        );
    }

    #[test]
    fn hit_tests_sortable_header_cell() {
        let columns = [TableColumn::new("A").sort(Some(TableSortDirection::Ascending))];
        let rows = [TableRow::new(vec!["one"])];
        let table = Table::new(&columns, &rows).policy(TablePolicy::bare());

        assert_eq!(
            table.hit_test(
                Rect::new(0, 0, 4, 2),
                &TableState::new(None),
                Point::new(2, 0)
            ),
            Some(TableHit::Header { column: 0 })
        );
    }

    #[test]
    fn renders_sort_indicators_with_fixed_and_flex_columns_and_truncation() {
        let columns = [
            TableColumn::new("LongName")
                .fixed(6)
                .sort(Some(TableSortDirection::Ascending)),
            TableColumn::new("Count").sort(Some(TableSortDirection::Descending)),
        ];
        let rows = [TableRow::new(vec!["alpha", "1"])];
        let state = TableState::new(None);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 2));
        let mut frame = Frame::new(&mut buffer);

        Table::new(&columns, &rows)
            .policy(TablePolicy::bare().sort_symbols("A", "D"))
            .render(Rect::new(0, 0, 12, 2), &state, &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("LongN… Coun…")
        );
    }

    #[test]
    fn selected_column_and_cell_styles_are_applied() {
        let columns = [
            TableColumn::new("A").fixed(3),
            TableColumn::new("B").fixed(3),
        ];
        let rows = [
            TableRow::new(vec!["a1", "b1"]),
            TableRow::new(vec!["a2", "b2"]),
        ];
        let styles = TableStyles {
            selected: Style::new().bg(Color::Blue),
            selected_column: Style::new().bg(Color::Green),
            selected_cell: Style::new().bg(Color::Yellow),
            ..TableStyles::default()
        };
        let state = {
            let mut state = TableState::new(Some(1));
            state.set_selected_column(Some(1));
            state
        };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 7, 3));
        let mut frame = Frame::new(&mut buffer);

        Table::new(&columns, &rows).styles(styles).render(
            Rect::new(0, 0, 7, 3),
            &state,
            &mut frame,
        );

        assert_eq!(
            frame
                .buffer()
                .get(Point::new(4, 1))
                .map(|cell| cell.style.bg),
            Some(Some(Color::Green))
        );
        assert_eq!(
            frame
                .buffer()
                .get(Point::new(4, 2))
                .map(|cell| cell.style.bg),
            Some(Some(Color::Yellow))
        );
    }

    #[test]
    fn left_and_right_keys_move_selected_column() {
        let columns = [TableColumn::new("A"), TableColumn::new("B")];
        let rows = [TableRow::new(vec!["a", "b"])];
        let mut state = TableState::new(Some(0));
        state.set_focused(true);

        assert_eq!(
            Table::new(&columns, &rows).handle_event(
                Rect::new(0, 0, 8, 2),
                &mut state,
                &Event::Key(KeyStroke::simple(KeyCode::Right)),
            ),
            TableOutcome::Redraw
        );
        assert_eq!(state.selected_column(), Some(1));
    }

    #[test]
    fn renders_header_separator_when_enabled() {
        let columns = [
            TableColumn::new("Name").fixed(4),
            TableColumn::new("Kind").fixed(4),
        ];
        let rows = [TableRow::new(vec!["alpha", "file"])];
        let state = TableState::new(None);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 9, 3));
        let mut frame = Frame::new(&mut buffer);

        Table::new(&columns, &rows)
            .policy(TablePolicy {
                header_separator: true,
                ..TablePolicy::default()
            })
            .render(Rect::new(0, 0, 9, 3), &state, &mut frame);

        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("──── ────"));
        assert_eq!(frame.buffer().row_symbols(2).as_deref(), Some("alp… file"));
    }

    #[test]
    fn renders_multiline_rows_and_hit_tests_full_height() {
        let columns = [TableColumn::new("Name").fixed(8)];
        let rows = [
            TableRow::multiline(vec![vec![Line::from("one-a"), Line::from("one-b")]]),
            TableRow::new(vec!["two"]),
        ];
        let mut state = TableState::new(None);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 4));
        let mut frame = Frame::new(&mut buffer);

        Table::new(&columns, &rows).render(Rect::new(0, 0, 8, 4), &state, &mut frame);

        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("one-a   "));
        assert_eq!(frame.buffer().row_symbols(2).as_deref(), Some("one-b   "));
        assert_eq!(frame.buffer().row_symbols(3).as_deref(), Some("two     "));

        assert_eq!(
            Table::new(&columns, &rows).handle_event(
                Rect::new(0, 0, 8, 4),
                &mut state,
                &Event::Mouse(MouseEvent::new(MouseEventKind::Move, Point::new(0, 2))),
            ),
            TableOutcome::Redraw
        );
        assert_eq!(state.hovered, Some(0));
    }

    #[test]
    fn renders_header_and_rows() {
        let columns = [
            TableColumn::new("Name").fixed(6),
            TableColumn::new("Kind").fixed(5),
        ];
        let rows = [
            TableRow::new(vec!["alpha", "file"]),
            TableRow::new(vec!["beta", "dir"]),
        ];
        let state = TableState::new(Some(1));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 3));
        let mut frame = Frame::new(&mut buffer);

        Table::new(&columns, &rows).render(Rect::new(0, 0, 12, 3), &state, &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("Name   Kind ")
        );
        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("alpha  file ")
        );
    }

    #[test]
    fn keyboard_navigation_selects_rows() {
        let columns = [TableColumn::new("Name")];
        let rows = [TableRow::new(vec!["alpha"]), TableRow::new(vec!["beta"])];
        let mut state = TableState::new(Some(0));
        state.set_focused(true);

        let outcome = Table::new(&columns, &rows).handle_event(
            Rect::new(0, 0, 10, 3),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Down)),
        );

        assert_eq!(outcome, TableOutcome::Focused(1));
        assert_eq!(state.selected(), Some(1));
    }

    #[test]
    fn directly_dispatched_table_key_navigates_without_visual_focus() {
        let columns = [TableColumn::new("Name")];
        let rows = [TableRow::new(vec!["alpha"]), TableRow::new(vec!["beta"])];
        let mut state = TableState::new(Some(0));

        let outcome = Table::new(&columns, &rows).handle_event(
            Rect::new(0, 0, 10, 3),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Down)),
        );

        assert_eq!(outcome, TableOutcome::Focused(1));
        assert_eq!(state.selected(), Some(1));
    }

    #[test]
    fn mouse_click_selects_row() {
        let columns = [TableColumn::new("Name")];
        let rows = [TableRow::new(vec!["alpha"]), TableRow::new(vec!["beta"])];
        let mut state = TableState::new(Some(0));

        let outcome = Table::new(&columns, &rows).handle_event(
            Rect::new(0, 0, 10, 3),
            &mut state,
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(1, 2),
            )),
        );

        assert_eq!(outcome, TableOutcome::Focused(1));
        assert_eq!(state.selected(), Some(1));
    }

    #[test]
    fn disabled_rows_do_not_select() {
        let columns = [TableColumn::new("Name")];
        let rows = [TableRow::new(vec!["alpha"]).disabled(true)];
        let mut state = TableState::new(None);

        assert_eq!(
            Table::new(&columns, &rows).handle_event(
                Rect::new(0, 0, 10, 2),
                &mut state,
                &Event::Key(KeyStroke::simple(KeyCode::Home)),
            ),
            TableOutcome::Ignored
        );
    }

    #[test]
    fn bare_policy_ignores_events() {
        let columns = [TableColumn::new("Name")];
        let rows = [TableRow::new(vec!["alpha"]), TableRow::new(vec!["beta"])];
        let mut state = TableState::new(Some(0));

        assert_eq!(
            Table::new(&columns, &rows)
                .policy(TablePolicy::bare())
                .handle_event(
                    Rect::new(0, 0, 10, 3),
                    &mut state,
                    &Event::Key(KeyStroke::simple(KeyCode::Down)),
                ),
            TableOutcome::Ignored
        );
        assert_eq!(state.selected(), Some(0));
    }

    #[test]
    fn renders_empty_table_message() {
        let columns = [TableColumn::new("Name")];
        let rows = [];
        let state = TableState::default();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 2));
        let mut frame = Frame::new(&mut buffer);

        Table::new(&columns, &rows).render(Rect::new(0, 0, 10, 2), &state, &mut frame);

        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("No rows   "));
    }

    #[test]
    fn scroll_offset_controls_viewport() {
        let columns = [TableColumn::new("Name")];
        let rows = [
            TableRow::new(vec!["one"]),
            TableRow::new(vec!["two"]),
            TableRow::new(vec!["three"]),
        ];
        let mut state = TableState::new(None);
        state.set_scroll(1);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 2));
        let mut frame = Frame::new(&mut buffer);

        Table::new(&columns, &rows)
            .policy(TablePolicy {
                auto_scroll_selected: false,
                ..TablePolicy::interactive()
            })
            .render(Rect::new(0, 0, 8, 2), &state, &mut frame);

        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("two     "));
    }

    #[test]
    fn component_layout_exposes_shared_scroll_viewport_over_exact_body_content() {
        let columns = [TableColumn::new("Name").fixed(6)];
        let rows = [
            TableRow::new(vec!["one"]),
            TableRow::multiline(vec![vec![Line::from("two"), Line::from("more")]]),
            TableRow::new(vec!["three"]),
        ];
        let state = RefCell::new(TableState::new(None));
        let component = TableComponent::new("grid", &columns, &rows, &state)
            .policy(TablePolicy::interactive().vertical_scrollbar(ScrollbarAxisLayoutMode::Gutter));
        let layout = component.layout(
            Constraints::tight(Rect::new(0, 0, 7, 3).size()),
            &mut LayoutCx::new(),
        );

        let viewport = layout
            .find(&LayoutId::new("grid.viewport"))
            .expect("shared viewport node");
        assert_eq!(
            viewport.size,
            LogicalSize::new(6, 2),
            "body minus header and gutter"
        );
        let content = layout
            .find(&LayoutId::new("grid.content"))
            .expect("measured body content");
        assert_eq!(content.size.height, 4, "exact sum of row heights");
        assert_eq!(
            layout
                .find_logical_rect(&LayoutId::new("grid.viewport"))
                .map(|rect| rect.y),
            Some(1),
            "viewport starts below the header"
        );
        assert_eq!(ScrollView::max_vertical_offset(viewport), 2);
    }

    #[test]
    fn wheel_and_page_scrolling_route_through_shared_scroll_view_and_clamp() {
        let columns = [TableColumn::new("Name").fixed(6)];
        let rows = [
            TableRow::new(vec!["one"]),
            TableRow::new(vec!["two"]),
            TableRow::new(vec!["three"]),
            TableRow::new(vec!["four"]),
        ];
        let table = Table::new(&columns, &rows).policy(TablePolicy {
            auto_scroll_selected: false,
            ..TablePolicy::interactive()
        });
        let area = Rect::new(0, 0, 6, 3);
        let mut state = TableState::new(None);

        let wheel = |kind| Event::Mouse(MouseEvent::new(kind, Point::new(1, 1)));
        assert_eq!(
            table.handle_event(area, &mut state, &wheel(MouseEventKind::ScrollDown)),
            TableOutcome::Redraw
        );
        assert_eq!(state.scroll(), 1);
        assert_eq!(
            table.handle_event(area, &mut state, &wheel(MouseEventKind::ScrollDown)),
            TableOutcome::Redraw
        );
        assert_eq!(state.scroll(), 2, "clamped to the final visible page");
        assert_eq!(
            table.handle_event(area, &mut state, &wheel(MouseEventKind::ScrollDown)),
            TableOutcome::Ignored,
            "scrolling past the end is a no-op"
        );
        assert_eq!(
            table.handle_event(area, &mut state, &wheel(MouseEventKind::ScrollUp)),
            TableOutcome::Redraw
        );
        assert_eq!(state.scroll(), 1);

        let mut oversized = TableState::new(None);
        oversized.set_scroll(99);
        table.reconcile(area, &mut oversized);
        assert_eq!(oversized.scroll(), 2, "stale offsets clamp to the layout");

        let mut horizontal = TableState::new(None);
        horizontal.set_horizontal_scroll(99);
        table.reconcile(area, &mut horizontal);
        assert_eq!(
            horizontal.horizontal_scroll(),
            0,
            "no horizontal overflow means no horizontal offset"
        );
    }

    #[test]
    fn horizontal_wheel_scrolls_columns_and_clamps_to_content_width() {
        let columns = [
            TableColumn::new("A").fixed(4),
            TableColumn::new("B").fixed(4),
        ];
        let rows = [TableRow::new(vec!["abcd", "efgh"])];
        let table = Table::new(&columns, &rows).policy(TablePolicy {
            header: false,
            ..TablePolicy::interactive()
        });
        let area = Rect::new(0, 0, 5, 1);
        let mut state = TableState::new(None);

        for _ in 0..6 {
            let _ = table.handle_event(
                area,
                &mut state,
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::ScrollRight,
                    Point::new(1, 0),
                )),
            );
        }
        assert_eq!(
            state.horizontal_scroll(),
            4,
            "content width 9 minus viewport 5"
        );

        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 1));
        let mut frame = Frame::new(&mut buffer);
        table.render(area, &state, &mut frame);
        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some(" efgh"));
    }

    #[test]
    fn auto_scroll_reconciles_selected_row_without_mutating_caller_offset_on_paint() {
        let columns = [TableColumn::new("Name").fixed(6)];
        let rows = [
            TableRow::new(vec!["one"]),
            TableRow::new(vec!["two"]),
            TableRow::new(vec!["three"]),
        ];
        let table = Table::new(&columns, &rows);
        let area = Rect::new(0, 0, 6, 2);
        let state = TableState::new(Some(2));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 2));
        let mut frame = Frame::new(&mut buffer);

        table.render(area, &state, &mut frame);

        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("three "));
        assert_eq!(state.scroll(), 0, "painting never mutates caller state");
        assert_eq!(
            table.row_hit_regions(area, &state)[0].key,
            2,
            "hit regions agree with the painted offset"
        );
    }
}
