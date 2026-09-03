//! Configurable selectable-menu component.

use std::cell::Cell;
use std::hash::{Hash, Hasher};

use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::component::{
    Component, ComponentRevision, Constraints, EventCx, LayoutCx, LayoutId, LayoutMetadata,
    LayoutNode, LogicalSize,
};
use bmux_tui::event::{Event, EventOutcome};
use bmux_tui::geometry::Rect;
use bmux_tui::hit::{HitRegion as SceneRegion, HitRole};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::semantic::SemanticRegion;

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

    /// Paint visible menu rows through a scoped local-coordinate context whose
    /// origin is this menu's top-left corner.
    pub fn paint(&self, area: Rect, state: &MenuState, fallback: Style, cx: &mut PaintCx<'_, '_>) {
        let items = self.list_items();
        SelectableList::new(&items)
            .policy(self.policy.list)
            .styles(self.styles)
            .paint(area, &state.list, fallback, cx);
    }

    /// Return the menu item index at a terminal point using the same
    /// component layout used for rendering and event handling.
    #[must_use]
    pub fn item_index_at(
        &self,
        area: Rect,
        state: &MenuState,
        point: bmux_tui::geometry::Point,
    ) -> Option<usize> {
        let items = self.list_items();
        let id = SelectableList::new(&items).semantic_id_at(area, &state.list, point)?;
        self.items.iter().position(|item| item.id == id)
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

/// Canonical component-lifecycle selectable menu.
///
/// The menu measures its exact stacked item height, paints complete rows
/// through the scoped paint context, registers one composite roving-focus
/// region plus one visible region per stable item id, and routes events
/// through the same resolved layout. Menu state remains caller-owned through
/// an interior-mutable `Cell`; semantic outcomes such as activation and
/// cancellation are read by callers from [`Menu::handle_event`] or by
/// comparing caller state before and after the event.
pub struct MenuComponent<'a, 'state> {
    id: LayoutId,
    menu: Menu<'a>,
    state: &'state Cell<MenuState>,
    fallback: Style,
}

impl<'a, 'state> MenuComponent<'a, 'state> {
    /// Create a menu with stable identity and caller-owned state.
    #[must_use]
    pub fn new(
        id: impl Into<LayoutId>,
        items: &'a [MenuItem],
        state: &'state Cell<MenuState>,
    ) -> Self {
        Self {
            id: id.into(),
            menu: Menu::new(items),
            state,
            fallback: Style::new(),
        }
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: MenuPolicy) -> Self {
        self.menu.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: MenuStyles) -> Self {
        self.menu.styles = styles;
        self
    }

    /// Set the row fill style applied beneath every item row.
    #[must_use]
    pub const fn fallback_style(mut self, fallback: Style) -> Self {
        self.fallback = fallback;
        self
    }
}

impl Component for MenuComponent<'_, '_> {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        for item in self.menu.items {
            item.id.hash(&mut layout);
            item.submenu.hash(&mut layout);
            item.lines.len().hash(&mut layout);
            for line in &item.lines {
                format!("{line:?}").hash(&mut layout);
            }
        }
        self.menu.policy.submenu_indicator.hash(&mut layout);
        self.menu.policy.list.highlight.symbol.hash(&mut layout);
        self.menu
            .policy
            .list
            .highlight
            .repeat_spacing
            .hash(&mut layout);
        format!("{:?}", self.menu.policy.list.scrollbar).hash(&mut layout);

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        for item in self.menu.items {
            item.disabled.hash(&mut paint);
            item.section.hash(&mut paint);
        }
        format!("{:?}", self.menu.styles).hash(&mut paint);
        format!("{:?}", self.fallback).hash(&mut paint);
        format!("{:?}", self.state.get()).hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let (width, _) = self.menu.size();
        let height = self
            .menu
            .items
            .iter()
            .map(|item| item.lines.len().max(1))
            .sum::<usize>();
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(width, height)),
        )
        .with_metadata(LayoutMetadata::new().semantic("menu"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let height = u16::try_from(layout.size.height).unwrap_or(u16::MAX);
        let area = Rect::new(0, 0, layout.size.width, height);
        if area.is_empty() {
            return;
        }
        let state = self.state.get();
        cx.push_hit(
            SceneRegion::new(self.id.as_str(), area)
                .role(HitRole::ListItem)
                .hoverable(self.menu.policy.list.mouse.hover)
                .focusable(true)
                .enabled(!state.list.interaction.disabled),
        );
        let items = self.menu.list_items();
        let list = SelectableList::new(&items)
            .policy(self.menu.policy.list)
            .styles(self.menu.styles);
        for region in list.visible_semantic_regions(area, &state.list) {
            let item_id = format!("{}.{}", self.id.as_str(), region.key);
            let disabled = self
                .menu
                .items
                .iter()
                .find(|item| item.id == region.key)
                .is_none_or(|item| !Menu::is_activatable_item(item));
            cx.push_hit(
                SceneRegion::new(item_id.clone(), region.rect)
                    .role(HitRole::ListItem)
                    .hoverable(self.menu.policy.list.mouse.hover)
                    .enabled(!state.list.interaction.disabled && !disabled),
            );
            cx.push_semantic(SemanticRegion::new(item_id, region.rect, "menu-item"));
        }
        list.paint(area, &state.list, self.fallback, cx);
        cx.push_semantic(SemanticRegion::new(self.id.as_str(), area, "menu"));
        cx.push_damage(LocalRect::new(0, 0, area.width, area.height));
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        let Some(area) = cx.find_rect(&layout.id) else {
            return EventOutcome::Ignored;
        };
        let mut state = self.state.get();
        let outcome = self.menu.handle_event(area, &mut state, event);
        self.state.set(state);
        match outcome {
            MenuOutcome::Ignored => EventOutcome::Ignored,
            MenuOutcome::Typeahead(_) | MenuOutcome::Cancelled => EventOutcome::Handled,
            MenuOutcome::Redraw | MenuOutcome::Focused(_) | MenuOutcome::Activated { .. } => {
                EventOutcome::Redraw
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use bmux_keyboard::{KeyCode, KeyStroke};
    use bmux_tui::buffer::Buffer;
    use bmux_tui::component::{Component, Constraints, EventCx, LayoutCx};
    use bmux_tui::event::{Event, EventOutcome, MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};
    use bmux_tui::hit::HitRole;
    use bmux_tui::paint::{LocalRect, PaintCx};
    use bmux_tui::prelude::{Line, Span};
    use bmux_tui::style::{Color, Style};

    use super::{
        Menu, MenuComponent, MenuItem, MenuOutcome, MenuPolicy, MenuState, SelectableListStyles,
    };

    fn render_component(component: &MenuComponent<'_, '_>, area: Rect, frame: &mut Frame<'_>) {
        let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
        PaintCx::new(frame).with_child(
            i32::from(area.x),
            i64::from(area.y),
            LocalRect::new(0, 0, area.width, area.height),
            |cx| component.paint(&layout, cx),
        );
    }

    trait MenuTestRender {
        fn render(&self, area: Rect, state: &MenuState, frame: &mut Frame<'_>);
    }

    impl MenuTestRender for Menu<'_> {
        fn render(&self, area: Rect, state: &MenuState, frame: &mut Frame<'_>) {
            let state = Cell::new(*state);
            let component = MenuComponent {
                id: "test.menu".into(),
                menu: self.clone(),
                state: &state,
                fallback: Style::new(),
            };
            render_component(&component, area, frame);
        }
    }

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
    fn component_measures_exact_height_and_registers_composite_and_item_geometry() {
        let items = [
            MenuItem::new("open", "Open"),
            MenuItem::new("recent", "Recent").section(true),
            MenuItem::new("close", "Close"),
        ];
        let enabled = Cell::new(MenuState::new(Some(0)));
        let mut disabled_state = MenuState::new(Some(0));
        disabled_state.set_disabled(true);
        let disabled = Cell::new(disabled_state);
        let mut buffer = Buffer::empty(Rect::new(3, 2, 20, 8));
        let mut frame = Frame::new(&mut buffer);

        let component = MenuComponent::new("file-menu", &items, &enabled);
        let mut cx = LayoutCx::new();
        let layout = component.layout(Constraints::for_width(12), &mut cx);
        assert_eq!(layout.size.height, 3);
        assert_eq!(cx.measured_nodes(), 1);
        render_component(&component, Rect::new(6, 3, 12, 3), &mut frame);
        render_component(
            &MenuComponent::new("disabled-menu", &items, &disabled),
            Rect::new(6, 6, 12, 3),
            &mut frame,
        );

        let regions = frame.hits().regions();
        assert_eq!(regions[0].id.as_str(), "file-menu");
        assert_eq!(regions[0].area, Rect::new(6, 3, 12, 3));
        assert_eq!(regions[0].role, HitRole::ListItem);
        assert!(regions[0].focusable);
        assert!(regions[0].enabled);
        assert_eq!(regions[1].id.as_str(), "file-menu.open");
        assert_eq!(regions[1].area, Rect::new(6, 3, 12, 1));
        assert!(regions[1].enabled);
        assert!(!regions[1].focusable);
        assert_eq!(regions[2].id.as_str(), "file-menu.recent");
        assert!(!regions[2].enabled, "section rows are not activatable");
        assert_eq!(regions[3].id.as_str(), "file-menu.close");
        assert_eq!(regions[4].id.as_str(), "disabled-menu");
        assert_eq!(regions[4].area, Rect::new(6, 6, 12, 3));
        assert!(!regions[4].enabled);
        assert!(regions[5..].iter().all(|region| !region.enabled));
        assert_eq!(frame.hits().focus_targets(None).len(), 1);
        let semantics = frame.semantics().regions();
        assert!(
            semantics
                .iter()
                .any(|region| region.id == "file-menu.close" && region.role == "menu-item")
        );
        assert!(
            semantics
                .iter()
                .any(|region| region.id == "file-menu" && region.role == "menu")
        );
    }

    #[test]
    fn component_routes_events_through_resolved_layout_and_updates_caller_state() {
        let items = items();
        let state = Cell::new(MenuState::new(Some(0)));
        let component = MenuComponent::new("file-menu", &items, &state);
        let layout = component.layout(
            Constraints::tight(Rect::new(0, 0, 12, 2).size()),
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
        assert_eq!(state.get().focused(), Some(1));
        assert_eq!(
            component.event(
                &Event::Key(KeyStroke::simple(KeyCode::Escape)),
                &layout,
                &mut EventCx::new(&layout),
            ),
            EventOutcome::Handled
        );
        assert_eq!(
            component.event(&Event::Tick, &layout, &mut EventCx::new(&layout)),
            EventOutcome::Ignored
        );
    }

    #[test]
    fn component_revision_separates_layout_and_paint_changes() {
        let items = items();
        let state = Cell::new(MenuState::new(None));
        let component = MenuComponent::new("file-menu", &items, &state);
        let before = component.revision();

        state.set(MenuState::new(Some(1)));
        let paint_only = component.revision();
        assert_eq!(before.layout, paint_only.layout);
        assert_ne!(before.paint, paint_only.paint);

        let more = [
            MenuItem::new("open", "Open"),
            MenuItem::new("close", "Close"),
            MenuItem::new("quit", "Quit"),
        ];
        let relayout = MenuComponent::new("file-menu", &more, &state).revision();
        assert_ne!(before.layout, relayout.layout);
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
