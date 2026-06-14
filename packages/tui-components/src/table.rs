//! Generic table display and selectable table component.

use bmux_keyboard::KeyCode;
use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::prelude::{Line, Span};
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui::text_width::{display_width, truncate_to_display_width};

use crate::common::{ComponentMousePolicy, InteractionState};

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
}

/// One table row with caller-owned cell content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRow {
    /// Row cells.
    pub cells: Vec<Line>,
    /// Whether this row cannot be selected.
    pub disabled: bool,
}

impl TableRow {
    /// Create a row from borrowed cell labels.
    #[must_use]
    pub fn new<'a>(cells: impl Into<Vec<&'a str>>) -> Self {
        Self {
            cells: cells.into().into_iter().map(Line::from).collect(),
            disabled: false,
        }
    }

    /// Create a row from rich cell content.
    #[must_use]
    pub fn rich(cells: impl Into<Vec<Line>>) -> Self {
        Self {
            cells: cells.into(),
            disabled: false,
        }
    }

    /// Return this row with disabled state set.
    #[must_use]
    pub const fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

/// Runtime selectable table state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TableState {
    selected: Option<usize>,
    hovered: Option<usize>,
    scroll: usize,
    /// Generic interaction flags.
    pub interaction: InteractionState,
}

impl TableState {
    /// Create table state with a selected row.
    #[must_use]
    pub const fn new(selected: Option<usize>) -> Self {
        Self {
            selected,
            hovered: None,
            scroll: 0,
            interaction: InteractionState::new(),
        }
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

    /// Return row scroll offset.
    #[must_use]
    pub const fn scroll(&self) -> usize {
        self.scroll
    }

    /// Set row scroll offset.
    pub const fn set_scroll(&mut self, scroll: usize) {
        self.scroll = scroll;
    }
}

/// Table behavior policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct TablePolicy {
    /// Render header row.
    pub header: bool,
    /// Render separator row after header.
    pub row_separator: bool,
    /// Keyboard row navigation enabled.
    pub keyboard: bool,
    /// Mouse behavior.
    pub mouse: ComponentMousePolicy,
    /// Keep selected row visible while rendering.
    pub auto_scroll_selected: bool,
    /// Separator between cells.
    pub cell_separator: &'static str,
}

impl TablePolicy {
    /// Render-only table.
    #[must_use]
    pub const fn bare() -> Self {
        Self {
            header: true,
            row_separator: false,
            keyboard: false,
            mouse: ComponentMousePolicy::disabled(),
            auto_scroll_selected: false,
            cell_separator: " ",
        }
    }

    /// Interactive selectable table.
    #[must_use]
    pub const fn interactive() -> Self {
        Self {
            header: true,
            row_separator: false,
            keyboard: true,
            mouse: ComponentMousePolicy::button(),
            auto_scroll_selected: true,
            cell_separator: " ",
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
    /// Hovered row style.
    pub hovered: Style,
    /// Disabled row style.
    pub disabled: Style,
    /// Separator style.
    pub separator: Style,
    /// Empty table style.
    pub empty: Style,
}

impl Default for TableStyles {
    fn default() -> Self {
        Self {
            header: Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            row: Style::new().fg(Color::White),
            selected: Style::new().fg(Color::Black).bg(Color::Cyan),
            hovered: Style::new().fg(Color::BrightWhite),
            disabled: Style::new().fg(Color::BrightBlack),
            separator: Style::new().fg(Color::BrightBlack),
            empty: Style::new().fg(Color::BrightBlack),
        }
    }
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableLayout {
    /// Resolved column widths.
    pub column_widths: Vec<u16>,
    /// Header area if visible.
    pub header: Option<Rect>,
    /// Body area.
    pub body: Rect,
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
                row_separator: false,
                keyboard: true,
                mouse: ComponentMousePolicy {
                    enabled: true,
                    hover: true,
                    click: true,
                },
                auto_scroll_selected: true,
                cell_separator: " ",
            },
            styles: TableStyles {
                header: Style::new(),
                row: Style::new(),
                selected: Style::new(),
                hovered: Style::new(),
                disabled: Style::new(),
                separator: Style::new(),
                empty: Style::new(),
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

    /// Compute table layout.
    #[must_use]
    pub fn layout(&self, area: Rect) -> TableLayout {
        let header_rows = u16::from(self.policy.header && area.height > 0);
        let header = (header_rows > 0).then_some(Rect::new(area.x, area.y, area.width, 1));
        let body_y = area.y.saturating_add(header_rows);
        let body_height = area.height.saturating_sub(header_rows);
        TableLayout {
            column_widths: resolve_column_widths(
                self.columns,
                area.width,
                self.policy.cell_separator,
            ),
            header,
            body: Rect::new(area.x, body_y, area.width, body_height),
        }
    }

    /// Render table.
    pub fn render(&self, area: Rect, state: &TableState, frame: &mut Frame<'_>) {
        if area.is_empty() {
            return;
        }
        let layout = self.layout(area);
        if let Some(header) = layout.header {
            frame.write_line_with_fallback_style(
                header,
                &self.row_line(
                    &layout.column_widths,
                    self.columns.iter().map(|column| Line::from(column.title)),
                    true,
                ),
                self.styles.header,
            );
        }
        if self.rows.is_empty() {
            frame.write_line_with_fallback_style(
                layout.body,
                &Line::from(self.empty),
                self.styles.empty,
            );
            return;
        }
        let scroll = self.effective_scroll(state, layout.body.height);
        for (visible, source) in (scroll..self.rows.len())
            .take(usize::from(layout.body.height))
            .enumerate()
        {
            let y = layout.body.y.saturating_add(u16_saturating(visible));
            let rect = Rect::new(layout.body.x, y, layout.body.width, 1);
            let row = &self.rows[source];
            frame.write_line_with_fallback_style(
                rect,
                &self.row_line(&layout.column_widths, row.cells.iter().cloned(), false),
                self.row_style(source, row, state),
            );
        }
    }

    /// Handle one event.
    pub fn handle_event(&self, area: Rect, state: &mut TableState, event: &Event) -> TableOutcome {
        if state.interaction.disabled {
            return TableOutcome::Ignored;
        }
        match event {
            Event::Key(stroke) if self.policy.keyboard => match stroke.key {
                KeyCode::Up => self.move_selection(state, -1),
                KeyCode::Down => self.move_selection(state, 1),
                KeyCode::Home => self.select_index(state, 0),
                KeyCode::End => self.select_index(state, self.rows.len().saturating_sub(1)),
                KeyCode::Enter => state
                    .selected
                    .map_or(TableOutcome::Ignored, TableOutcome::Selected),
                _ => TableOutcome::Ignored,
            },
            Event::Mouse(mouse) if self.policy.mouse.enabled => {
                self.handle_mouse(area, state, *mouse)
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

    fn handle_mouse(&self, area: Rect, state: &mut TableState, mouse: MouseEvent) -> TableOutcome {
        match mouse.kind {
            MouseEventKind::Move if self.policy.mouse.hover => {
                let hovered = self.row_at(area, state, mouse.position);
                if hovered == state.hovered {
                    TableOutcome::Ignored
                } else {
                    state.hovered = hovered;
                    TableOutcome::Redraw
                }
            }
            MouseEventKind::Down(MouseButton::Left) if self.policy.mouse.click => self
                .row_at(area, state, mouse.position)
                .map_or(TableOutcome::Ignored, |row| self.select_index(state, row)),
            MouseEventKind::ScrollDown => {
                state.scroll = state
                    .scroll
                    .saturating_add(1)
                    .min(self.rows.len().saturating_sub(1));
                TableOutcome::Redraw
            }
            MouseEventKind::ScrollUp => {
                state.scroll = state.scroll.saturating_sub(1);
                TableOutcome::Redraw
            }
            MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::Move
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => TableOutcome::Ignored,
        }
    }

    fn row_at(&self, area: Rect, state: &TableState, position: Point) -> Option<usize> {
        let layout = self.layout(area);
        if !layout.body.contains(position) {
            return None;
        }
        let visible = usize::from(position.y.saturating_sub(layout.body.y));
        let source = self
            .effective_scroll(state, layout.body.height)
            .saturating_add(visible);
        (source < self.rows.len()).then_some(source)
    }

    fn move_selection(&self, state: &mut TableState, delta: i32) -> TableOutcome {
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
        self.select_index(state, next)
    }

    fn select_index(&self, state: &mut TableState, index: usize) -> TableOutcome {
        if self.rows.get(index).is_none_or(|row| row.disabled) {
            return TableOutcome::Ignored;
        }
        state.selected = Some(index);
        TableOutcome::Focused(index)
    }

    fn effective_scroll(&self, state: &TableState, body_height: u16) -> usize {
        if !self.policy.auto_scroll_selected || body_height == 0 {
            return state.scroll.min(self.rows.len().saturating_sub(1));
        }
        let Some(selected) = state.selected else {
            return state.scroll.min(self.rows.len().saturating_sub(1));
        };
        let height = usize::from(body_height);
        if selected < state.scroll {
            selected
        } else if selected >= state.scroll.saturating_add(height) {
            selected.saturating_sub(height.saturating_sub(1))
        } else {
            state.scroll
        }
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

    fn row_line(&self, widths: &[u16], cells: impl Iterator<Item = Line>, header: bool) -> Line {
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
    let plain = line.plain_text();
    Line::from(format_cell(&plain, u16_saturating(width), align, truncate))
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

fn format_cell(text: &str, width: u16, align: TableAlign, truncate: bool) -> String {
    let width = usize::from(width);
    if width == 0 {
        return String::new();
    }
    let text = if truncate && display_width(text) > width {
        truncate_to_display_width(text, width)
    } else {
        text.to_owned()
    };
    let text_width = display_width(&text);
    if text_width >= width {
        return text;
    }
    let padding = width.saturating_sub(text_width);
    match align {
        TableAlign::Left => format!("{text}{}", " ".repeat(padding)),
        TableAlign::Center => {
            let left = padding / 2;
            let right = padding.saturating_sub(left);
            format!("{}{}{}", " ".repeat(left), text, " ".repeat(right))
        }
        TableAlign::Right => format!("{}{text}", " ".repeat(padding)),
    }
}

fn u16_saturating(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use bmux_keyboard::{KeyCode, KeyStroke};
    use bmux_tui::buffer::Buffer;
    use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};
    use bmux_tui::prelude::{Line, Span};
    use bmux_tui::style::{Color, Style};

    use super::{
        Table, TableAlign, TableColumn, TableOutcome, TablePolicy, TableRow, TableState,
        TableStyles, format_cell,
    };

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
}
