//! Command palette / filtered picker primitives.

use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_text_edit::TextEditBuffer;

use crate::chrome::{Border, Panel};
use crate::frame::Frame;
use crate::geometry::Rect;
use crate::hit::{HitId, HitMap, HitRegion, HitRole};
use crate::input::{TextInput, TextInputEnterBehavior, TextInputKeyHandler, TextInputKeyOutcome};
use crate::layout::{Direction, split_leading};
use crate::list::{List, ListItem, ListKeyHandler, ListKeyOutcome, ListState};
use crate::style::Style;
use crate::text::Line;
use crate::widget::{StatefulWidget, Widget};

/// One command palette item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteItem {
    /// Stable caller-owned item id.
    pub id: String,
    /// Rendered item label.
    pub label: Line,
    /// Additional searchable text.
    pub search_text: String,
}

impl PaletteItem {
    /// Create a palette item.
    #[must_use]
    pub fn new(id: impl Into<String>, label: impl Into<Line>) -> Self {
        let label = label.into();
        let search_text = label.plain_text();
        Self {
            id: id.into(),
            label,
            search_text,
        }
    }

    /// Set additional searchable text.
    #[must_use]
    pub fn search_text(mut self, search_text: impl Into<String>) -> Self {
        self.search_text = search_text.into();
        self
    }

    fn matches(&self, query: &str) -> bool {
        if query.is_empty() {
            return true;
        }
        let query = query.to_ascii_lowercase();
        self.label
            .plain_text()
            .to_ascii_lowercase()
            .contains(&query)
            || self.search_text.to_ascii_lowercase().contains(&query)
    }
}

/// Command palette mutable state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandPaletteState {
    /// Query buffer.
    pub query: TextEditBuffer,
    /// Filtered-list state.
    pub list: ListState,
}

/// Result of handling a palette key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandPaletteKeyOutcome {
    /// Key was ignored.
    Ignored,
    /// Query changed.
    QueryEdited,
    /// Selection moved.
    SelectionMoved,
    /// Item was activated by source item index.
    Activated(usize),
    /// Palette was canceled.
    Canceled,
}

/// A filtered command palette widget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandPalette<'items> {
    items: &'items [PaletteItem],
    panel: Panel,
    empty: Line,
    input_height: u16,
    gap: u16,
    placeholder: Line,
    list_style: Style,
    selected_style: Style,
}

impl<'items> CommandPalette<'items> {
    /// Create a command palette from items.
    #[must_use]
    pub fn new(items: &'items [PaletteItem]) -> Self {
        Self {
            items,
            panel: Panel::new().border(Border::single()),
            empty: Line::raw("No matches"),
            input_height: 1,
            gap: 1,
            placeholder: Line::raw("Search"),
            list_style: Style::new(),
            selected_style: Style::new().add_modifier(crate::style::Modifier::REVERSED),
        }
    }

    /// Set panel chrome.
    #[must_use]
    pub fn panel(mut self, panel: Panel) -> Self {
        self.panel = panel;
        self
    }

    /// Set empty-state line.
    #[must_use]
    pub fn empty(mut self, empty: impl Into<Line>) -> Self {
        self.empty = empty.into();
        self
    }

    /// Set input placeholder text.
    #[must_use]
    pub fn placeholder(mut self, placeholder: impl Into<Line>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    /// Set base and selected list-row styles.
    #[must_use]
    pub const fn list_styles(mut self, style: Style, selected_style: Style) -> Self {
        self.list_style = style;
        self.selected_style = selected_style;
        self
    }

    /// Return source item indices matching a query.
    #[must_use]
    pub fn filtered_indices(&self, query: &str) -> Vec<usize> {
        self.items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| item.matches(query).then_some(index))
            .collect()
    }

    /// Render using a caller-provided source-index projection.
    ///
    /// This supports hosts with a domain-specific ranking algorithm while
    /// retaining this component's input, list, cursor, and viewport rendering.
    pub fn render_projected(
        &self,
        area: Rect,
        frame: &mut Frame<'_>,
        state: &mut CommandPaletteState,
        filtered: &[usize],
    ) {
        if area.is_empty() {
            return;
        }
        self.panel.render(area, frame);
        let areas = self.areas(area);
        TextInput::new(&state.query)
            .placeholder(self.placeholder.clone())
            .render(areas.input, frame);

        let valid = filtered
            .iter()
            .copied()
            .filter(|index| *index < self.items.len())
            .collect::<Vec<_>>();
        if valid.is_empty() {
            state.list.selected = None;
            state.list.offset = 0;
            frame.write_line(areas.list, &self.empty);
            return;
        }
        let items = valid
            .iter()
            .map(|index| ListItem::new(self.items[*index].label.clone()))
            .collect::<Vec<_>>();
        List::new(&items)
            .style(self.list_style)
            .selected_style(self.selected_style)
            .highlight_symbol("> ")
            .render(areas.list, frame, &mut state.list);
    }

    /// Handle a key for query/list palette interaction.
    pub fn handle_key(
        &self,
        state: &mut CommandPaletteState,
        viewport_height: u16,
        stroke: KeyStroke,
    ) -> CommandPaletteKeyOutcome {
        let filtered = self.filtered_indices(state.query.text());
        match stroke.key {
            KeyCode::Up
            | KeyCode::Down
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::PageUp
            | KeyCode::PageDown => {
                let outcome = ListKeyHandler.handle_key(
                    &mut state.list,
                    filtered.len(),
                    viewport_height,
                    stroke,
                );
                return match outcome {
                    ListKeyOutcome::Moved => CommandPaletteKeyOutcome::SelectionMoved,
                    ListKeyOutcome::Ignored => CommandPaletteKeyOutcome::Ignored,
                    ListKeyOutcome::Activated | ListKeyOutcome::Canceled => {
                        CommandPaletteKeyOutcome::Ignored
                    }
                };
            }
            KeyCode::Enter if stroke.modifiers.is_empty() => {
                let selected = state.list.selected.unwrap_or(0);
                return filtered.get(selected).copied().map_or(
                    CommandPaletteKeyOutcome::Ignored,
                    CommandPaletteKeyOutcome::Activated,
                );
            }
            KeyCode::Escape if stroke.modifiers.is_empty() => {
                return CommandPaletteKeyOutcome::Canceled;
            }
            _ => {}
        }

        let outcome = TextInputKeyHandler::new(
            bmux_text_edit::keyboard::TextKeymap::default(),
            TextInputEnterBehavior::Submit,
        )
        .handle_key(&mut state.query, stroke);
        match outcome {
            TextInputKeyOutcome::Edited => {
                state.list.offset = 0;
                state.list.selected = None;
                CommandPaletteKeyOutcome::QueryEdited
            }
            TextInputKeyOutcome::Submitted => filtered.first().copied().map_or(
                CommandPaletteKeyOutcome::Ignored,
                CommandPaletteKeyOutcome::Activated,
            ),
            TextInputKeyOutcome::Ignored => CommandPaletteKeyOutcome::Ignored,
        }
    }

    /// Register visible command-row hit regions.
    pub fn register_hits(
        &self,
        area: Rect,
        state: &CommandPaletteState,
        hits: &mut HitMap,
        id_prefix: &str,
    ) {
        if area.is_empty() {
            return;
        }
        let filtered = self.filtered_indices(state.query.text());
        self.register_projected_hits(area, state, &filtered, hits, id_prefix);
    }

    /// Register visible command-row hits for a caller-provided source projection.
    pub fn register_projected_hits(
        &self,
        area: Rect,
        state: &CommandPaletteState,
        filtered: &[usize],
        hits: &mut HitMap,
        id_prefix: &str,
    ) {
        if area.is_empty() {
            return;
        }
        let areas = self.areas(area);
        let visible_count = usize::from(areas.list.height);
        for (row, source_index) in filtered
            .iter()
            .copied()
            .filter(|index| *index < self.items.len())
            .skip(state.list.offset)
            .take(visible_count)
            .enumerate()
        {
            let Ok(row) = u16::try_from(row) else {
                return;
            };
            hits.push(
                HitRegion::new(
                    HitId::new(format!("{id_prefix}:{source_index}")),
                    Rect::new(
                        areas.list.x,
                        areas.list.y.saturating_add(row),
                        areas.list.width,
                        1,
                    ),
                )
                .role(HitRole::ListItem),
            );
        }
    }

    /// Resolve a hit id generated by [`Self::register_hits`] into a source item index.
    #[must_use]
    pub fn hit_item_index(id: &HitId, id_prefix: &str) -> Option<usize> {
        id.as_str()
            .strip_prefix(id_prefix)?
            .strip_prefix(':')?
            .parse()
            .ok()
    }

    const fn areas(&self, area: Rect) -> PaletteAreas {
        let inner = self.panel.inner_area(area);
        let input_split = split_leading(inner, Direction::Vertical, self.input_height);
        let list_split = split_leading(input_split.second, Direction::Vertical, self.gap);
        PaletteAreas {
            input: input_split.first,
            list: list_split.second,
        }
    }
}

impl StatefulWidget for CommandPalette<'_> {
    type State = CommandPaletteState;

    fn render(&self, area: Rect, frame: &mut Frame<'_>, state: &mut Self::State) {
        let filtered = self.filtered_indices(state.query.text());
        self.render_projected(area, frame, state, &filtered);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PaletteAreas {
    input: Rect,
    list: Rect,
}

#[cfg(test)]
mod tests {
    use super::{CommandPalette, CommandPaletteKeyOutcome, CommandPaletteState, PaletteItem};
    use bmux_keyboard::{KeyCode, KeyStroke};

    use crate::buffer::Buffer;
    use crate::chrome::{Border, Panel};
    use crate::frame::Frame;
    use crate::geometry::Rect;
    use crate::widget::StatefulWidget;

    fn items() -> Vec<PaletteItem> {
        vec![
            PaletteItem::new("open", "Open File").search_text("file picker"),
            PaletteItem::new("settings", "Open Settings"),
            PaletteItem::new("close", "Close Window"),
        ]
    }

    #[test]
    fn palette_filters_items() {
        let items = items();
        let palette = CommandPalette::new(&items);

        assert_eq!(palette.filtered_indices("open"), vec![0, 1]);
        assert_eq!(palette.filtered_indices("picker"), vec![0]);
        assert_eq!(palette.filtered_indices("missing"), Vec::<usize>::new());
    }

    #[test]
    fn palette_renders_query_and_filtered_list() {
        let items = items();
        let palette = CommandPalette::new(&items).panel(Panel::new().border(Border::ascii()));
        let mut state = CommandPaletteState::default();
        state.query.insert_str("settings");
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 5));
        let mut frame = Frame::new(&mut buffer);

        palette.render(Rect::new(0, 0, 20, 5), &mut frame, &mut state);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("+------------------+")
        );
        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("|settings          |")
        );
        assert_eq!(
            frame.buffer().row_symbols(3).as_deref(),
            Some("|> Open Settings   |")
        );
    }

    #[test]
    fn palette_renders_empty_state() {
        let items = items();
        let palette = CommandPalette::new(&items).empty("Nothing");
        let mut state = CommandPaletteState::default();
        state.query.insert_str("zzz");
        let mut buffer = Buffer::empty(Rect::new(0, 0, 16, 5));
        let mut frame = Frame::new(&mut buffer);

        palette.render(Rect::new(0, 0, 16, 5), &mut frame, &mut state);

        assert_eq!(
            frame.buffer().row_symbols(3).as_deref(),
            Some("│Nothing       │")
        );
    }

    #[test]
    fn palette_registers_filtered_row_hit_regions() {
        let items = items();
        let palette = CommandPalette::new(&items);
        let mut state = CommandPaletteState::default();
        state.query.insert_str("open");
        let mut hits = crate::hit::HitMap::new();

        palette.register_hits(Rect::new(0, 0, 20, 6), &state, &mut hits, "commands");

        assert_eq!(hits.regions().len(), 2);
        let hit = hits
            .hit_test(crate::geometry::Point::new(1, 4))
            .expect("second filtered command should be hittable");
        assert_eq!(hit.id().as_str(), "commands:1");
        assert_eq!(
            CommandPalette::hit_item_index(hit.id(), "commands"),
            Some(1)
        );
    }

    #[test]
    fn palette_hit_item_index_rejects_other_prefixes() {
        assert_eq!(
            CommandPalette::hit_item_index(&crate::hit::HitId::new("other:1"), "commands"),
            None
        );
        assert_eq!(
            CommandPalette::hit_item_index(&crate::hit::HitId::new("commands:nope"), "commands"),
            None
        );
    }

    #[test]
    fn palette_key_handling_edits_moves_and_activates() {
        let items = items();
        let palette = CommandPalette::new(&items);
        let mut state = CommandPaletteState::default();

        assert_eq!(
            palette.handle_key(&mut state, 5, KeyStroke::simple(KeyCode::Char('o'))),
            CommandPaletteKeyOutcome::QueryEdited
        );
        assert_eq!(state.query.text(), "o");
        assert_eq!(
            palette.handle_key(&mut state, 5, KeyStroke::simple(KeyCode::Down)),
            CommandPaletteKeyOutcome::SelectionMoved
        );
        assert_eq!(
            palette.handle_key(&mut state, 5, KeyStroke::simple(KeyCode::Enter)),
            CommandPaletteKeyOutcome::Activated(0)
        );
    }
}
