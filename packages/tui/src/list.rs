//! List and picker widgets.

use bmux_keyboard::{KeyCode, KeyStroke};

use crate::frame::Frame;
use crate::geometry::Rect;
use crate::hit::{HitId, HitMap, HitRegion, HitRole};
use crate::style::Style;
use crate::text::Line;

/// A list item rendered as one styled line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListItem {
    line: Line,
}

impl ListItem {
    /// Create a list item from a line.
    #[must_use]
    pub fn new(line: impl Into<Line>) -> Self {
        Self { line: line.into() }
    }

    /// Return the rendered line.
    #[must_use]
    pub const fn line(&self) -> &Line {
        &self.line
    }
}

/// Scroll and selection state for [`List`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ListState {
    /// Selected item index, if any.
    pub selected: Option<usize>,
    /// First visible item index.
    pub offset: usize,
}

impl ListState {
    /// Create empty list state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            selected: None,
            offset: 0,
        }
    }

    /// Select an item by index.
    pub const fn select(&mut self, index: Option<usize>) {
        self.selected = index;
    }

    /// Move selection down by one item.
    pub fn select_next(&mut self, item_count: usize) {
        if item_count == 0 {
            self.selected = None;
            self.offset = 0;
            return;
        }
        self.selected = Some(
            self.selected
                .map_or(0, |selected| selected.saturating_add(1).min(item_count - 1)),
        );
    }

    /// Move selection up by one item.
    pub fn select_previous(&mut self, item_count: usize) {
        if item_count == 0 {
            self.selected = None;
            self.offset = 0;
            return;
        }
        self.selected = Some(
            self.selected
                .map_or(0, |selected| selected.saturating_sub(1)),
        );
    }

    /// Adjust offset so the selection is visible in a viewport of `height` rows.
    pub fn ensure_selected_visible(&mut self, height: u16, item_count: usize) {
        if item_count == 0 {
            self.selected = None;
            self.offset = 0;
            return;
        }
        let height = usize::from(height.max(1));
        let selected = self.selected.unwrap_or(0).min(item_count - 1);
        self.selected = Some(selected);
        if selected < self.offset {
            self.offset = selected;
        } else if selected >= self.offset.saturating_add(height) {
            self.offset = selected.saturating_add(1).saturating_sub(height);
        }
        self.offset = self.offset.min(item_count.saturating_sub(1));
    }
}

/// Result of handling a key stroke for a list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListKeyOutcome {
    /// The key was not recognized as list input.
    Ignored,
    /// Selection or scroll position changed.
    Moved,
    /// The selected item was activated.
    Activated,
    /// The list interaction was canceled.
    Canceled,
}

/// Key handling policy for selectable lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ListKeyHandler;

impl ListKeyHandler {
    /// Apply a key stroke to list state.
    pub fn handle_key(
        self,
        state: &mut ListState,
        item_count: usize,
        viewport_height: u16,
        stroke: KeyStroke,
    ) -> ListKeyOutcome {
        if !stroke.modifiers.is_empty() {
            return ListKeyOutcome::Ignored;
        }
        match stroke.key {
            KeyCode::Up => {
                state.select_previous(item_count);
                state.ensure_selected_visible(viewport_height, item_count);
                ListKeyOutcome::Moved
            }
            KeyCode::Down => {
                state.select_next(item_count);
                state.ensure_selected_visible(viewport_height, item_count);
                ListKeyOutcome::Moved
            }
            KeyCode::Home => {
                state.select(if item_count == 0 { None } else { Some(0) });
                state.ensure_selected_visible(viewport_height, item_count);
                ListKeyOutcome::Moved
            }
            KeyCode::End => {
                state.select(item_count.checked_sub(1));
                state.ensure_selected_visible(viewport_height, item_count);
                ListKeyOutcome::Moved
            }
            KeyCode::PageUp => {
                move_selection_by_page(state, item_count, viewport_height, PageDirection::Up);
                ListKeyOutcome::Moved
            }
            KeyCode::PageDown => {
                move_selection_by_page(state, item_count, viewport_height, PageDirection::Down);
                ListKeyOutcome::Moved
            }
            KeyCode::Enter => {
                if state.selected.is_some() {
                    ListKeyOutcome::Activated
                } else {
                    ListKeyOutcome::Ignored
                }
            }
            KeyCode::Escape => ListKeyOutcome::Canceled,
            KeyCode::Char(_)
            | KeyCode::Tab
            | KeyCode::Backspace
            | KeyCode::Delete
            | KeyCode::Space
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Insert
            | KeyCode::F(_) => ListKeyOutcome::Ignored,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageDirection {
    Up,
    Down,
}

fn move_selection_by_page(
    state: &mut ListState,
    item_count: usize,
    viewport_height: u16,
    direction: PageDirection,
) {
    if item_count == 0 {
        state.selected = None;
        state.offset = 0;
        return;
    }
    let page = usize::from(viewport_height.max(1));
    let selected = state.selected.unwrap_or(0).min(item_count - 1);
    let next = match direction {
        PageDirection::Up => selected.saturating_sub(page),
        PageDirection::Down => selected.saturating_add(page).min(item_count - 1),
    };
    state.select(Some(next));
    state.ensure_selected_visible(viewport_height, item_count);
}

/// A virtualized single-line list widget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct List<'items> {
    items: &'items [ListItem],
    style: Style,
    selected_style: Style,
    highlight_symbol: Option<String>,
}

impl<'items> List<'items> {
    /// Create a list from items.
    #[must_use]
    pub const fn new(items: &'items [ListItem]) -> Self {
        Self {
            items,
            style: Style::new(),
            selected_style: Style::new().add_modifier(crate::style::Modifier::REVERSED),
            highlight_symbol: None,
        }
    }

    /// Set base row style. This style is used to fill each rendered row and as
    /// a fallback behind item span styles.
    #[must_use]
    pub const fn style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    /// Set selected item style. This style patches over item span styles.
    #[must_use]
    pub const fn selected_style(mut self, style: Style) -> Self {
        self.selected_style = style;
        self
    }

    /// Set an optional highlight symbol rendered before the selected item.
    #[must_use]
    pub fn highlight_symbol(mut self, symbol: impl Into<String>) -> Self {
        self.highlight_symbol = Some(symbol.into());
        self
    }

    /// Register visible row hit regions for this list.
    pub fn register_hits(&self, area: Rect, state: &ListState, hits: &mut HitMap, id_prefix: &str) {
        if area.is_empty() {
            return;
        }
        let visible_count = usize::from(area.height);
        for (row, index) in (state.offset..self.items.len())
            .take(visible_count)
            .enumerate()
        {
            let Ok(row) = u16::try_from(row) else {
                return;
            };
            hits.push(
                HitRegion::new(
                    HitId::new(format!("{id_prefix}:{index}")),
                    Rect::new(area.x, area.y.saturating_add(row), area.width, 1),
                )
                .role(HitRole::ListItem),
            );
        }
    }

    /// Resolve a hit id generated by [`Self::register_hits`] into an item index.
    #[must_use]
    pub fn hit_item_index(id: &HitId, id_prefix: &str) -> Option<usize> {
        id.as_str()
            .strip_prefix(id_prefix)?
            .strip_prefix(':')?
            .parse()
            .ok()
    }

    /// Return this list's items.
    #[must_use]
    pub const fn items(&self) -> &[ListItem] {
        self.items
    }
}

impl crate::widget::StatefulWidget for List<'_> {
    type State = ListState;

    fn render(&self, area: Rect, frame: &mut Frame<'_>, state: &mut Self::State) {
        if area.is_empty() {
            return;
        }
        state.ensure_selected_visible(area.height, self.items.len());
        let visible_count = usize::from(area.height);
        for (row, (index, item)) in self
            .items
            .iter()
            .enumerate()
            .skip(state.offset)
            .take(visible_count)
            .enumerate()
        {
            let Ok(row) = u16::try_from(row) else {
                return;
            };
            let selected = state.selected == Some(index);
            let line = self.render_item_line(item, selected);
            frame.write_line_with_fallback_style(
                Rect::new(area.x, area.y.saturating_add(row), area.width, 1),
                &line,
                if selected {
                    self.selected_style
                } else {
                    self.style
                },
            );
        }
    }
}

impl List<'_> {
    fn render_item_line(&self, item: &ListItem, selected: bool) -> Line {
        let style = if selected {
            self.selected_style
        } else {
            self.style
        };
        let line = item.line().with_fallback_style(style);
        if selected && let Some(symbol) = &self.highlight_symbol {
            let mut spans = vec![crate::text::Span::styled(
                symbol.clone(),
                self.selected_style,
            )];
            spans.extend(line.spans);
            return Line::from_spans(spans);
        }
        line
    }
}

#[cfg(test)]
mod tests {
    use super::{List, ListItem, ListKeyHandler, ListKeyOutcome, ListState};
    use crate::buffer::Buffer;
    use crate::frame::Frame;
    use crate::geometry::Rect;
    use crate::style::{Color, Modifier, Style};
    use crate::widget::StatefulWidget;
    use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};

    #[test]
    fn list_renders_visible_window_and_selection() {
        let items = vec![
            ListItem::new("one"),
            ListItem::new("two"),
            ListItem::new("three"),
            ListItem::new("four"),
        ];
        let mut state = ListState {
            selected: Some(2),
            offset: 0,
        };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 7, 2));
        let mut frame = Frame::new(&mut buffer);
        let selected_style = Style::new().add_modifier(Modifier::REVERSED);

        List::new(&items).selected_style(selected_style).render(
            Rect::new(0, 0, 7, 2),
            &mut frame,
            &mut state,
        );

        assert_eq!(state.offset, 1);
        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("two    "));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("three  "));
        assert_eq!(
            frame
                .buffer()
                .get(crate::geometry::Point::new(0, 1))
                .map(|cell| cell.style),
            Some(selected_style)
        );
    }

    #[test]
    fn list_base_style_fills_unselected_rows() {
        let items = vec![ListItem::new("one"), ListItem::new("two")];
        let mut state = ListState {
            selected: Some(1),
            offset: 0,
        };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 2));
        let mut frame = Frame::new(&mut buffer);
        let style = Style::new().fg(Color::White).bg(Color::Black);

        List::new(&items)
            .style(style)
            .render(Rect::new(0, 0, 5, 2), &mut frame, &mut state);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("one  "));
        assert_eq!(
            frame
                .buffer()
                .get(crate::geometry::Point::new(4, 0))
                .map(|cell| cell.style),
            Some(style)
        );
    }

    #[test]
    fn list_state_moves_selection_with_bounds() {
        let mut state = ListState::new();

        state.select_next(3);
        assert_eq!(state.selected, Some(0));
        state.select_next(3);
        state.select_next(3);
        state.select_next(3);
        assert_eq!(state.selected, Some(2));
        state.select_previous(3);
        assert_eq!(state.selected, Some(1));
        state.select_previous(0);
        assert_eq!(state.selected, None);
    }

    #[test]
    fn list_renders_highlight_symbol_for_selected_item() {
        let items = vec![ListItem::new("one"), ListItem::new("two")];
        let mut state = ListState {
            selected: Some(0),
            offset: 0,
        };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 1));
        let mut frame = Frame::new(&mut buffer);

        List::new(&items).highlight_symbol("> ").render(
            Rect::new(0, 0, 6, 1),
            &mut frame,
            &mut state,
        );

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("> one "));
    }

    #[test]
    fn list_registers_visible_row_hit_regions() {
        let items = vec![
            ListItem::new("one"),
            ListItem::new("two"),
            ListItem::new("three"),
            ListItem::new("four"),
        ];
        let list = List::new(&items);
        let state = ListState {
            selected: Some(2),
            offset: 1,
        };
        let mut hits = crate::hit::HitMap::new();

        list.register_hits(Rect::new(5, 2, 10, 2), &state, &mut hits, "files");

        assert_eq!(hits.regions().len(), 2);
        let hit = hits
            .hit_test(crate::geometry::Point::new(6, 3))
            .expect("second visible row should be hittable");
        assert_eq!(hit.id().as_str(), "files:2");
        assert_eq!(hit.role(), crate::hit::HitRole::ListItem);
        assert_eq!(List::hit_item_index(hit.id(), "files"), Some(2));
    }

    #[test]
    fn list_hit_item_index_rejects_other_prefixes() {
        assert_eq!(
            List::hit_item_index(&crate::hit::HitId::new("other:7"), "files"),
            None
        );
        assert_eq!(
            List::hit_item_index(&crate::hit::HitId::new("files:not-number"), "files"),
            None
        );
    }

    #[test]
    fn list_key_handler_moves_and_pages_selection() {
        let mut state = ListState::new();
        let handler = ListKeyHandler;

        assert_eq!(
            handler.handle_key(&mut state, 10, 3, KeyStroke::simple(KeyCode::Down)),
            ListKeyOutcome::Moved
        );
        assert_eq!(state.selected, Some(0));
        assert_eq!(
            handler.handle_key(&mut state, 10, 3, KeyStroke::simple(KeyCode::PageDown)),
            ListKeyOutcome::Moved
        );
        assert_eq!(state.selected, Some(3));
        assert_eq!(state.offset, 1);
        assert_eq!(
            handler.handle_key(&mut state, 10, 3, KeyStroke::simple(KeyCode::PageUp)),
            ListKeyOutcome::Moved
        );
        assert_eq!(state.selected, Some(0));
        assert_eq!(state.offset, 0);
    }

    #[test]
    fn list_key_handler_supports_home_end_activate_and_cancel() {
        let mut state = ListState {
            selected: Some(1),
            offset: 0,
        };
        let handler = ListKeyHandler;

        assert_eq!(
            handler.handle_key(&mut state, 4, 2, KeyStroke::simple(KeyCode::End)),
            ListKeyOutcome::Moved
        );
        assert_eq!(state.selected, Some(3));
        assert_eq!(
            handler.handle_key(&mut state, 4, 2, KeyStroke::simple(KeyCode::Home)),
            ListKeyOutcome::Moved
        );
        assert_eq!(state.selected, Some(0));
        assert_eq!(
            handler.handle_key(&mut state, 4, 2, KeyStroke::simple(KeyCode::Enter)),
            ListKeyOutcome::Activated
        );
        assert_eq!(
            handler.handle_key(&mut state, 4, 2, KeyStroke::simple(KeyCode::Escape)),
            ListKeyOutcome::Canceled
        );
    }

    #[test]
    fn list_key_handler_ignores_modified_and_unmapped_keys() {
        let mut state = ListState::new();
        let handler = ListKeyHandler;

        assert_eq!(
            handler.handle_key(
                &mut state,
                4,
                2,
                KeyStroke::with_modifiers(
                    KeyCode::Down,
                    Modifiers {
                        shift: true,
                        ..Modifiers::NONE
                    },
                ),
            ),
            ListKeyOutcome::Ignored
        );
        assert_eq!(
            handler.handle_key(&mut state, 4, 2, KeyStroke::simple(KeyCode::Char('x'))),
            ListKeyOutcome::Ignored
        );
    }
}
