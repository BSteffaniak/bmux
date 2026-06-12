//! Reusable action-button row component.

use bmux_keyboard::KeyCode;
use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::prelude::Style;

use crate::button::{Button, ButtonState};
use crate::common::{ComponentMousePolicy, InteractionState};

/// One action button in an action row.
#[derive(Debug, Clone, PartialEq, Eq)]
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

    /// Common keyboard action-row behavior.
    #[must_use]
    pub const fn interactive() -> Self {
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
        Self::interactive()
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
    /// Common keyboard and mouse action-row behavior.
    #[must_use]
    pub const fn interactive() -> Self {
        Self {
            mouse: ComponentMousePolicy::button(),
            keyboard: ActionRowKeyboardPolicy::interactive(),
        }
    }
}

impl Default for ActionRowPolicy {
    fn default() -> Self {
        Self::interactive()
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
    focused: usize,
    spacing: u16,
    policy: ActionRowPolicy,
    styles: ActionRowStyles,
}

impl<'a> ActionRow<'a> {
    /// Create an action row.
    #[must_use]
    pub fn new(actions: &'a [ActionButton]) -> Self {
        Self {
            actions,
            focused: 0,
            spacing: 1,
            policy: ActionRowPolicy::interactive(),
            styles: ActionRowStyles::default(),
        }
    }

    /// Set focused action index for stateless rendering.
    #[must_use]
    pub const fn focused(mut self, focused: usize) -> Self {
        self.focused = focused;
        self
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

    /// Render the action row using stateless focused-index configuration.
    pub fn render(&self, area: Rect, frame: &mut Frame<'_>) {
        self.render_actions(area, None, frame, None);
    }

    /// Render the action row with runtime state.
    pub fn render_state(&self, area: Rect, state: &ActionRowState, frame: &mut Frame<'_>) {
        self.render_actions(area, Some(state), frame, None);
    }

    /// Render the action row with a fallback style filling each button area.
    pub fn render_with_fallback_style(&self, area: Rect, frame: &mut Frame<'_>, style: Style) {
        self.render_actions(area, None, frame, Some(style));
    }

    /// Render the action row with runtime state and a fallback style filling each button area.
    pub fn render_state_with_fallback_style(
        &self,
        area: Rect,
        state: &ActionRowState,
        frame: &mut Frame<'_>,
        style: Style,
    ) {
        self.render_actions(area, Some(state), frame, Some(style));
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
            Event::Key(stroke) if stroke.modifiers.is_empty() => self.handle_key(state, stroke.key),
            Event::Mouse(mouse) => self.handle_mouse(area, state, *mouse),
            Event::Key(_)
            | Event::Resize(_)
            | Event::Paste(_)
            | Event::Focus(_)
            | Event::Tick
            | Event::User(_) => ActionRowOutcome::Ignored,
        }
    }

    fn render_actions(
        &self,
        area: Rect,
        state: Option<&ActionRowState>,
        frame: &mut Frame<'_>,
        fallback: Option<Style>,
    ) {
        for (index, action_area) in self.action_areas(area).into_iter().enumerate() {
            let Some(action) = self.actions.get(index) else {
                return;
            };
            let button_state = self.button_state(state, index);
            let button = Button::new(action.label.as_str()).styles(self.styles);
            if let Some(fallback) = fallback {
                button.render_with_fallback_style(action_area, &button_state, frame, fallback);
            } else {
                button.render(action_area, &button_state, frame);
            }
        }
    }

    fn button_state(&self, state: Option<&ActionRowState>, index: usize) -> ButtonState {
        let mut button_state = ButtonState::new();
        if let Some(state) = state {
            button_state.set_focused(state.focused == Some(index));
            button_state.interaction.hovered = state.hovered == Some(index);
            button_state.interaction.pressed = state.pressed == Some(index);
            button_state.interaction.disabled = state.interaction.disabled;
        } else {
            button_state.set_focused(index == self.focused);
        }
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
            KeyCode::Tab if self.policy.keyboard.tab_navigation() => {
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
        if !self.policy.mouse.enabled {
            return ActionRowOutcome::Ignored;
        }
        let hit = self.action_index_at(area, mouse.position);
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

    fn action_index_at(&self, area: Rect, point: Point) -> Option<usize> {
        self.action_areas(area)
            .into_iter()
            .position(|action_area| action_area.contains(point))
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
    use bmux_keyboard::{KeyCode, KeyStroke};
    use bmux_tui::buffer::Buffer;
    use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};

    use super::{ActionButton, ActionRow, ActionRowOutcome, ActionRowState};

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
    fn renders_buttons() {
        let actions = [ActionButton::new("approve", "Approve")];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 1));
        let mut frame = Frame::new(&mut buffer);

        ActionRow::new(&actions).render(Rect::new(0, 0, 12, 1), &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("[ Approve ] ")
        );
    }

    #[test]
    fn keyboard_navigation_moves_focus() {
        let actions = [
            ActionButton::new("approve", "Approve"),
            ActionButton::new("deny", "Deny"),
        ];
        let row = ActionRow::new(&actions);
        let mut state = ActionRowState::new();
        state.set_focused(Some(0));

        let outcome = row.handle_event(Rect::new(0, 0, 30, 1), &mut state, &key(KeyCode::Right));

        assert_eq!(outcome, ActionRowOutcome::FocusMoved { index: 1 });
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

    fn key(key: KeyCode) -> Event {
        Event::Key(KeyStroke::simple(key))
    }

    fn mouse(kind: MouseEventKind, x: u16, y: u16) -> Event {
        Event::Mouse(MouseEvent::new(kind, Point::new(x, y)))
    }
}
