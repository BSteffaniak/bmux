//! Configurable selectable-menu component.

use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::hit::{HitId, HitRegion as SceneRegion, HitRole};
use bmux_tui::prelude::{Line, Span, Style};

use crate::selectable_list::{
    SelectableList, SelectableListItem, SelectableListOutcome, SelectableListPolicy,
    SelectableListState, SelectableListStyles,
};

/// One selectable menu item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    /// Stable item id chosen by the caller.
    pub id: String,
    /// Visible rich item content lines.
    pub lines: Vec<Line>,
    /// Whether this menu item has a submenu affordance.
    pub submenu: bool,
    /// Whether this menu item is a non-activating section/header row.
    pub section: bool,
    /// Whether this menu item is disabled independently from the whole menu.
    pub disabled: bool,
}

impl MenuItem {
    /// Create an enabled menu item.
    #[must_use]
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            lines: vec![Line::from(label.into())],
            submenu: false,
            section: false,
            disabled: false,
        }
    }

    /// Create an enabled menu item with rich line content.
    #[must_use]
    pub fn rich(id: impl Into<String>, line: impl Into<Line>) -> Self {
        Self {
            id: id.into(),
            lines: vec![line.into()],
            submenu: false,
            section: false,
            disabled: false,
        }
    }

    /// Return this menu item with submenu affordance state set.
    #[must_use]
    pub const fn submenu(mut self, submenu: bool) -> Self {
        self.submenu = submenu;
        self
    }

    /// Return this menu item with non-activating section/header state set.
    #[must_use]
    pub const fn section(mut self, section: bool) -> Self {
        self.section = section;
        self
    }

    /// Return this menu item with disabled state set.
    #[must_use]
    pub const fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

/// Visual styles for a selectable menu.
pub type MenuStyles = SelectableListStyles;

/// Configurable selectable-menu behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuPolicy {
    /// Underlying selectable-list behavior.
    pub list: SelectableListPolicy,
    /// Whether Escape cancels/dismisses the menu.
    pub escape_cancels: bool,
    /// Whether printable character keys are reported as typeahead requests.
    pub typeahead: bool,
    /// Submenu affordance suffix.
    pub submenu_indicator: &'static str,
}

impl MenuPolicy {
    /// Common interactive menu behavior.
    #[must_use]
    pub const fn interactive() -> Self {
        Self {
            list: SelectableListPolicy::interactive(),
            escape_cancels: true,
            typeahead: false,
            submenu_indicator: "›",
        }
    }
}

impl Default for MenuPolicy {
    fn default() -> Self {
        Self::interactive()
    }
}

/// Runtime selectable-menu state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuState {
    /// Underlying selectable-list state.
    pub list: SelectableListState,
}

impl MenuState {
    /// Create enabled menu state.
    #[must_use]
    pub const fn new(selected: Option<usize>) -> Self {
        Self {
            list: SelectableListState::new(selected),
        }
    }

    /// Return selected item index.
    #[must_use]
    pub const fn selected(self) -> Option<usize> {
        self.list.selected()
    }

    /// Return focused item index.
    #[must_use]
    pub const fn focused(self) -> Option<usize> {
        self.list.focused()
    }

    /// Set focused item index, or clear keyboard focus.
    pub const fn set_focused(&mut self, focused: Option<usize>) {
        self.list.set_focused(focused);
    }

    /// Set disabled state for the whole menu.
    pub const fn set_disabled(&mut self, disabled: bool) {
        self.list.set_disabled(disabled);
    }
}

/// Outcome from selectable-menu input handling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuOutcome {
    /// Event was not handled.
    Ignored,
    /// Visual state changed without semantic action.
    Redraw,
    /// Focus moved to the contained item index.
    Focused(usize),
    /// Menu item was activated.
    Activated { index: usize, id: String },
    /// Character was requested for caller-owned typeahead/search handling.
    Typeahead(char),
    /// Menu was cancelled/dismissed.
    Cancelled,
}

/// Configurable vertical selectable menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu<'a> {
    items: &'a [MenuItem],
    policy: MenuPolicy,
    styles: MenuStyles,
}

impl<'a> Menu<'a> {
    /// Create a selectable menu over caller-owned items.
    #[must_use]
    pub fn new(items: &'a [MenuItem]) -> Self {
        Self {
            items,
            policy: MenuPolicy::default(),
            styles: MenuStyles::default(),
        }
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: MenuPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: MenuStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Return required render size.
    #[must_use]
    pub fn size(&self) -> (u16, u16) {
        let items = self.list_items();
        SelectableList::new(&items).size()
    }

    /// Render the menu and register it as one composite tab stop.
    ///
    /// Use [`Self::render_with_id`] when focus must survive responsive reflow
    /// or callers route events by semantic identity.
    pub fn render(&self, area: Rect, state: &MenuState, frame: &mut Frame<'_>) {
        let id = frame.next_interaction_id("menu");
        self.render_with_id(id, area, state, frame);
    }

    /// Render the menu with a stable interaction identifier.
    pub fn render_with_id(
        &self,
        id: impl Into<HitId>,
        area: Rect,
        state: &MenuState,
        frame: &mut Frame<'_>,
    ) {
        self.render_with_id_and_fallback_style(id, area, state, frame, Style::new());
    }

    /// Render the menu with fallback style filling each item row.
    pub fn render_with_fallback_style(
        &self,
        area: Rect,
        state: &MenuState,
        frame: &mut Frame<'_>,
        fallback: Style,
    ) {
        let id = frame.next_interaction_id("menu");
        self.render_with_id_and_fallback_style(id, area, state, frame, fallback);
    }

    /// Render with a stable interaction identifier and fallback style.
    pub fn render_with_id_and_fallback_style(
        &self,
        id: impl Into<HitId>,
        area: Rect,
        state: &MenuState,
        frame: &mut Frame<'_>,
        fallback: Style,
    ) {
        frame.push_hit(
            SceneRegion::new(id, area)
                .role(HitRole::ListItem)
                .hoverable(self.policy.list.mouse.hover)
                .focusable(true)
                .enabled(!state.list.interaction.disabled),
        );
        let items = self.list_items();
        SelectableList::new(&items)
            .policy(self.policy.list)
            .styles(self.styles)
            .render_with_fallback_style(area, &state.list, frame, fallback);
    }

    /// Handle one input event.
    pub fn handle_event(&self, area: Rect, state: &mut MenuState, event: &Event) -> MenuOutcome {
        let keyboard_event = matches!(event, Event::Key(_));
        if keyboard_event && matches!(event, Event::Key(stroke) if self.is_cancel_key(*stroke)) {
            return MenuOutcome::Cancelled;
        }
        if keyboard_event && matches!(event, Event::Key(stroke) if Self::is_activation_key(*stroke))
        {
            return state
                .focused()
                .filter(|index| {
                    self.items
                        .get(*index)
                        .is_some_and(Self::is_activatable_item)
                })
                .map_or(MenuOutcome::Ignored, |index| self.activate(index));
        }
        if keyboard_event
            && let Event::Key(stroke) = event
            && self.policy.typeahead
            && stroke.modifiers.is_empty()
            && let KeyCode::Char(ch) = stroke.key
        {
            return MenuOutcome::Typeahead(ch);
        }
        let items = self.list_items();
        match SelectableList::new(&items)
            .policy(self.policy.list)
            .styles(self.styles)
            .handle_event(area, &mut state.list, event)
        {
            SelectableListOutcome::Ignored => MenuOutcome::Ignored,
            SelectableListOutcome::Redraw => MenuOutcome::Redraw,
            SelectableListOutcome::Focused(index) => MenuOutcome::Focused(index),
            SelectableListOutcome::Selected(index) => self.activate(index),
        }
    }

    fn activate(&self, index: usize) -> MenuOutcome {
        self.items
            .get(index)
            .filter(|item| Self::is_activatable_item(item))
            .map_or(MenuOutcome::Ignored, |item| MenuOutcome::Activated {
                index,
                id: item.id.clone(),
            })
    }

    const fn is_activatable_item(item: &MenuItem) -> bool {
        !item.disabled && !item.section
    }

    fn is_cancel_key(&self, stroke: KeyStroke) -> bool {
        self.policy.escape_cancels && stroke.modifiers.is_empty() && stroke.key == KeyCode::Escape
    }

    const fn is_activation_key(stroke: KeyStroke) -> bool {
        stroke.modifiers.is_empty()
            && matches!(
                stroke.key,
                KeyCode::Enter | KeyCode::Space | KeyCode::Char(' ')
            )
    }

    fn list_items(&self) -> Vec<SelectableListItem> {
        self.items
            .iter()
            .map(|item| {
                let mut lines = item.lines.clone();
                if item.submenu
                    && let Some(line) = lines.first_mut()
                {
                    line.push_span(Span::raw(format!(" {}", self.policy.submenu_indicator)));
                }
                SelectableListItem {
                    id: item.id.clone(),
                    lines,
                    disabled: item.disabled || item.section,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use bmux_keyboard::{KeyCode, KeyStroke};
    use bmux_tui::buffer::Buffer;
    use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};
    use bmux_tui::hit::HitRole;
    use bmux_tui::prelude::{Line, Span};
    use bmux_tui::style::{Color, Style};

    use super::{Menu, MenuItem, MenuOutcome, MenuPolicy, MenuState, SelectableListStyles};

    #[test]
    fn renders_menu_items() {
        let items = items();
        let menu = Menu::new(&items);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 2));
        let mut frame = Frame::new(&mut buffer);

        menu.render(Rect::new(0, 0, 12, 2), &MenuState::new(Some(0)), &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("> Open      ")
        );
        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("  Close     ")
        );
    }

    #[test]
    fn render_registers_exact_composite_geometry_and_disabled_state() {
        let items = items();
        let menu = Menu::new(&items);
        let enabled = MenuState::new(Some(0));
        let mut disabled = MenuState::new(Some(0));
        disabled.set_disabled(true);
        let mut buffer = Buffer::empty(Rect::new(3, 2, 20, 5));
        let mut frame = Frame::new(&mut buffer);

        menu.render_with_id("file-menu", Rect::new(6, 3, 12, 2), &enabled, &mut frame);
        menu.render_with_id_and_fallback_style(
            "disabled-menu",
            Rect::new(6, 5, 12, 2),
            &disabled,
            &mut frame,
            Style::new(),
        );

        let regions = frame.hits().regions();
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0].id.as_str(), "file-menu");
        assert_eq!(regions[0].area, Rect::new(6, 3, 12, 2));
        assert_eq!(regions[0].role, HitRole::ListItem);
        assert!(regions[0].focusable);
        assert!(regions[0].enabled);
        assert_eq!(regions[1].id.as_str(), "disabled-menu");
        assert_eq!(regions[1].area, Rect::new(6, 5, 12, 2));
        assert!(!regions[1].enabled);
        assert_eq!(frame.hits().focus_targets(None).len(), 1);
    }

    #[test]
    fn enter_activates_focused_item() {
        let items = items();
        let menu = Menu::new(&items);
        let mut state = MenuState::new(Some(0));
        state.set_focused(state.selected());

        let outcome = menu.handle_event(
            Rect::new(0, 0, 12, 2),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Enter)),
        );

        assert_eq!(
            outcome,
            MenuOutcome::Activated {
                index: 0,
                id: "open".to_string()
            }
        );
    }

    #[test]
    fn directly_dispatched_menu_activation_uses_selected_item() {
        let items = items();
        let menu = Menu::new(&items);
        let mut state = MenuState::new(Some(0));

        let outcome = menu.handle_event(
            Rect::new(0, 0, 12, 2),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Enter)),
        );

        assert_eq!(
            outcome,
            MenuOutcome::Activated {
                index: 0,
                id: "open".to_string(),
            }
        );
    }

    #[test]
    fn escape_cancels_menu() {
        let items = items();
        let menu = Menu::new(&items);
        let mut state = MenuState::new(Some(0));
        state.set_focused(state.selected());

        let outcome = menu.handle_event(
            Rect::new(0, 0, 12, 2),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Escape)),
        );

        assert_eq!(outcome, MenuOutcome::Cancelled);
    }

    #[test]
    fn mouse_click_activates_item() {
        let items = items();
        let menu = Menu::new(&items);
        let mut state = MenuState::new(Some(0));
        let area = Rect::new(0, 0, 12, 2);

        let _ = menu.handle_event(
            area,
            &mut state,
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(1, 1),
            )),
        );
        let outcome = menu.handle_event(
            area,
            &mut state,
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Up(MouseButton::Left),
                Point::new(1, 1),
            )),
        );

        assert_eq!(
            outcome,
            MenuOutcome::Activated {
                index: 1,
                id: "close".to_string()
            }
        );
    }

    #[test]
    fn rich_labels_preserve_styles_and_submenu_affordance_renders() {
        let accent = Style::new().fg(Color::Yellow);
        let items = [
            MenuItem::rich("new", Line::from_spans([Span::styled("New", accent)])).submenu(true),
        ];
        let menu = Menu::new(&items).policy(MenuPolicy {
            submenu_indicator: ">",
            ..MenuPolicy::default()
        });
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 1));
        let mut frame = Frame::new(&mut buffer);

        menu.render(Rect::new(0, 0, 10, 1), &MenuState::new(Some(0)), &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("> New >   "));
        assert_eq!(
            frame
                .buffer()
                .get(Point::new(2, 0))
                .map(|cell| cell.style.fg),
            Some(Some(Color::Yellow))
        );
    }

    #[test]
    fn section_items_are_not_activated() {
        let items = [MenuItem::new("section", "File").section(true)];
        let menu = Menu::new(&items);
        let mut state = MenuState::new(Some(0));
        state.set_focused(state.selected());

        let outcome = menu.handle_event(
            Rect::new(0, 0, 12, 1),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Enter)),
        );

        assert_eq!(outcome, MenuOutcome::Ignored);
    }

    #[test]
    fn typeahead_reports_printable_character_without_owning_search_state() {
        let items = items();
        let menu = Menu::new(&items).policy(MenuPolicy {
            typeahead: true,
            ..MenuPolicy::default()
        });
        let mut state = MenuState::new(Some(0));
        state.set_focused(state.selected());

        let outcome = menu.handle_event(
            Rect::new(0, 0, 12, 2),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Char('o'))),
        );

        assert_eq!(outcome, MenuOutcome::Typeahead('o'));
    }

    #[test]
    fn disabled_focused_and_hovered_style_precedence_comes_from_selectable_list() {
        let items = [MenuItem::new("disabled", "Disabled").disabled(true)];
        let styles = SelectableListStyles {
            disabled: Style::new().fg(Color::Red),
            focused: Style::new().fg(Color::Green),
            hovered: Style::new().fg(Color::Blue),
            ..SelectableListStyles::default()
        };
        let menu = Menu::new(&items).styles(styles);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 1));
        let mut frame = Frame::new(&mut buffer);

        menu.render(Rect::new(0, 0, 12, 1), &MenuState::new(Some(0)), &mut frame);

        assert_eq!(
            frame
                .buffer()
                .get(Point::new(2, 0))
                .map(|cell| cell.style.fg),
            Some(Some(Color::Red))
        );
    }

    fn items() -> Vec<MenuItem> {
        vec![
            MenuItem::new("open", "Open"),
            MenuItem::new("close", "Close"),
        ]
    }
}
