//! Configurable checkbox component.

use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::Modifier;

use crate::common::{ComponentMousePolicy, InteractionState};

/// Visual styles for a checkbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckboxStyles {
    /// Style used when the checkbox is enabled and inactive.
    pub normal: Style,
    /// Style used when the checkbox has keyboard focus.
    pub focused: Style,
    /// Style used when the pointer is hovering the checkbox.
    pub hovered: Style,
    /// Style used while the primary pointer/button is pressed.
    pub pressed: Style,
    /// Style used when the checkbox is disabled.
    pub disabled: Style,
}

impl Default for CheckboxStyles {
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

/// Configurable checkbox behavior policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckboxPolicy {
    /// Mouse behavior.
    pub mouse: ComponentMousePolicy,
    /// Whether Enter toggles the checkbox when focused.
    pub enter_toggles: bool,
    /// Whether Space toggles the checkbox when focused.
    pub space_toggles: bool,
}

impl CheckboxPolicy {
    /// Common interactive checkbox behavior.
    #[must_use]
    pub const fn interactive() -> Self {
        Self {
            mouse: ComponentMousePolicy::button(),
            enter_toggles: true,
            space_toggles: true,
        }
    }
}

impl Default for CheckboxPolicy {
    fn default() -> Self {
        Self::interactive()
    }
}

/// Runtime checkbox state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckboxState {
    checked: bool,
    interaction: InteractionState,
}

impl CheckboxState {
    /// Create checkbox state.
    #[must_use]
    pub const fn new(checked: bool) -> Self {
        Self {
            checked,
            interaction: InteractionState::new(),
        }
    }

    /// Return whether the checkbox is checked.
    #[must_use]
    pub const fn checked(self) -> bool {
        self.checked
    }

    /// Set checked state.
    pub const fn set_checked(&mut self, checked: bool) {
        self.checked = checked;
    }

    /// Return interaction state.
    #[must_use]
    pub const fn interaction(self) -> InteractionState {
        self.interaction
    }

    /// Set focused state.
    pub const fn set_focused(&mut self, focused: bool) {
        self.interaction.focused = focused;
    }

    /// Set disabled state.
    pub const fn set_disabled(&mut self, disabled: bool) {
        self.interaction.disabled = disabled;
    }
}

/// Outcome from checkbox input handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckboxOutcome {
    /// Event was not handled.
    Ignored,
    /// Visual state changed without changing checked value.
    Redraw,
    /// Checked state changed to the contained value.
    Toggled(bool),
}

/// Configurable checkbox control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checkbox<'a> {
    label: &'a str,
    policy: CheckboxPolicy,
    styles: CheckboxStyles,
}

impl<'a> Checkbox<'a> {
    /// Create a checkbox with a label.
    #[must_use]
    pub fn new(label: &'a str) -> Self {
        Self {
            label,
            policy: CheckboxPolicy::default(),
            styles: CheckboxStyles::default(),
        }
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: CheckboxPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: CheckboxStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Return rendered checkbox width.
    #[must_use]
    pub fn width(&self) -> u16 {
        u16::try_from(bmux_tui::text_width::display_width(self.label))
            .unwrap_or(u16::MAX)
            .saturating_add(4)
    }

    /// Render the checkbox.
    pub fn render(&self, area: Rect, state: &CheckboxState, frame: &mut Frame<'_>) {
        frame.write_line(area, &self.line(*state));
    }

    /// Render the checkbox with a fallback style filling its area.
    pub fn render_with_fallback_style(
        &self,
        area: Rect,
        state: &CheckboxState,
        frame: &mut Frame<'_>,
        fallback: Style,
    ) {
        frame.write_line_with_fallback_style(area, &self.line(*state), fallback);
    }

    /// Handle one input event.
    pub const fn handle_event(
        &self,
        area: Rect,
        state: &mut CheckboxState,
        event: &Event,
    ) -> CheckboxOutcome {
        if state.interaction.disabled {
            return CheckboxOutcome::Ignored;
        }
        match event {
            Event::Key(stroke) => self.handle_key(state, *stroke),
            Event::Mouse(mouse) => self.handle_mouse(area, state, *mouse),
            Event::Resize(_) | Event::Paste(_) | Event::Focus(_) | Event::Tick | Event::User(_) => {
                CheckboxOutcome::Ignored
            }
        }
    }

    fn line(&self, state: CheckboxState) -> Line {
        let mark = if state.checked { 'x' } else { ' ' };
        Line::from_spans(vec![Span::styled(
            format!("[{mark}] {}", self.label),
            self.style_for(state),
        )])
    }

    const fn style_for(&self, state: CheckboxState) -> Style {
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

    const fn handle_key(&self, state: &mut CheckboxState, stroke: KeyStroke) -> CheckboxOutcome {
        if !state.interaction.focused || !stroke.modifiers.is_empty() {
            return CheckboxOutcome::Ignored;
        }
        match stroke.key {
            KeyCode::Enter if self.policy.enter_toggles => toggle(state),
            KeyCode::Space | KeyCode::Char(' ') if self.policy.space_toggles => toggle(state),
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
            | KeyCode::F(_) => CheckboxOutcome::Ignored,
        }
    }

    const fn handle_mouse(
        &self,
        area: Rect,
        state: &mut CheckboxState,
        mouse: MouseEvent,
    ) -> CheckboxOutcome {
        if !self.policy.mouse.enabled {
            return CheckboxOutcome::Ignored;
        }
        let inside = area.contains(mouse.position);
        match mouse.kind {
            MouseEventKind::Move if self.policy.mouse.hover => {
                if state.interaction.hovered == inside {
                    CheckboxOutcome::Ignored
                } else {
                    state.interaction.hovered = inside;
                    CheckboxOutcome::Redraw
                }
            }
            MouseEventKind::Down(MouseButton::Left) if self.policy.mouse.click && inside => {
                state.interaction.pressed = true;
                state.interaction.hovered = true;
                CheckboxOutcome::Redraw
            }
            MouseEventKind::Up(MouseButton::Left) if self.policy.mouse.click => {
                let was_pressed = state.interaction.pressed;
                state.interaction.pressed = false;
                if was_pressed && inside {
                    toggle(state)
                } else if was_pressed {
                    CheckboxOutcome::Redraw
                } else {
                    CheckboxOutcome::Ignored
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.policy.mouse.click => {
                let pressed = state.interaction.pressed && inside;
                if state.interaction.hovered != inside || state.interaction.pressed != pressed {
                    state.interaction.hovered = inside;
                    state.interaction.pressed = pressed;
                    CheckboxOutcome::Redraw
                } else {
                    CheckboxOutcome::Ignored
                }
            }
            MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight
            | MouseEventKind::Move => CheckboxOutcome::Ignored,
        }
    }
}

const fn toggle(state: &mut CheckboxState) -> CheckboxOutcome {
    state.checked = !state.checked;
    CheckboxOutcome::Toggled(state.checked)
}

impl crate::theme::ComponentTheme {
    /// Convert this semantic component theme into [`CheckboxStyles`].
    #[must_use]
    pub fn checkbox_styles(self) -> CheckboxStyles {
        CheckboxStyles::from(self)
    }
}

impl From<crate::theme::ComponentTheme> for CheckboxStyles {
    fn from(theme: crate::theme::ComponentTheme) -> Self {
        Self {
            normal: theme.base,
            focused: theme.focused,
            hovered: theme.info,
            pressed: theme.selected,
            disabled: theme.disabled,
        }
    }
}

#[cfg(test)]
mod tests {
    use bmux_keyboard::{KeyCode, KeyStroke};
    use bmux_tui::buffer::Buffer;
    use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};

    use super::{Checkbox, CheckboxOutcome, CheckboxState};

    #[test]
    fn renders_checked_and_unchecked_states() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 16, 2));
        let mut frame = Frame::new(&mut buffer);
        let checkbox = Checkbox::new("Enable");

        checkbox.render(
            Rect::new(0, 0, 16, 1),
            &CheckboxState::new(false),
            &mut frame,
        );
        checkbox.render(
            Rect::new(0, 1, 16, 1),
            &CheckboxState::new(true),
            &mut frame,
        );

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("[ ] Enable      ")
        );
        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("[x] Enable      ")
        );
    }

    #[test]
    fn focused_space_toggles_checkbox() {
        let checkbox = Checkbox::new("Enable");
        let mut state = CheckboxState::new(false);
        state.set_focused(true);

        let outcome = checkbox.handle_event(
            Rect::new(0, 0, 12, 1),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Space)),
        );

        assert_eq!(outcome, CheckboxOutcome::Toggled(true));
        assert!(state.checked());
    }

    #[test]
    fn mouse_click_inside_toggles_checkbox() {
        let checkbox = Checkbox::new("Enable");
        let mut state = CheckboxState::new(false);
        let area = Rect::new(0, 0, 12, 1);

        let down = checkbox.handle_event(
            area,
            &mut state,
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(1, 0),
            )),
        );
        let up = checkbox.handle_event(
            area,
            &mut state,
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Up(MouseButton::Left),
                Point::new(1, 0),
            )),
        );

        assert_eq!(down, CheckboxOutcome::Redraw);
        assert_eq!(up, CheckboxOutcome::Toggled(true));
        assert!(state.checked());
    }

    #[test]
    fn disabled_checkbox_ignores_events() {
        let checkbox = Checkbox::new("Enable");
        let mut state = CheckboxState::new(false);
        state.set_disabled(true);
        state.set_focused(true);

        let outcome = checkbox.handle_event(
            Rect::new(0, 0, 12, 1),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Space)),
        );

        assert_eq!(outcome, CheckboxOutcome::Ignored);
        assert!(!state.checked());
    }
}
