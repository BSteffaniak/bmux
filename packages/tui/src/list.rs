//! List and picker widgets.

use bmux_keyboard::{KeyCode, KeyStroke};

use crate::frame::Frame;
use crate::geometry::Rect;
use crate::hit::{HitId, HitMap, HitRegion, HitRole};
use crate::style::Style;
use crate::text::Line;

use crate::widgets::line_with_fallback_style;

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
    selected_style: Style,
    highlight_symbol: Option<String>,
}

impl<'items> List<'items> {
    /// Create a list from items.
    #[must_use]
    pub const fn new(items: &'items [ListItem]) -> Self {
        Self {
            items,
            selected_style: Style::new().add_modifier(crate::style::Modifier::REVERSED),
            highlight_symbol: None,
        }
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
            frame.write_line(
                Rect::new(area.x, area.y.saturating_add(row), area.width, 1),
                &line,
            );
        }
    }
}

impl List<'_> {
    fn render_item_line(&self, item: &ListItem, selected: bool) -> Line {
        let line = if selected {
            line_with_fallback_style(item.line(), self.selected_style)
        } else {
            item.line().clone()
        };
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
