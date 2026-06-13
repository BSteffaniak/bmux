//! Configurable selectable-list component.

use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::Modifier;

use crate::common::{ComponentMousePolicy, InteractionState};

/// One selectable list item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectableListItem {
    /// Stable item id chosen by the caller.
    pub id: String,
    /// Visible item label.
    pub label: String,
    /// Whether this item is disabled independently from the whole list.
    pub disabled: bool,
}

impl SelectableListItem {
    /// Create an enabled selectable-list item.
    #[must_use]
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            disabled: false,
        }
    }

    /// Return this item with disabled state set.
    #[must_use]
    pub const fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

/// Visual styles for a selectable list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectableListStyles {
    /// Style used for enabled inactive items.
    pub normal: Style,
    /// Style used for the focused item.
    pub focused: Style,
    /// Style used for the selected item.
    pub selected: Style,
    /// Style used for the hovered item.
    pub hovered: Style,
    /// Style used while an item is pressed.
    pub pressed: Style,
    /// Style used for disabled items or lists.
    pub disabled: Style,
}

impl Default for SelectableListStyles {
    fn default() -> Self {
        Self {
            normal: Style::new(),
            focused: Style::new().add_modifier(Modifier::REVERSED),
            selected: Style::new().add_modifier(Modifier::BOLD),
            hovered: Style::new().add_modifier(Modifier::UNDERLINE),
            pressed: Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED),
            disabled: Style::new().add_modifier(Modifier::DIM),
        }
    }
}

/// Keyboard behavior for a selectable list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct SelectableListKeyboardPolicy {
    /// Whether arrow keys move focus between items.
    pub arrows_move_focus: bool,
    /// Whether Home and End move to the first/last enabled item.
    pub home_end_move_focus: bool,
    /// Whether focus wraps at list edges.
    pub wrap: bool,
    /// Whether Enter selects the focused item.
    pub enter_selects: bool,
    /// Whether Space selects the focused item.
    pub space_selects: bool,
}

impl SelectableListKeyboardPolicy {
    /// Common interactive keyboard behavior.
    #[must_use]
    pub const fn interactive() -> Self {
        Self {
            arrows_move_focus: true,
            home_end_move_focus: true,
            wrap: false,
            enter_selects: true,
            space_selects: true,
        }
    }
}

impl Default for SelectableListKeyboardPolicy {
    fn default() -> Self {
        Self::interactive()
    }
}

/// Configurable selectable-list behavior policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectableListPolicy {
    /// Mouse behavior.
    pub mouse: ComponentMousePolicy,
    /// Keyboard behavior.
    pub keyboard: SelectableListKeyboardPolicy,
}

impl SelectableListPolicy {
    /// Common interactive selectable-list behavior.
    #[must_use]
    pub const fn interactive() -> Self {
        Self {
            mouse: ComponentMousePolicy::button(),
            keyboard: SelectableListKeyboardPolicy::interactive(),
        }
    }
}

impl Default for SelectableListPolicy {
    fn default() -> Self {
        Self::interactive()
    }
}

/// Runtime selectable-list state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectableListState {
    /// Common list interaction flags.
    pub interaction: InteractionState,
    selected: Option<usize>,
    focused: Option<usize>,
    hovered: Option<usize>,
    pressed: Option<usize>,
}

impl SelectableListState {
    /// Create enabled selectable-list state.
    #[must_use]
    pub const fn new(selected: Option<usize>) -> Self {
        Self {
            interaction: InteractionState::new(),
            selected,
            focused: selected,
            hovered: None,
            pressed: None,
        }
    }

    /// Return selected item index.
    #[must_use]
    pub const fn selected(self) -> Option<usize> {
        self.selected
    }

    /// Set selected item index.
    pub const fn set_selected(&mut self, selected: Option<usize>) {
        self.selected = selected;
    }

    /// Return focused item index.
    #[must_use]
    pub const fn focused(self) -> Option<usize> {
        self.focused
    }

    /// Set focused item index.
    pub const fn set_focused(&mut self, focused: Option<usize>) {
        self.focused = focused;
        self.interaction.focused = focused.is_some();
    }

    /// Set disabled state for the whole list.
    pub const fn set_disabled(&mut self, disabled: bool) {
        self.interaction.disabled = disabled;
        if disabled {
            self.hovered = None;
            self.pressed = None;
        }
    }
}

/// Outcome from selectable-list input handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectableListOutcome {
    /// Event was not handled.
    Ignored,
    /// Visual state changed without changing selected value.
    Redraw,
    /// Focus moved to the contained item index.
    Focused(usize),
    /// Selection changed to the contained item index.
    Selected(usize),
}

/// Configurable vertical selectable-list control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectableList<'a> {
    items: &'a [SelectableListItem],
    policy: SelectableListPolicy,
    styles: SelectableListStyles,
}

impl<'a> SelectableList<'a> {
    /// Create a selectable list over caller-owned items.
    #[must_use]
    pub fn new(items: &'a [SelectableListItem]) -> Self {
        Self {
            items,
            policy: SelectableListPolicy::default(),
            styles: SelectableListStyles::default(),
        }
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: SelectableListPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: SelectableListStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Return required render size.
    #[must_use]
    pub fn size(&self) -> (u16, u16) {
        let width = self
            .items
            .iter()
            .map(|item| bmux_tui::text_width::display_width(&item.label))
            .max()
            .unwrap_or(0);

        (
            u16::try_from(width).unwrap_or(u16::MAX).saturating_add(2),
            u16::try_from(self.items.len()).unwrap_or(u16::MAX),
        )
    }

    /// Render the selectable list.
    pub fn render(&self, area: Rect, state: &SelectableListState, frame: &mut Frame<'_>) {
        self.render_with_fallback_style(area, state, frame, Style::new());
    }

    /// Render the selectable list with a fallback style filling each item row.
    pub fn render_with_fallback_style(
        &self,
        area: Rect,
        state: &SelectableListState,
        frame: &mut Frame<'_>,
        fallback: Style,
    ) {
        for (index, item) in self.items.iter().take(usize::from(area.height)).enumerate() {
            let row = area
                .y
                .saturating_add(u16::try_from(index).unwrap_or(u16::MAX));
            frame.write_line_with_fallback_style(
                Rect::new(area.x, row, area.width, 1),
                &self.line(index, item, *state),
                fallback,
            );
        }
    }

    /// Handle one input event.
    pub fn handle_event(
        &self,
        area: Rect,
        state: &mut SelectableListState,
        event: &Event,
    ) -> SelectableListOutcome {
        self.normalize_state(state);
        if state.interaction.disabled {
            return SelectableListOutcome::Ignored;
        }
        match event {
            Event::Key(stroke) => self.handle_key(state, *stroke),
            Event::Mouse(mouse) => self.handle_mouse(area, state, *mouse),
            Event::Resize(_) | Event::Paste(_) | Event::Focus(_) | Event::Tick | Event::User(_) => {
                SelectableListOutcome::Ignored
            }
        }
    }

    fn line(&self, index: usize, item: &SelectableListItem, state: SelectableListState) -> Line {
        let marker = if state.selected == Some(index) {
            '>'
        } else {
            ' '
        };
        Line::from_spans(vec![Span::styled(
            format!("{marker} {}", item.label),
            self.style_for(index, item, state),
        )])
    }

    fn style_for(
        &self,
        index: usize,
        item: &SelectableListItem,
        state: SelectableListState,
    ) -> Style {
        if state.interaction.disabled || item.disabled {
            self.styles.disabled
        } else if state.pressed == Some(index) {
            self.styles.pressed
        } else if state.focused == Some(index) {
            self.styles.focused
        } else if state.hovered == Some(index) {
            self.styles.hovered
        } else if state.selected == Some(index) {
            self.styles.selected
        } else {
            self.styles.normal
        }
    }

    fn handle_key(
        &self,
        state: &mut SelectableListState,
        stroke: KeyStroke,
    ) -> SelectableListOutcome {
        if state.focused.is_none() || !stroke.modifiers.is_empty() {
            return SelectableListOutcome::Ignored;
        }
        match stroke.key {
            KeyCode::Up if self.policy.keyboard.arrows_move_focus => {
                self.move_focus(state, Direction::Previous)
            }
            KeyCode::Down if self.policy.keyboard.arrows_move_focus => {
                self.move_focus(state, Direction::Next)
            }
            KeyCode::Home if self.policy.keyboard.home_end_move_focus => {
                self.focus_edge(state, true)
            }
            KeyCode::End if self.policy.keyboard.home_end_move_focus => {
                self.focus_edge(state, false)
            }
            KeyCode::Enter if self.policy.keyboard.enter_selects => self.select_focused(state),
            KeyCode::Space | KeyCode::Char(' ') if self.policy.keyboard.space_selects => {
                self.select_focused(state)
            }
            KeyCode::Char(_)
            | KeyCode::Enter
            | KeyCode::Tab
            | KeyCode::Backspace
            | KeyCode::Delete
            | KeyCode::Escape
            | KeyCode::Space
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Insert
            | KeyCode::F(_) => SelectableListOutcome::Ignored,
        }
    }

    fn handle_mouse(
        &self,
        area: Rect,
        state: &mut SelectableListState,
        mouse: MouseEvent,
    ) -> SelectableListOutcome {
        if !self.policy.mouse.enabled {
            return SelectableListOutcome::Ignored;
        }
        let hit = self.hit_index(area, mouse);
        match mouse.kind {
            MouseEventKind::Move if self.policy.mouse.hover => Self::hover(state, hit),
            MouseEventKind::Down(MouseButton::Left) if self.policy.mouse.click => {
                Self::press(state, hit)
            }
            MouseEventKind::Up(MouseButton::Left) if self.policy.mouse.click => {
                self.release(state, hit)
            }
            MouseEventKind::Drag(MouseButton::Left) if self.policy.mouse.click => {
                Self::drag(state, hit)
            }
            MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight
            | MouseEventKind::Move => SelectableListOutcome::Ignored,
        }
    }

    fn hover(state: &mut SelectableListState, hit: Option<usize>) -> SelectableListOutcome {
        if state.hovered == hit {
            SelectableListOutcome::Ignored
        } else {
            state.hovered = hit;
            SelectableListOutcome::Redraw
        }
    }

    const fn press(state: &mut SelectableListState, hit: Option<usize>) -> SelectableListOutcome {
        let Some(index) = hit else {
            return SelectableListOutcome::Ignored;
        };
        state.pressed = Some(index);
        state.hovered = Some(index);
        state.set_focused(Some(index));
        SelectableListOutcome::Redraw
    }

    fn release(
        &self,
        state: &mut SelectableListState,
        hit: Option<usize>,
    ) -> SelectableListOutcome {
        let was_pressed = state.pressed;
        state.pressed = None;
        if let Some(index) = hit.filter(|hit_index| was_pressed == Some(*hit_index)) {
            return self.select_index(state, index);
        }
        if was_pressed.is_some() {
            SelectableListOutcome::Redraw
        } else {
            SelectableListOutcome::Ignored
        }
    }

    fn drag(state: &mut SelectableListState, hit: Option<usize>) -> SelectableListOutcome {
        let pressed = if state.pressed.is_some() { hit } else { None };
        if state.hovered == hit && state.pressed == pressed {
            SelectableListOutcome::Ignored
        } else {
            state.hovered = hit;
            state.pressed = pressed;
            SelectableListOutcome::Redraw
        }
    }

    fn move_focus(
        &self,
        state: &mut SelectableListState,
        direction: Direction,
    ) -> SelectableListOutcome {
        let Some(current) = state.focused else {
            return SelectableListOutcome::Ignored;
        };
        let Some(next) = self.next_enabled(current, direction) else {
            return SelectableListOutcome::Ignored;
        };
        if next == current {
            return SelectableListOutcome::Ignored;
        }
        state.set_focused(Some(next));
        SelectableListOutcome::Focused(next)
    }

    fn focus_edge(&self, state: &mut SelectableListState, first: bool) -> SelectableListOutcome {
        let next = if first {
            self.items.iter().position(|item| !item.disabled)
        } else {
            self.items.iter().rposition(|item| !item.disabled)
        };
        let Some(index) = next else {
            return SelectableListOutcome::Ignored;
        };
        if state.focused == Some(index) {
            SelectableListOutcome::Ignored
        } else {
            state.set_focused(Some(index));
            SelectableListOutcome::Focused(index)
        }
    }

    fn select_focused(&self, state: &mut SelectableListState) -> SelectableListOutcome {
        let Some(index) = state.focused else {
            return SelectableListOutcome::Ignored;
        };
        self.select_index(state, index)
    }

    fn select_index(&self, state: &mut SelectableListState, index: usize) -> SelectableListOutcome {
        if !self.is_enabled_item(index) || state.selected == Some(index) {
            return SelectableListOutcome::Ignored;
        }
        state.selected = Some(index);
        state.set_focused(Some(index));
        SelectableListOutcome::Selected(index)
    }

    fn next_enabled(&self, current: usize, direction: Direction) -> Option<usize> {
        if self.items.is_empty() {
            return None;
        }
        let mut index = current;
        for _ in 0..self.items.len() {
            index = match direction {
                Direction::Previous if index == 0 && self.policy.keyboard.wrap => {
                    self.items.len().saturating_sub(1)
                }
                Direction::Previous if index == 0 => return Some(current),
                Direction::Previous => index.saturating_sub(1),
                Direction::Next if index + 1 >= self.items.len() && self.policy.keyboard.wrap => 0,
                Direction::Next if index + 1 >= self.items.len() => return Some(current),
                Direction::Next => index + 1,
            };
            if self.is_enabled_item(index) {
                return Some(index);
            }
        }
        Some(current)
    }

    fn hit_index(&self, area: Rect, mouse: MouseEvent) -> Option<usize> {
        if !area.contains(mouse.position) {
            return None;
        }
        let index = usize::from(mouse.position.y.saturating_sub(area.y));
        if index < self.items.len() && self.is_enabled_item(index) {
            Some(index)
        } else {
            None
        }
    }

    fn normalize_state(&self, state: &mut SelectableListState) {
        if state
            .selected
            .is_some_and(|index| index >= self.items.len())
        {
            state.selected = None;
        }
        if state.focused.is_some_and(|index| index >= self.items.len()) {
            state.set_focused(None);
        }
        if state.hovered.is_some_and(|index| index >= self.items.len()) {
            state.hovered = None;
        }
        if state.pressed.is_some_and(|index| index >= self.items.len()) {
            state.pressed = None;
        }
    }

    fn is_enabled_item(&self, index: usize) -> bool {
        self.items.get(index).is_some_and(|item| !item.disabled)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Previous,
    Next,
}

#[cfg(test)]
mod tests {
    use bmux_keyboard::{KeyCode, KeyStroke};
    use bmux_tui::buffer::Buffer;
    use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};

    use super::{SelectableList, SelectableListItem, SelectableListOutcome, SelectableListState};

    #[test]
    fn renders_selected_item() {
        let items = vec![
            SelectableListItem::new("draft", "Draft"),
            SelectableListItem::new("published", "Published"),
        ];
        let list = SelectableList::new(&items);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 14, 2));
        let mut frame = Frame::new(&mut buffer);

        list.render(
            Rect::new(0, 0, 14, 2),
            &SelectableListState::new(Some(1)),
            &mut frame,
        );

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("  Draft       ")
        );
        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("> Published   ")
        );
    }

    #[test]
    fn arrow_key_moves_focus_to_next_enabled_item() {
        let items = vec![
            SelectableListItem::new("draft", "Draft"),
            SelectableListItem::new("review", "Review").disabled(true),
            SelectableListItem::new("published", "Published"),
        ];
        let list = SelectableList::new(&items);
        let mut state = SelectableListState::new(Some(0));
        state.set_focused(Some(0));

        let outcome = list.handle_event(
            Rect::new(0, 0, 14, 3),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Down)),
        );

        assert_eq!(outcome, SelectableListOutcome::Focused(2));
        assert_eq!(state.focused(), Some(2));
    }

    #[test]
    fn focused_enter_selects_item() {
        let items = vec![
            SelectableListItem::new("draft", "Draft"),
            SelectableListItem::new("published", "Published"),
        ];
        let list = SelectableList::new(&items);
        let mut state = SelectableListState::new(Some(0));
        state.set_focused(Some(1));

        let outcome = list.handle_event(
            Rect::new(0, 0, 14, 2),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Enter)),
        );

        assert_eq!(outcome, SelectableListOutcome::Selected(1));
        assert_eq!(state.selected(), Some(1));
    }

    #[test]
    fn mouse_click_focuses_and_selects_item() {
        let items = vec![
            SelectableListItem::new("draft", "Draft"),
            SelectableListItem::new("published", "Published"),
        ];
        let list = SelectableList::new(&items);
        let mut state = SelectableListState::new(Some(0));
        let area = Rect::new(0, 0, 14, 2);

        let down = list.handle_event(
            area,
            &mut state,
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(1, 1),
            )),
        );
        let up = list.handle_event(
            area,
            &mut state,
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Up(MouseButton::Left),
                Point::new(1, 1),
            )),
        );

        assert_eq!(down, SelectableListOutcome::Redraw);
        assert_eq!(up, SelectableListOutcome::Selected(1));
        assert_eq!(state.focused(), Some(1));
        assert_eq!(state.selected(), Some(1));
    }

    #[test]
    fn disabled_list_ignores_events() {
        let items = vec![SelectableListItem::new("draft", "Draft")];
        let list = SelectableList::new(&items);
        let mut state = SelectableListState::new(Some(0));
        state.set_disabled(true);
        state.set_focused(Some(0));

        let outcome = list.handle_event(
            Rect::new(0, 0, 14, 1),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Enter)),
        );

        assert_eq!(outcome, SelectableListOutcome::Ignored);
        assert_eq!(state.selected(), Some(0));
    }
}
