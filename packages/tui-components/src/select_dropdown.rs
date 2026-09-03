//! Configurable select/dropdown field control.

use std::cell::Cell;
use std::hash::{Hash, Hasher};

use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::component::{
    Component, ComponentRevision, Constraints, EventCx, LayoutCx, LayoutId, LayoutMetadata,
    LayoutNode, LogicalSize,
};
use bmux_tui::event::{Event, EventOutcome, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::geometry::Rect;
use bmux_tui::hit::{HitRegion as SceneRegion, HitRole};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::semantic::SemanticRegion;
use bmux_tui::style::Modifier;
use bmux_tui::text_width::display_width;

use crate::common::{ComponentMousePolicy, InteractionState};
use crate::selectable_list::{SelectableList, SelectableListItem, SelectableListState};

/// One select/dropdown option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectOption {
    /// Stable option id chosen by the caller.
    pub id: String,
    /// Visible option label.
    pub label: String,
    /// Whether this option is disabled independently from the whole control.
    pub disabled: bool,
}

impl SelectOption {
    /// Create an enabled select option.
    #[must_use]
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            disabled: false,
        }
    }

    /// Return this option with disabled state set.
    #[must_use]
    pub const fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

/// Visual styles for a select/dropdown control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectDropdownStyles {
    /// Style used when the closed control is enabled and inactive.
    pub normal: Style,
    /// Style used when the control has keyboard focus.
    pub focused: Style,
    /// Style used when the pointer is hovering the closed control.
    pub hovered: Style,
    /// Style used while the primary pointer/button is pressed.
    pub pressed: Style,
    /// Style used when the control is disabled.
    pub disabled: Style,
}

impl Default for SelectDropdownStyles {
    fn default() -> Self {
        Self {
            normal: Style::new(),
            focused: Style::new().add_modifier(Modifier::REVERSED),
            hovered: Style::new().add_modifier(Modifier::UNDERLINE),
            pressed: Style::new().add_modifier(Modifier::BOLD),
            disabled: Style::new().add_modifier(Modifier::DIM),
        }
    }
}

/// Configurable select/dropdown behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectDropdownPolicy {
    /// Mouse behavior for the closed control and open list.
    pub mouse: ComponentMousePolicy,
    /// Whether Enter toggles the dropdown when focused.
    pub enter_toggles: bool,
    /// Whether Space toggles the dropdown when focused.
    pub space_toggles: bool,
    /// Whether Escape closes the dropdown.
    pub escape_closes: bool,
}

impl SelectDropdownPolicy {
    /// Common interactive select/dropdown behavior.
    #[must_use]
    pub const fn interactive() -> Self {
        Self {
            mouse: ComponentMousePolicy::button(),
            enter_toggles: true,
            space_toggles: true,
            escape_closes: true,
        }
    }
}

impl Default for SelectDropdownPolicy {
    fn default() -> Self {
        Self::interactive()
    }
}

/// Runtime select/dropdown state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectDropdownState {
    /// Common select/dropdown interaction flags.
    pub interaction: InteractionState,
    selected: Option<usize>,
    open: bool,
    list: SelectableListState,
}

impl SelectDropdownState {
    /// Create enabled select/dropdown state.
    #[must_use]
    pub const fn new(selected: Option<usize>) -> Self {
        Self {
            interaction: InteractionState::new(),
            selected,
            open: false,
            list: SelectableListState::new(selected),
        }
    }

    /// Return selected option index.
    #[must_use]
    pub const fn selected(self) -> Option<usize> {
        self.selected
    }

    /// Set selected option index.
    pub const fn set_selected(&mut self, selected: Option<usize>) {
        self.selected = selected;
        self.list.set_selected(selected);
    }

    /// Return whether the dropdown list is open.
    #[must_use]
    pub const fn is_open(self) -> bool {
        self.open
    }

    /// Open or close the dropdown list.
    pub const fn set_open(&mut self, open: bool) {
        self.open = open;
        self.list.set_focused(self.selected);
    }

    /// Set disabled state for the whole control.
    pub const fn set_disabled(&mut self, disabled: bool) {
        self.interaction.disabled = disabled;
        self.list.set_disabled(disabled);
        if disabled {
            self.open = false;
        }
    }
}

/// Outcome from select/dropdown input handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectDropdownOutcome {
    /// Event was not handled.
    Ignored,
    /// Visual state changed without changing selected value.
    Redraw,
    /// The dropdown was opened.
    Opened,
    /// The dropdown was closed.
    Closed,
    /// Focus moved to the contained option index while open.
    Focused(usize),
    /// Selection changed to the contained option index.
    Selected(usize),
}

/// Configurable select/dropdown control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectDropdown<'a> {
    options: &'a [SelectOption],
    placeholder: &'a str,
    policy: SelectDropdownPolicy,
    styles: SelectDropdownStyles,
}

impl<'a> SelectDropdown<'a> {
    /// Create a select/dropdown over caller-owned options.
    #[must_use]
    pub const fn new(options: &'a [SelectOption]) -> Self {
        Self {
            options,
            placeholder: "Select...",
            policy: SelectDropdownPolicy {
                mouse: ComponentMousePolicy {
                    enabled: true,
                    hover: true,
                    click: true,
                },
                enter_toggles: true,
                space_toggles: true,
                escape_closes: true,
            },
            styles: SelectDropdownStyles {
                normal: Style::new(),
                focused: Style::new().add_modifier(Modifier::REVERSED),
                hovered: Style::new().add_modifier(Modifier::UNDERLINE),
                pressed: Style::new().add_modifier(Modifier::BOLD),
                disabled: Style::new().add_modifier(Modifier::DIM),
            },
        }
    }

    /// Set placeholder text used when no option is selected.
    #[must_use]
    pub const fn placeholder(mut self, placeholder: &'a str) -> Self {
        self.placeholder = placeholder;
        self
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: SelectDropdownPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: SelectDropdownStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Return the area used by the dropdown list when open.
    #[must_use]
    pub fn list_area(&self, area: Rect) -> Rect {
        Rect::new(
            area.x,
            area.y.saturating_add(1),
            area.width,
            self.list_height(),
        )
    }

    /// Return the natural size: a width that fits every closed label and the
    /// option list, and one closed row plus every option row when open.
    #[must_use]
    pub fn size(&self, state: SelectDropdownState) -> (u16, u16) {
        let items = self.list_items();
        let (list_width, _) = SelectableList::new(&items).size();
        let marker_width = 2;
        let closed_width = self
            .options
            .iter()
            .map(|option| display_width(&option.label))
            .chain(std::iter::once(display_width(self.placeholder)))
            .max()
            .unwrap_or(0)
            .saturating_add(marker_width);
        let width = closed_width.max(usize::from(list_width));
        let height = if state.open {
            self.list_height().saturating_add(1)
        } else {
            1
        };
        (u16::try_from(width).unwrap_or(u16::MAX), height)
    }

    /// Paint the closed control row and, when open, the option list through a
    /// scoped local-coordinate context whose origin is this control's
    /// top-left corner.
    pub fn paint(
        &self,
        area: Rect,
        state: &SelectDropdownState,
        fallback: Style,
        cx: &mut PaintCx<'_, '_>,
    ) {
        if area.is_empty() {
            return;
        }
        let closed_area = Rect::new(area.x, area.y, area.width, 1);
        cx.write_line_with_fallback_style(
            LocalRect::terminal(closed_area),
            &self.closed_line(*state),
            fallback,
        );
        if state.open {
            let items = self.list_items();
            SelectableList::new(&items).paint(
                self.list_area(area).intersection(area),
                &state.list,
                fallback,
                cx,
            );
        }
    }

    /// Handle one input event.
    pub fn handle_event(
        &self,
        area: Rect,
        state: &mut SelectDropdownState,
        event: &Event,
    ) -> SelectDropdownOutcome {
        self.normalize_state(state);
        if state.interaction.disabled {
            return SelectDropdownOutcome::Ignored;
        }
        match event {
            Event::Key(stroke) => self.handle_key(area, state, *stroke),
            Event::Mouse(mouse) => self.handle_mouse(area, state, *mouse),
            Event::Resize(_) | Event::Paste(_) | Event::Focus(_) | Event::Tick | Event::User(_) => {
                SelectDropdownOutcome::Ignored
            }
        }
    }

    fn closed_line(&self, state: SelectDropdownState) -> Line {
        let label = state
            .selected
            .and_then(|index| self.options.get(index))
            .map_or(self.placeholder, |option| option.label.as_str());
        let marker = if state.open { '▴' } else { '▾' };
        Line::from_spans(vec![Span::styled(
            format!("{label} {marker}"),
            self.style_for(state),
        )])
    }

    const fn style_for(&self, state: SelectDropdownState) -> Style {
        if state.interaction.disabled {
            self.styles.disabled
        } else if state.interaction.pressed {
            self.styles.pressed
        } else if state.interaction.focused {
            self.styles.focused
        } else if state.interaction.hovered {
            self.styles.hovered
        } else {
            self.styles.normal
        }
    }

    fn handle_key(
        &self,
        area: Rect,
        state: &mut SelectDropdownState,
        stroke: KeyStroke,
    ) -> SelectDropdownOutcome {
        if !stroke.modifiers.is_empty() {
            return SelectDropdownOutcome::Ignored;
        }
        if state.open {
            match stroke.key {
                KeyCode::Escape if self.policy.escape_closes => return Self::close(state),
                KeyCode::Enter | KeyCode::Space | KeyCode::Char(' ') => {
                    return self.toggle_or_delegate_key(area, state, stroke);
                }
                _ => return self.delegate_key_to_list(area, state, stroke),
            }
        }
        match stroke.key {
            KeyCode::Enter if self.policy.enter_toggles => self.open(state),
            KeyCode::Space | KeyCode::Char(' ') if self.policy.space_toggles => self.open(state),
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
            | KeyCode::F(_) => SelectDropdownOutcome::Ignored,
        }
    }

    fn handle_mouse(
        &self,
        area: Rect,
        state: &mut SelectDropdownState,
        mouse: MouseEvent,
    ) -> SelectDropdownOutcome {
        if !self.policy.mouse.enabled {
            return SelectDropdownOutcome::Ignored;
        }
        if state.open && self.list_area(area).contains(mouse.position) {
            return self.delegate_mouse_to_list(area, state, mouse);
        }
        let closed_area = Rect::new(area.x, area.y, area.width, 1);
        let hit = closed_area.contains(mouse.position);
        match mouse.kind {
            MouseEventKind::Move if self.policy.mouse.hover => {
                if state.interaction.hovered == hit {
                    SelectDropdownOutcome::Ignored
                } else {
                    state.interaction.hovered = hit;
                    SelectDropdownOutcome::Redraw
                }
            }
            MouseEventKind::Down(MouseButton::Left) if self.policy.mouse.click && hit => {
                state.interaction.pressed = true;
                state.interaction.focused = true;
                SelectDropdownOutcome::Redraw
            }
            MouseEventKind::Up(MouseButton::Left) if self.policy.mouse.click => {
                let was_pressed = state.interaction.pressed;
                state.interaction.pressed = false;
                if was_pressed && hit {
                    self.toggle(state)
                } else if was_pressed {
                    SelectDropdownOutcome::Redraw
                } else {
                    SelectDropdownOutcome::Ignored
                }
            }
            MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight
            | MouseEventKind::Move => SelectDropdownOutcome::Ignored,
        }
    }

    fn toggle_or_delegate_key(
        &self,
        area: Rect,
        state: &mut SelectDropdownState,
        stroke: KeyStroke,
    ) -> SelectDropdownOutcome {
        if state.list.focused().is_some() {
            self.delegate_key_to_list(area, state, stroke)
        } else {
            Self::close(state)
        }
    }

    fn delegate_key_to_list(
        &self,
        area: Rect,
        state: &mut SelectDropdownState,
        stroke: KeyStroke,
    ) -> SelectDropdownOutcome {
        let items = self.list_items();
        match SelectableList::new(&items).handle_event(
            self.list_area(area),
            &mut state.list,
            &Event::Key(stroke),
        ) {
            crate::selectable_list::SelectableListOutcome::Ignored => {
                SelectDropdownOutcome::Ignored
            }
            crate::selectable_list::SelectableListOutcome::Redraw => SelectDropdownOutcome::Redraw,
            crate::selectable_list::SelectableListOutcome::Focused(index) => {
                SelectDropdownOutcome::Focused(index)
            }
            crate::selectable_list::SelectableListOutcome::Selected(index) => {
                state.selected = Some(index);
                state.open = false;
                SelectDropdownOutcome::Selected(index)
            }
        }
    }

    fn delegate_mouse_to_list(
        &self,
        area: Rect,
        state: &mut SelectDropdownState,
        mouse: MouseEvent,
    ) -> SelectDropdownOutcome {
        let items = self.list_items();
        match SelectableList::new(&items).handle_event(
            self.list_area(area),
            &mut state.list,
            &Event::Mouse(mouse),
        ) {
            crate::selectable_list::SelectableListOutcome::Ignored => {
                SelectDropdownOutcome::Ignored
            }
            crate::selectable_list::SelectableListOutcome::Redraw => SelectDropdownOutcome::Redraw,
            crate::selectable_list::SelectableListOutcome::Focused(index) => {
                SelectDropdownOutcome::Focused(index)
            }
            crate::selectable_list::SelectableListOutcome::Selected(index) => {
                state.selected = Some(index);
                state.open = false;
                SelectDropdownOutcome::Selected(index)
            }
        }
    }

    fn open(&self, state: &mut SelectDropdownState) -> SelectDropdownOutcome {
        if state.open {
            SelectDropdownOutcome::Ignored
        } else {
            state.open = true;
            state
                .list
                .set_focused(state.selected.or_else(|| self.first_enabled_index()));
            SelectDropdownOutcome::Opened
        }
    }

    const fn close(state: &mut SelectDropdownState) -> SelectDropdownOutcome {
        if state.open {
            state.open = false;
            SelectDropdownOutcome::Closed
        } else {
            SelectDropdownOutcome::Ignored
        }
    }

    fn toggle(&self, state: &mut SelectDropdownState) -> SelectDropdownOutcome {
        if state.open {
            Self::close(state)
        } else {
            self.open(state)
        }
    }

    fn normalize_state(&self, state: &mut SelectDropdownState) {
        if state
            .selected
            .is_some_and(|index| index >= self.options.len())
        {
            state.set_selected(None);
        }
    }

    fn list_items(&self) -> Vec<SelectableListItem> {
        self.options
            .iter()
            .map(|option| SelectableListItem {
                id: option.id.clone(),
                lines: vec![Line::from(option.label.clone())],
                disabled: option.disabled,
            })
            .collect()
    }

    fn list_height(&self) -> u16 {
        u16::try_from(self.options.len()).unwrap_or(u16::MAX)
    }

    fn first_enabled_index(&self) -> Option<usize> {
        self.options.iter().position(|option| !option.disabled)
    }
}

/// Canonical component-lifecycle select/dropdown control.
///
/// The component measures one closed row plus the option list while open,
/// paints through the scoped paint context, registers the closed row as the
/// focusable composite region and each visible option as an item region, and
/// routes events through the same resolved layout. Control state remains
/// caller-owned through an interior-mutable `Cell`.
pub struct SelectDropdownComponent<'a, 'state> {
    id: LayoutId,
    select: SelectDropdown<'a>,
    state: &'state Cell<SelectDropdownState>,
    fallback: Style,
}

impl<'a, 'state> SelectDropdownComponent<'a, 'state> {
    /// Create a select/dropdown with stable identity and caller-owned state.
    #[must_use]
    pub fn new(
        id: impl Into<LayoutId>,
        options: &'a [SelectOption],
        state: &'state Cell<SelectDropdownState>,
    ) -> Self {
        Self {
            id: id.into(),
            select: SelectDropdown::new(options),
            state,
            fallback: Style::new(),
        }
    }

    /// Set placeholder text used when no option is selected.
    #[must_use]
    pub const fn placeholder(mut self, placeholder: &'a str) -> Self {
        self.select.placeholder = placeholder;
        self
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: SelectDropdownPolicy) -> Self {
        self.select.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: SelectDropdownStyles) -> Self {
        self.select.styles = styles;
        self
    }

    /// Set the row fill style applied beneath the control and option rows.
    #[must_use]
    pub const fn fallback_style(mut self, fallback: Style) -> Self {
        self.fallback = fallback;
        self
    }
}

impl Component for SelectDropdownComponent<'_, '_> {
    fn revision(&self) -> ComponentRevision {
        let state = self.state.get();
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        self.select.placeholder.hash(&mut layout);
        for option in self.select.options {
            option.id.hash(&mut layout);
            option.label.hash(&mut layout);
        }
        state.open.hash(&mut layout);

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        for option in self.select.options {
            option.disabled.hash(&mut paint);
        }
        format!("{:?}", self.select.styles).hash(&mut paint);
        format!("{:?}", self.fallback).hash(&mut paint);
        format!("{state:?}").hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let (width, height) = self.select.size(self.state.get());
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(width, usize::from(height))),
        )
        .with_metadata(LayoutMetadata::new().semantic("select"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let height = u16::try_from(layout.size.height).unwrap_or(u16::MAX);
        let area = Rect::new(0, 0, layout.size.width, height);
        if area.is_empty() {
            return;
        }
        let state = self.state.get();
        let closed_area = Rect::new(0, 0, area.width, 1);
        cx.push_hit(
            SceneRegion::new(self.id.as_str(), closed_area)
                .role(HitRole::ListItem)
                .hoverable(self.select.policy.mouse.hover)
                .focusable(true)
                .enabled(!state.interaction.disabled),
        );
        if state.open {
            let list_area = self.select.list_area(area).intersection(area);
            if !list_area.is_empty() {
                cx.push_hit(
                    SceneRegion::new(format!("{}.options", self.id.as_str()), list_area)
                        .role(HitRole::ListItem)
                        .hoverable(self.select.policy.mouse.hover)
                        .focusable(false)
                        .enabled(!state.interaction.disabled),
                );
                let items = self.select.list_items();
                let list = SelectableList::new(&items);
                for region in list.visible_semantic_regions(list_area, &state.list) {
                    let option_id = format!("{}.{}", self.id.as_str(), region.key);
                    let disabled = self
                        .select
                        .options
                        .iter()
                        .find(|option| option.id == region.key)
                        .is_none_or(|option| option.disabled);
                    cx.push_hit(
                        SceneRegion::new(option_id.clone(), region.rect)
                            .role(HitRole::ListItem)
                            .hoverable(self.select.policy.mouse.hover)
                            .enabled(!state.interaction.disabled && !disabled),
                    );
                    cx.push_semantic(SemanticRegion::new(option_id, region.rect, "option"));
                }
            }
        }
        self.select.paint(area, &state, self.fallback, cx);
        cx.push_semantic(SemanticRegion::new(self.id.as_str(), area, "select"));
        cx.push_damage(LocalRect::new(0, 0, area.width, area.height));
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        let Some(area) = cx.find_rect(&layout.id) else {
            return EventOutcome::Ignored;
        };
        let mut state = self.state.get();
        let outcome = self.select.handle_event(area, &mut state, event);
        self.state.set(state);
        match outcome {
            SelectDropdownOutcome::Ignored => EventOutcome::Ignored,
            SelectDropdownOutcome::Redraw
            | SelectDropdownOutcome::Opened
            | SelectDropdownOutcome::Closed
            | SelectDropdownOutcome::Focused(_)
            | SelectDropdownOutcome::Selected(_) => EventOutcome::Redraw,
        }
    }
}

impl crate::theme::ComponentTheme {
    /// Convert this semantic component theme into [`SelectDropdownStyles`].
    #[must_use]
    pub fn select_dropdown_styles(self) -> SelectDropdownStyles {
        SelectDropdownStyles::from(self)
    }
}

impl From<crate::theme::ComponentTheme> for SelectDropdownStyles {
    fn from(theme: crate::theme::ComponentTheme) -> Self {
        let theme = theme.for_surface(crate::theme::ComponentSurfaceDepth::Normal);
        Self {
            normal: theme.text,
            focused: theme.focused,
            hovered: theme.info,
            pressed: theme.selected,
            disabled: theme.disabled,
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

    use super::{
        SelectDropdown, SelectDropdownComponent, SelectDropdownOutcome, SelectDropdownState,
        SelectOption,
    };

    fn render_component(
        component: &SelectDropdownComponent<'_, '_>,
        area: Rect,
        frame: &mut Frame<'_>,
    ) {
        let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
        PaintCx::new(frame).with_child(
            i32::from(area.x),
            i64::from(area.y),
            LocalRect::new(0, 0, area.width, area.height),
            |cx| component.paint(&layout, cx),
        );
    }

    #[test]
    fn renders_closed_selected_option() {
        let options = options();
        let state = Cell::new(SelectDropdownState::new(Some(1)));
        let component = SelectDropdownComponent::new("status", &options, &state);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 14, 1));
        let mut frame = Frame::new(&mut buffer);

        render_component(&component, Rect::new(0, 0, 14, 1), &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("Published ▾   ")
        );
    }

    #[test]
    fn component_measures_closed_and_open_heights() {
        let options = options();
        let state = Cell::new(SelectDropdownState::new(Some(0)));
        let component = SelectDropdownComponent::new("status", &options, &state);
        let mut cx = LayoutCx::new();

        let closed = component.layout(Constraints::for_width(14), &mut cx);
        assert_eq!(closed.size.height, 1);
        assert_eq!(closed.size.width, 14);
        let closed_revision = component.revision();

        let mut opened = state.get();
        opened.set_open(true);
        state.set(opened);
        let open = component.layout(Constraints::for_width(14), &mut cx);
        assert_eq!(open.size.height, 3);
        assert_eq!(cx.measured_nodes(), 2);
        assert_ne!(
            closed_revision.layout,
            component.revision().layout,
            "opening the list changes measured height and must relayout"
        );
    }

    #[test]
    fn render_registers_closed_and_open_composite_geometry() {
        let options = options();
        let mut initial = SelectDropdownState::new(Some(0));
        initial.interaction.focused = true;
        initial.set_open(true);
        let state = Cell::new(initial);
        let component = SelectDropdownComponent::new("status", &options, &state);
        let mut buffer = Buffer::empty(Rect::new(4, 3, 20, 6));
        let mut frame = Frame::new(&mut buffer);

        render_component(&component, Rect::new(7, 4, 14, 3), &mut frame);

        let regions = frame.hits().regions();
        assert_eq!(regions.len(), 4);
        assert_eq!(regions[0].id.as_str(), "status");
        assert_eq!(regions[0].area, Rect::new(7, 4, 14, 1));
        assert_eq!(regions[0].role, HitRole::ListItem);
        assert!(regions[0].focusable);
        assert_eq!(regions[1].id.as_str(), "status.options");
        assert_eq!(regions[1].area, Rect::new(7, 5, 14, 2));
        assert!(!regions[1].focusable);
        assert_eq!(regions[2].id.as_str(), "status.draft");
        assert_eq!(regions[2].area, Rect::new(7, 5, 14, 1));
        assert_eq!(regions[3].id.as_str(), "status.published");
        assert_eq!(regions[3].area, Rect::new(7, 6, 14, 1));
        assert_eq!(frame.hits().focus_targets(None).len(), 1);
        assert_eq!(
            frame.buffer().row_symbols(5).as_deref(),
            Some("   > Draft          ")
        );
        assert!(
            frame
                .semantics()
                .regions()
                .iter()
                .any(|region| region.id == "status.published" && region.role == "option")
        );
    }

    #[test]
    fn open_list_is_clipped_to_the_assigned_area() {
        let options = options();
        let mut initial = SelectDropdownState::new(Some(0));
        initial.set_open(true);
        let state = Cell::new(initial);
        let component = SelectDropdownComponent::new("status", &options, &state);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 14, 2));
        let mut frame = Frame::new(&mut buffer);

        render_component(&component, Rect::new(0, 0, 14, 2), &mut frame);

        let regions = frame.hits().regions();
        assert_eq!(regions[1].area, Rect::new(0, 1, 14, 1));
        assert!(
            regions
                .iter()
                .all(|region| region.id.as_str() != "status.published")
        );
        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("> Draft       ")
        );
    }

    #[test]
    fn disabled_dropdown_is_excluded_from_traversal() {
        let options = options();
        let mut initial = SelectDropdownState::new(Some(0));
        initial.set_disabled(true);
        let state = Cell::new(initial);
        let component = SelectDropdownComponent::new("status", &options, &state);
        let mut buffer = Buffer::empty(Rect::new(2, 2, 16, 2));
        let mut frame = Frame::new(&mut buffer);

        render_component(&component, Rect::new(3, 2, 14, 1), &mut frame);

        assert_eq!(frame.hits().regions().len(), 1);
        assert!(!frame.hits().regions()[0].enabled);
        assert!(frame.hits().focus_targets(None).is_empty());
    }

    #[test]
    fn component_routes_events_through_resolved_layout_and_updates_caller_state() {
        let options = options();
        let mut initial = SelectDropdownState::new(Some(0));
        initial.interaction.focused = true;
        let state = Cell::new(initial);
        let component = SelectDropdownComponent::new("status", &options, &state);
        let layout = component.layout(
            Constraints::tight(Rect::new(0, 0, 14, 3).size()),
            &mut LayoutCx::new(),
        );

        assert_eq!(
            component.event(
                &Event::Key(KeyStroke::simple(KeyCode::Enter)),
                &layout,
                &mut EventCx::new(&layout),
            ),
            EventOutcome::Redraw
        );
        assert!(state.get().is_open());

        let layout = component.layout(
            Constraints::tight(Rect::new(0, 0, 14, 3).size()),
            &mut LayoutCx::new(),
        );
        assert_eq!(
            component.event(
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Down(MouseButton::Left),
                    Point::new(2, 2),
                )),
                &layout,
                &mut EventCx::new(&layout),
            ),
            EventOutcome::Redraw
        );
        assert_eq!(
            component.event(
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Up(MouseButton::Left),
                    Point::new(2, 2),
                )),
                &layout,
                &mut EventCx::new(&layout),
            ),
            EventOutcome::Redraw
        );
        assert_eq!(state.get().selected(), Some(1));
        assert!(!state.get().is_open());
        assert_eq!(
            component.event(&Event::Tick, &layout, &mut EventCx::new(&layout)),
            EventOutcome::Ignored
        );
    }

    #[test]
    fn enter_opens_focused_dropdown() {
        let options = options();
        let select = SelectDropdown::new(&options);
        let mut state = SelectDropdownState::new(Some(0));
        state.interaction.focused = true;

        let outcome = select.handle_event(
            Rect::new(0, 0, 14, 3),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Enter)),
        );

        assert_eq!(outcome, SelectDropdownOutcome::Opened);
        assert!(state.is_open());
    }

    #[test]
    fn open_dropdown_delegates_selection_to_list() {
        let options = options();
        let select = SelectDropdown::new(&options);
        let mut state = SelectDropdownState::new(Some(0));
        state.interaction.focused = true;
        state.set_open(true);

        let moved = select.handle_event(
            Rect::new(0, 0, 14, 3),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Down)),
        );
        let selected = select.handle_event(
            Rect::new(0, 0, 14, 3),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Enter)),
        );

        assert_eq!(moved, SelectDropdownOutcome::Focused(1));
        assert_eq!(selected, SelectDropdownOutcome::Selected(1));
        assert_eq!(state.selected(), Some(1));
        assert!(!state.is_open());
    }

    #[test]
    fn mouse_click_toggles_closed_control() {
        let options = options();
        let select = SelectDropdown::new(&options);
        let mut state = SelectDropdownState::new(Some(0));
        let area = Rect::new(0, 0, 14, 3);

        let down = select.handle_event(
            area,
            &mut state,
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(1, 0),
            )),
        );
        let up = select.handle_event(
            area,
            &mut state,
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Up(MouseButton::Left),
                Point::new(1, 0),
            )),
        );

        assert_eq!(down, SelectDropdownOutcome::Redraw);
        assert_eq!(up, SelectDropdownOutcome::Opened);
        assert!(state.is_open());
    }

    #[test]
    fn disabled_dropdown_ignores_events() {
        let options = options();
        let select = SelectDropdown::new(&options);
        let mut state = SelectDropdownState::new(Some(0));
        state.interaction.focused = true;
        state.set_disabled(true);

        let outcome = select.handle_event(
            Rect::new(0, 0, 14, 3),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Enter)),
        );

        assert_eq!(outcome, SelectDropdownOutcome::Ignored);
        assert!(!state.is_open());
    }

    fn options() -> Vec<SelectOption> {
        vec![
            SelectOption::new("draft", "Draft"),
            SelectOption::new("published", "Published"),
        ]
    }
}
