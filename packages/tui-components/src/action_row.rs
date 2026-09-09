//! Reusable action-button row component.

use std::cell::Cell;
use std::hash::{Hash, Hasher};

use bmux_keyboard::KeyCode;
use bmux_tui::component::{
    Component, ComponentRevision, Constraints, EventCx, LayoutCx, LayoutId, LayoutMetadata,
    LayoutNode, LogicalSize,
};
use bmux_tui::event::{Event, EventOutcome, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::hit::{HitRegion as SceneRegion, HitRole};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::semantic::SemanticRegion;

use crate::button::{Button, ButtonState};
use crate::common::{ComponentMousePolicy, InteractionState};
use crate::hit_test::{HitRegion, hit_region_at};

/// One action button in an action row.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ActionButton {
    /// Stable action id chosen by the caller.
    pub id: String,
    /// Visible button label.
    pub label: String,
}

impl ActionButton {
    /// Create an action button.
    #[must_use]
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
        }
    }
}

/// Visual styles for an action row.
pub type ActionRowStyles = crate::button::ButtonStyles;

/// Runtime action-row state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ActionRowState {
    /// Common row interaction flags.
    pub interaction: InteractionState,
    focused: Option<usize>,
    hovered: Option<usize>,
    pressed: Option<usize>,
}

impl ActionRowState {
    /// Create enabled action-row state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            interaction: InteractionState::new(),
            focused: None,
            hovered: None,
            pressed: None,
        }
    }

    /// Return the focused action index.
    #[must_use]
    pub const fn focused(self) -> Option<usize> {
        self.focused
    }

    /// Set focused action index.
    pub const fn set_focused(&mut self, focused: Option<usize>) {
        self.focused = focused;
        self.interaction.focused = focused.is_some();
    }

    /// Return the hovered action index.
    #[must_use]
    pub const fn hovered(self) -> Option<usize> {
        self.hovered
    }

    /// Return the pressed action index.
    #[must_use]
    pub const fn pressed(self) -> Option<usize> {
        self.pressed
    }

    /// Set disabled state.
    pub const fn set_disabled(&mut self, disabled: bool) {
        self.interaction.disabled = disabled;
        if disabled {
            self.hovered = None;
            self.pressed = None;
        }
    }
}

/// Keyboard behavior for an action row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionRowKeyboardPolicy {
    flags: u8,
}

impl ActionRowKeyboardPolicy {
    const ARROW_NAVIGATION: u8 = 1 << 0;
    const TAB_NAVIGATION: u8 = 1 << 1;
    const WRAP_NAVIGATION: u8 = 1 << 2;
    const ENTER_ACTIVATES: u8 = 1 << 3;
    const SPACE_ACTIVATES: u8 = 1 << 4;

    /// Global action-row behavior: each rendered button is an independent tab stop.
    #[must_use]
    pub const fn global() -> Self {
        Self {
            flags: Self::ENTER_ACTIVATES | Self::SPACE_ACTIVATES,
        }
    }

    /// Deliberate local roving behavior with arrow navigation and wrapping.
    #[must_use]
    pub const fn roving() -> Self {
        Self {
            flags: Self::ARROW_NAVIGATION
                | Self::TAB_NAVIGATION
                | Self::WRAP_NAVIGATION
                | Self::ENTER_ACTIVATES
                | Self::SPACE_ACTIVATES,
        }
    }

    /// Return true when Left/Right move focus.
    #[must_use]
    pub const fn arrow_navigation(self) -> bool {
        self.flags & Self::ARROW_NAVIGATION != 0
    }

    /// Return true when Tab moves focus.
    #[must_use]
    pub const fn tab_navigation(self) -> bool {
        self.flags & Self::TAB_NAVIGATION != 0
    }

    /// Return true when focus wraps at row ends.
    #[must_use]
    pub const fn wrap_navigation(self) -> bool {
        self.flags & Self::WRAP_NAVIGATION != 0
    }

    /// Return true when Enter activates the focused action.
    #[must_use]
    pub const fn enter_activates(self) -> bool {
        self.flags & Self::ENTER_ACTIVATES != 0
    }

    /// Return true when Space activates the focused action.
    #[must_use]
    pub const fn space_activates(self) -> bool {
        self.flags & Self::SPACE_ACTIVATES != 0
    }
}

impl Default for ActionRowKeyboardPolicy {
    fn default() -> Self {
        Self::global()
    }
}

/// Configurable action-row behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionRowPolicy {
    /// Mouse behavior.
    pub mouse: ComponentMousePolicy,
    /// Keyboard behavior.
    pub keyboard: ActionRowKeyboardPolicy,
}

impl ActionRowPolicy {
    /// Default global keyboard and mouse action-row behavior.
    #[must_use]
    pub const fn global() -> Self {
        Self {
            mouse: ComponentMousePolicy::button(),
            keyboard: ActionRowKeyboardPolicy::global(),
        }
    }

    /// Deliberate local roving keyboard and mouse behavior.
    #[must_use]
    pub const fn roving() -> Self {
        Self {
            mouse: ComponentMousePolicy::button(),
            keyboard: ActionRowKeyboardPolicy::roving(),
        }
    }
}

impl Default for ActionRowPolicy {
    fn default() -> Self {
        Self::global()
    }
}

/// Outcome from handling an action-row event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionRowOutcome {
    /// Event was not handled.
    Ignored,
    /// Event was handled without requiring redraw.
    Handled,
    /// Event was handled and requires redraw.
    Redraw,
    /// Row focus was requested.
    FocusRequested { index: usize },
    /// Focus moved to another action.
    FocusMoved { index: usize },
    /// An action was activated.
    Activated { index: usize, id: String },
}

impl ActionRowOutcome {
    /// Return true when the event was handled.
    #[must_use]
    pub const fn is_handled(&self) -> bool {
        matches!(
            self,
            Self::Handled
                | Self::Redraw
                | Self::FocusRequested { .. }
                | Self::FocusMoved { .. }
                | Self::Activated { .. }
        )
    }

    /// Return true when rendering should be refreshed.
    #[must_use]
    pub const fn needs_redraw(&self) -> bool {
        matches!(
            self,
            Self::Redraw
                | Self::FocusRequested { .. }
                | Self::FocusMoved { .. }
                | Self::Activated { .. }
        )
    }
}

/// Horizontal action-button row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionRow<'a> {
    actions: &'a [ActionButton],
    spacing: u16,
    policy: ActionRowPolicy,
    styles: ActionRowStyles,
}

/// Canonical component-lifecycle action row.
pub struct ActionRowComponent<'a, 'state> {
    id: LayoutId,
    row: ActionRow<'a>,
    state: &'state Cell<ActionRowState>,
}

impl<'a, 'state> ActionRowComponent<'a, 'state> {
    /// Create an action row with stable identity and caller-owned state.
    #[must_use]
    pub fn new(
        id: impl Into<LayoutId>,
        actions: &'a [ActionButton],
        state: &'state Cell<ActionRowState>,
    ) -> Self {
        Self {
            id: id.into(),
            row: ActionRow::new(actions),
            state,
        }
    }

    /// Set horizontal spacing.
    #[must_use]
    pub const fn spacing(mut self, spacing: u16) -> Self {
        self.row.spacing = spacing;
        self
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: ActionRowPolicy) -> Self {
        self.row.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: ActionRowStyles) -> Self {
        self.row.styles = styles;
        self
    }
}

impl Component for ActionRowComponent<'_, '_> {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        self.row.actions.hash(&mut layout);
        self.row.spacing.hash(&mut layout);

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        self.row.policy.mouse.enabled.hash(&mut paint);
        self.row.policy.mouse.hover.hash(&mut paint);
        self.row.policy.mouse.click.hash(&mut paint);
        self.row.policy.keyboard.flags.hash(&mut paint);
        self.row.styles.normal.hash(&mut paint);
        self.row.styles.focused.hash(&mut paint);
        self.row.styles.hovered.hash(&mut paint);
        self.row.styles.pressed.hash(&mut paint);
        self.row.styles.disabled.hash(&mut paint);
        let state = self.state.get();
        state.interaction.focused.hash(&mut paint);
        state.interaction.hovered.hash(&mut paint);
        state.interaction.pressed.hash(&mut paint);
        state.interaction.disabled.hash(&mut paint);
        state.focused.hash(&mut paint);
        state.hovered.hash(&mut paint);
        state.pressed.hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let width = self
            .row
            .actions
            .iter()
            .map(action_width)
            .fold(0_u16, u16::saturating_add)
            .saturating_add(self.row.spacing.saturating_mul(
                u16::try_from(self.row.actions.len().saturating_sub(1)).unwrap_or(u16::MAX),
            ));
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(width.into(), 1)),
        )
        .with_metadata(LayoutMetadata::new().semantic("actions"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        if layout.size.width == 0 || layout.size.height == 0 {
            return;
        }
        let state = self.state.get();
        let area = Rect::new(0, 0, layout.size.width.try_into().unwrap_or(u16::MAX), 1);
        for (index, action_area) in self.row.action_areas(area).into_iter().enumerate() {
            let Some(action) = self.row.actions.get(index) else {
                break;
            };
            let button_state = ActionRow::button_state(&state, index);
            let style = self.row.style_for(button_state);
            cx.write_line(
                LocalRect::new(i32::from(action_area.x), 0, action_area.width, 1),
                &Line::from_spans([Span::styled(format!("[ {} ]", action.label), style)]),
            );
            cx.push_hit(
                SceneRegion::new(format!("{}.{}", self.id.as_str(), action.id), action_area)
                    .role(HitRole::Action)
                    .pointer_events(self.row.policy.mouse.enabled)
                    .hoverable(self.row.policy.mouse.enabled && self.row.policy.mouse.hover)
                    .focusable(true)
                    .enabled(!state.interaction.disabled),
            );
        }
        cx.push_semantic(SemanticRegion::new(self.id.as_str(), area, "actions"));
        cx.push_damage(LocalRect::new(
            0,
            0,
            layout.size.width.try_into().unwrap_or(u16::MAX),
            1,
        ));
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        let Some(area) = cx.find_rect(&layout.id).filter(|area| !area.is_empty()) else {
            return EventOutcome::Ignored;
        };
        let mut state = self.state.get();
        let outcome = if let Event::Mouse(mouse) = event {
            let hit = self
                .row
                .action_areas(Rect::new(
                    0,
                    0,
                    layout.size.width.try_into().unwrap_or(u16::MAX),
                    1,
                ))
                .iter()
                .position(|area| {
                    cx.visible_rect(bmux_tui::component::LogicalRect::new(
                        area.x.into(),
                        u64::try_from(usize::from(area.y)).unwrap_or(u64::MAX),
                        u64::try_from(usize::from(area.width)).unwrap_or(u64::MAX),
                        u64::try_from(usize::from(area.height)).unwrap_or(u64::MAX),
                    ))
                    .contains(mouse.position)
                });
            if state.interaction.disabled {
                ActionRowOutcome::Ignored
            } else {
                self.row.handle_mouse_hit(&mut state, *mouse, hit)
            }
        } else {
            self.row.handle_event(area, &mut state, event)
        };
        self.state.set(state);
        match outcome {
            ActionRowOutcome::Ignored => EventOutcome::Ignored,
            ActionRowOutcome::Handled => EventOutcome::Handled,
            ActionRowOutcome::Redraw
            | ActionRowOutcome::FocusRequested { .. }
            | ActionRowOutcome::FocusMoved { .. }
            | ActionRowOutcome::Activated { .. } => EventOutcome::Redraw,
        }
    }
}

impl<'a> ActionRow<'a> {
    /// Create an action row.
    #[must_use]
    pub fn new(actions: &'a [ActionButton]) -> Self {
        Self {
            actions,
            spacing: 1,
            policy: ActionRowPolicy::default(),
            styles: ActionRowStyles::default(),
        }
    }

    /// Set horizontal spacing between buttons.
    #[must_use]
    pub const fn spacing(mut self, spacing: u16) -> Self {
        self.spacing = spacing;
        self
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: ActionRowPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set row styles.
    #[must_use]
    pub const fn styles(mut self, styles: ActionRowStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Return button hit boxes for this row in `area`.
    #[must_use]
    pub fn action_areas(&self, area: Rect) -> Vec<Rect> {
        let mut x = area.x;
        let mut areas = Vec::with_capacity(self.actions.len());
        for action in self.actions {
            if x >= area.right() {
                break;
            }
            let width = action_width(action).min(area.right().saturating_sub(x));
            areas.push(Rect::new(x, area.y, width, area.height.min(1)));
            x = x.saturating_add(width).saturating_add(self.spacing);
        }
        areas
    }

    /// Handle one input event.
    pub fn handle_event(
        &self,
        area: Rect,
        state: &mut ActionRowState,
        event: &Event,
    ) -> ActionRowOutcome {
        if state.interaction.disabled || self.actions.is_empty() {
            return ActionRowOutcome::Ignored;
        }
        match event {
            Event::Key(stroke) if stroke.modifiers.is_empty() && stroke.key != KeyCode::Tab => {
                self.handle_key(state, stroke.key)
            }
            Event::Mouse(mouse) => self.handle_mouse(area, state, *mouse),
            Event::Key(_)
            | Event::Resize(_)
            | Event::Paste(_)
            | Event::Focus(_)
            | Event::Tick
            | Event::User(_) => ActionRowOutcome::Ignored,
        }
    }

    const fn style_for(&self, state: ButtonState) -> Style {
        if state.interaction.disabled {
            self.styles.disabled
        } else if state.interaction.pressed {
            self.styles.pressed
        } else if state.interaction.hovered {
            self.styles.hovered
        } else if state.interaction.focused {
            self.styles.focused
        } else {
            self.styles.normal
        }
    }

    fn button_state(state: &ActionRowState, index: usize) -> ButtonState {
        let mut button_state = ButtonState::new();
        button_state.set_focused(state.focused == Some(index));
        button_state.interaction.hovered = state.hovered == Some(index);
        button_state.interaction.pressed = state.pressed == Some(index);
        button_state.interaction.disabled = state.interaction.disabled;
        button_state
    }

    fn handle_key(&self, state: &mut ActionRowState, key: KeyCode) -> ActionRowOutcome {
        match key {
            KeyCode::Left if self.policy.keyboard.arrow_navigation() => {
                self.move_focus(state, Direction::Previous)
            }
            KeyCode::Right if self.policy.keyboard.arrow_navigation() => {
                self.move_focus(state, Direction::Next)
            }
            KeyCode::Enter if self.policy.keyboard.enter_activates() => {
                self.activate_focused(state)
            }
            KeyCode::Space | KeyCode::Char(' ') if self.policy.keyboard.space_activates() => {
                self.activate_focused(state)
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
            | KeyCode::F(_) => ActionRowOutcome::Ignored,
        }
    }

    fn handle_mouse(
        &self,
        area: Rect,
        state: &mut ActionRowState,
        mouse: MouseEvent,
    ) -> ActionRowOutcome {
        let hit = self.action_index_at(area, mouse.position);
        self.handle_mouse_hit(state, mouse, hit)
    }

    fn handle_mouse_hit(
        &self,
        state: &mut ActionRowState,
        mouse: MouseEvent,
        hit: Option<usize>,
    ) -> ActionRowOutcome {
        if !self.policy.mouse.enabled {
            return ActionRowOutcome::Ignored;
        }
        match mouse.kind {
            MouseEventKind::Move if self.policy.mouse.hover => {
                if state.hovered == hit {
                    ActionRowOutcome::Handled
                } else {
                    state.hovered = hit;
                    ActionRowOutcome::Redraw
                }
            }
            MouseEventKind::Down(MouseButton::Left) if self.policy.mouse.click => {
                let Some(index) = hit else {
                    return ActionRowOutcome::Ignored;
                };
                state.pressed = Some(index);
                state.hovered = Some(index);
                state.set_focused(Some(index));
                ActionRowOutcome::FocusRequested { index }
            }
            MouseEventKind::Drag(MouseButton::Left) if state.pressed.is_some() => {
                if state.hovered == hit {
                    ActionRowOutcome::Handled
                } else {
                    state.hovered = hit;
                    ActionRowOutcome::Redraw
                }
            }
            MouseEventKind::Up(MouseButton::Left) if state.pressed.is_some() => {
                let pressed = state.pressed.take();
                state.hovered = hit;
                if let (Some(index), true) = (pressed, pressed == hit) {
                    self.activate(index)
                } else {
                    ActionRowOutcome::Redraw
                }
            }
            MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::Move
            | MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => ActionRowOutcome::Ignored,
        }
    }

    fn move_focus(&self, state: &mut ActionRowState, direction: Direction) -> ActionRowOutcome {
        let Some(index) = next_index(
            state.focused.unwrap_or(0),
            self.actions.len(),
            direction,
            self.policy.keyboard.wrap_navigation(),
        ) else {
            return ActionRowOutcome::Handled;
        };
        state.set_focused(Some(index));
        ActionRowOutcome::FocusMoved { index }
    }

    fn activate_focused(&self, state: &mut ActionRowState) -> ActionRowOutcome {
        let index = state
            .focused
            .unwrap_or(0)
            .min(self.actions.len().saturating_sub(1));
        state.set_focused(Some(index));
        self.activate(index)
    }

    fn activate(&self, index: usize) -> ActionRowOutcome {
        let Some(action) = self.actions.get(index) else {
            return ActionRowOutcome::Ignored;
        };
        ActionRowOutcome::Activated {
            index,
            id: action.id.clone(),
        }
    }

    fn action_hit_regions(&self, area: Rect) -> Vec<HitRegion<usize>> {
        self.action_areas(area)
            .into_iter()
            .enumerate()
            .map(|(index, rect)| HitRegion::new(index, rect))
            .collect()
    }

    fn action_index_at(&self, area: Rect, point: Point) -> Option<usize> {
        hit_region_at(&self.action_hit_regions(area), point).map(|region| region.key)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Previous,
    Next,
}

const fn next_index(current: usize, len: usize, direction: Direction, wrap: bool) -> Option<usize> {
    if len == 0 {
        return None;
    }
    match direction {
        Direction::Previous if current == 0 && wrap => Some(len - 1),
        Direction::Previous if current == 0 => Some(0),
        Direction::Previous => Some(current.saturating_sub(1)),
        Direction::Next if current + 1 >= len && wrap => Some(0),
        Direction::Next if current + 1 >= len => Some(len - 1),
        Direction::Next => Some(current + 1),
    }
}

fn action_width(action: &ActionButton) -> u16 {
    Button::new(action.label.as_str()).width()
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use bmux_keyboard::{KeyCode, KeyStroke};
    use bmux_tui::buffer::Buffer;
    use bmux_tui::component::{Component, Constraints, LayoutCx};
    use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect, Size};
    use bmux_tui::paint::PaintCx;

    use super::{
        ActionButton, ActionRow, ActionRowComponent, ActionRowOutcome, ActionRowPolicy,
        ActionRowState,
    };

    #[test]
    fn mouse_disabled_row_retains_keyboard_focus_without_pointer_regions() {
        let actions = [ActionButton::new("ok", "OK")];
        let state = Cell::new(ActionRowState::new());
        let mut policy = ActionRowPolicy::global();
        policy.mouse.enabled = false;
        let component = ActionRowComponent::new("actions", &actions, &state).policy(policy);
        let layout = component.layout(Constraints::tight(Size::new(6, 1)), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 1));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        let hit = &frame.hits().regions()[0];
        assert!(hit.focusable);
        assert!(hit.enabled);
        assert!(!hit.pointer_events);
        assert!(!hit.hoverable);
        let outcome = component.event(
            &mouse(MouseEventKind::Down(MouseButton::Left), 1, 0),
            &layout,
            &mut bmux_tui::component::EventCx::new(&layout),
        );
        assert!(!outcome.is_handled());
        assert_eq!(state.get(), ActionRowState::new());
    }

    #[test]
    fn horizontal_clip_preserves_action_identity() {
        let actions = [
            ActionButton::new("first", "One"),
            ActionButton::new("second", "Two"),
        ];
        let state = Cell::new(ActionRowState::new());
        let component = ActionRowComponent::new("actions", &actions, &state);
        let layout = component.layout(Constraints::tight(Size::new(15, 1)), &mut LayoutCx::new());
        let mut cx = bmux_tui::component::EventCx::new(&layout);
        let outcome = cx.with_transform(0, 0, -9, 0, Rect::new(0, 0, 6, 1), |cx| {
            component.event(
                &mouse(MouseEventKind::Down(MouseButton::Left), 1, 0),
                &layout,
                cx,
            )
        });
        assert!(outcome.is_handled());
        assert_eq!(state.get().focused(), Some(1));
        assert_eq!(state.get().pressed(), Some(1));
    }

    #[test]
    fn interaction_state_changes_repaint_without_remeasurement() {
        let actions = [ActionButton::new("ok", "OK")];
        let state = Cell::new(ActionRowState::new());
        let component = ActionRowComponent::new("actions", &actions, &state);
        let initial = component.revision();
        let mut cache = bmux_tui::component::LayoutCache::new();
        let mut cx = LayoutCx::new();
        let constraints = Constraints::tight(Size::new(6, 1));
        cache.layout("actions".into(), &component, constraints, &mut cx);
        for index in 0..7 {
            let mut changed = ActionRowState::new();
            match index {
                0 => changed.interaction.focused = true,
                1 => changed.interaction.hovered = true,
                2 => changed.interaction.pressed = true,
                3 => changed.interaction.disabled = true,
                4 => changed.focused = Some(0),
                5 => changed.hovered = Some(0),
                _ => changed.pressed = Some(0),
            }
            state.set(changed);
            assert_eq!(component.revision().layout, initial.layout);
            assert_ne!(component.revision().paint, initial.paint);
            cache.layout("actions".into(), &component, constraints, &mut cx);
        }
        assert_eq!(cx.measured_nodes(), 1);
        assert_eq!(cache.stats().hits, 7);
    }

    #[test]
    fn retained_layout_invalidates_action_labels_and_identity() {
        let initial = [ActionButton::new("ok", "OK")];
        let relabeled = [ActionButton::new("ok", "Confirm")];
        let renamed = [ActionButton::new("confirm", "Confirm")];
        let state = Cell::new(ActionRowState::new());
        let mut cache = bmux_tui::component::LayoutCache::new();
        let mut cx = LayoutCx::new();
        let constraints = Constraints::new(0, 40, 0, Some(1));
        for (actions, width) in [(&initial, 6), (&relabeled, 11), (&renamed, 11)] {
            let component = ActionRowComponent::new("actions", actions, &state);
            let layout = cache.layout("actions".into(), &component, constraints, &mut cx);
            assert_eq!(layout.size.width, width);
        }
        assert_eq!(cx.measured_nodes(), 3);
        let component = ActionRowComponent::new("actions", &renamed, &state);
        cache.layout("actions".into(), &component, constraints, &mut cx);
        assert_eq!(cx.measured_nodes(), 3);
        assert_eq!(cache.stats().hits, 1);
    }

    #[test]
    fn empty_component_geometry_cannot_activate_actions() {
        let actions = [ActionButton::new("approve", "Approve")];
        let mut initial = ActionRowState::new();
        initial.set_focused(Some(0));
        let state = Cell::new(initial);
        let component = ActionRowComponent::new("actions", &actions, &state);
        for size in [Size::new(0, 1), Size::new(20, 0), Size::new(0, 0)] {
            let layout = component.layout(Constraints::tight(size), &mut LayoutCx::new());
            let outcome = component.event(
                &key(KeyCode::Enter),
                &layout,
                &mut bmux_tui::component::EventCx::new(&layout),
            );
            assert!(!outcome.is_handled());
            assert_eq!(state.get(), initial);
        }
    }

    #[test]
    fn action_areas_follow_rendered_button_widths() {
        let actions = [
            ActionButton::new("approve", "Approve"),
            ActionButton::new("deny", "Deny"),
        ];
        let row = ActionRow::new(&actions).spacing(2);

        let areas = row.action_areas(Rect::new(3, 4, 30, 1));

        assert_eq!(areas, vec![Rect::new(3, 4, 11, 1), Rect::new(16, 4, 8, 1)]);
    }

    #[test]
    fn default_global_row_yields_arrow_navigation_to_global_routing() {
        let actions = [
            ActionButton::new("approve", "Approve"),
            ActionButton::new("deny", "Deny"),
        ];
        let row = ActionRow::new(&actions);
        let mut state = ActionRowState::new();
        state.set_focused(Some(0));

        let outcome = row.handle_event(Rect::new(0, 0, 30, 1), &mut state, &key(KeyCode::Right));

        assert_eq!(outcome, ActionRowOutcome::Ignored);
        assert_eq!(state.focused(), Some(0));
    }

    #[test]
    fn explicitly_roving_row_moves_focus_with_arrows() {
        let actions = [
            ActionButton::new("approve", "Approve"),
            ActionButton::new("deny", "Deny"),
        ];
        let row = ActionRow::new(&actions).policy(ActionRowPolicy::roving());
        let mut state = ActionRowState::new();
        state.set_focused(Some(0));

        let outcome = row.handle_event(Rect::new(0, 0, 30, 1), &mut state, &key(KeyCode::Right));

        assert_eq!(outcome, ActionRowOutcome::FocusMoved { index: 1 });
        assert_eq!(state.focused(), Some(1));

        let wrapped = row.handle_event(Rect::new(0, 0, 30, 1), &mut state, &key(KeyCode::Right));
        assert_eq!(wrapped, ActionRowOutcome::FocusMoved { index: 0 });
        assert_eq!(state.focused(), Some(0));

        let reverse_wrapped =
            row.handle_event(Rect::new(0, 0, 30, 1), &mut state, &key(KeyCode::Left));
        assert_eq!(reverse_wrapped, ActionRowOutcome::FocusMoved { index: 1 });
        assert_eq!(state.focused(), Some(1));
    }

    #[test]
    fn keyboard_activation_returns_action_id() {
        let actions = [ActionButton::new("approve", "Approve")];
        let row = ActionRow::new(&actions);
        let mut state = ActionRowState::new();
        state.set_focused(Some(0));

        let outcome = row.handle_event(Rect::new(0, 0, 12, 1), &mut state, &key(KeyCode::Enter));

        assert_eq!(
            outcome,
            ActionRowOutcome::Activated {
                index: 0,
                id: "approve".to_owned()
            }
        );
    }

    #[test]
    fn mouse_click_focuses_and_activates_action() {
        let actions = [
            ActionButton::new("approve", "Approve"),
            ActionButton::new("deny", "Deny"),
        ];
        let row = ActionRow::new(&actions).spacing(2);
        let mut state = ActionRowState::new();
        let area = Rect::new(0, 0, 30, 1);

        let down = row.handle_event(
            area,
            &mut state,
            &mouse(MouseEventKind::Down(MouseButton::Left), 13, 0),
        );
        let up = row.handle_event(
            area,
            &mut state,
            &mouse(MouseEventKind::Up(MouseButton::Left), 13, 0),
        );

        assert_eq!(down, ActionRowOutcome::FocusRequested { index: 1 });
        assert_eq!(state.focused(), Some(1));
        assert_eq!(
            up,
            ActionRowOutcome::Activated {
                index: 1,
                id: "deny".to_owned()
            }
        );
    }

    #[test]
    fn disabled_row_ignores_events() {
        let actions = [ActionButton::new("approve", "Approve")];
        let row = ActionRow::new(&actions);
        let mut state = ActionRowState::new();
        state.set_disabled(true);

        let outcome = row.handle_event(Rect::new(0, 0, 12, 1), &mut state, &key(KeyCode::Enter));

        assert_eq!(outcome, ActionRowOutcome::Ignored);
    }

    #[test]
    fn canonical_component_uses_one_layout_for_all_channels() {
        let actions = [
            ActionButton::new("approve", "Approve"),
            ActionButton::new("deny", "Deny"),
        ];
        let state = Cell::new(ActionRowState::new());
        let row = ActionRowComponent::new("actions", &actions, &state).spacing(2);
        let mut layout_cx = LayoutCx::new();
        let layout = row.layout(Constraints::loose(Size::new(30, 2)), &mut layout_cx);
        assert_eq!(layout.size, bmux_tui::component::LogicalSize::new(21, 1));
        assert_eq!(layout.metadata.semantics, ["actions"]);

        let mut buffer = Buffer::empty(Rect::new(0, 0, 30, 2));
        let mut frame = Frame::new(&mut buffer);
        row.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(frame.hits().regions()[0].area, Rect::new(0, 0, 11, 1));
        assert_eq!(frame.hits().regions()[1].area, Rect::new(13, 0, 8, 1));
        assert_eq!(frame.semantics().regions()[0].area, Rect::new(0, 0, 21, 1));
    }

    #[test]
    fn canonical_component_revision_separates_geometry_and_paint() {
        let actions = [ActionButton::new("approve", "Approve")];
        let state = Cell::new(ActionRowState::new());
        let initial = ActionRowComponent::new("actions", &actions, &state).revision();
        let mut focused_state = ActionRowState::new();
        focused_state.set_focused(Some(0));
        state.set(focused_state);
        let focused = ActionRowComponent::new("actions", &actions, &state).revision();
        assert_eq!(initial.layout, focused.layout);
        assert_ne!(initial.paint, focused.paint);
        assert_ne!(
            initial.layout,
            ActionRowComponent::new("actions", &actions, &state)
                .spacing(2)
                .revision()
                .layout
        );
    }

    fn key(key: KeyCode) -> Event {
        Event::Key(KeyStroke::simple(key))
    }

    fn mouse(kind: MouseEventKind, x: u16, y: u16) -> Event {
        Event::Mouse(MouseEvent::new(kind, Point::new(x, y)))
    }
}
