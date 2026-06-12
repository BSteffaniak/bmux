//! Configurable button component.

use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::Modifier;

use crate::common::{ComponentMousePolicy, InteractionState};

/// Visual styles for a button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ButtonStyles {
    /// Style used when the button is enabled and inactive.
    pub normal: Style,
    /// Style used when the button has keyboard focus.
    pub focused: Style,
    /// Style used when the pointer is hovering the button.
    pub hovered: Style,
    /// Style used while the primary pointer/button is pressed.
    pub pressed: Style,
    /// Style used when the button is disabled.
    pub disabled: Style,
}

impl Default for ButtonStyles {
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

/// Configurable button behavior policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ButtonPolicy {
    /// Mouse behavior.
    pub mouse: ComponentMousePolicy,
    /// Whether Enter activates the button when focused.
    pub enter_activates: bool,
    /// Whether Space activates the button when focused.
    pub space_activates: bool,
}

impl ButtonPolicy {
    /// Keyboard-only button behavior.
    #[must_use]
    pub const fn keyboard() -> Self {
        Self {
            mouse: ComponentMousePolicy::disabled(),
            enter_activates: true,
            space_activates: true,
        }
    }

    /// Common keyboard and mouse button behavior.
    #[must_use]
    pub const fn interactive() -> Self {
        Self {
            mouse: ComponentMousePolicy::button(),
            enter_activates: true,
            space_activates: true,
        }
    }
}

impl Default for ButtonPolicy {
    fn default() -> Self {
        Self::interactive()
    }
}

/// Runtime button state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ButtonState {
    /// Common focus/hover/press/disabled state.
    pub interaction: InteractionState,
}

impl ButtonState {
    /// Create enabled button state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            interaction: InteractionState::new(),
        }
    }

    /// Return whether the button has keyboard focus.
    #[must_use]
    pub const fn focused(self) -> bool {
        self.interaction.focused
    }

    /// Set keyboard focus.
    pub const fn set_focused(&mut self, focused: bool) {
        self.interaction.focused = focused;
    }

    /// Set disabled state.
    pub const fn set_disabled(&mut self, disabled: bool) {
        self.interaction.disabled = disabled;
        if disabled {
            self.interaction.hovered = false;
            self.interaction.pressed = false;
        }
    }
}

/// Outcome from handling a button event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonOutcome {
    /// Event was not handled.
    Ignored,
    /// Event was handled without requiring redraw.
    Handled,
    /// Event was handled and requires redraw.
    Redraw,
    /// Button was activated.
    Pressed,
}

impl ButtonOutcome {
    /// Return true when the event was handled.
    #[must_use]
    pub const fn is_handled(self) -> bool {
        matches!(self, Self::Handled | Self::Redraw | Self::Pressed)
    }

    /// Return true when rendering should be refreshed.
    #[must_use]
    pub const fn needs_redraw(self) -> bool {
        matches!(self, Self::Redraw | Self::Pressed)
    }
}

/// Button renderer and event handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Button<'a> {
    label: &'a str,
    policy: ButtonPolicy,
    styles: ButtonStyles,
}

impl<'a> Button<'a> {
    /// Create a button with a visible label.
    #[must_use]
    pub const fn new(label: &'a str) -> Self {
        Self {
            label,
            policy: ButtonPolicy::interactive(),
            styles: ButtonStyles {
                normal: Style::new(),
                focused: Style::new().add_modifier(Modifier::REVERSED),
                hovered: Style::new().add_modifier(Modifier::UNDERLINE),
                pressed: Style::new().add_modifier(Modifier::BOLD),
                disabled: Style::new().add_modifier(Modifier::DIM),
            },
        }
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: ButtonPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: ButtonStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Return rendered button width.
    #[must_use]
    pub fn width(&self) -> u16 {
        u16::try_from(bmux_tui::text_width::display_width(self.label))
            .unwrap_or(u16::MAX)
            .saturating_add(4)
    }

    /// Render the button.
    pub fn render(&self, area: Rect, state: &ButtonState, frame: &mut Frame<'_>) {
        let line = Line::from_spans(vec![Span::styled(
            format!("[ {} ]", self.label),
            self.style_for(*state),
        )]);
        frame.write_line(area, &line);
    }

    /// Render the button with a fallback style filling its area.
    pub fn render_with_fallback_style(
        &self,
        area: Rect,
        state: &ButtonState,
        frame: &mut Frame<'_>,
        fallback: Style,
    ) {
        let line = Line::from_spans(vec![Span::styled(
            format!("[ {} ]", self.label),
            self.style_for(*state),
        )]);
        frame.write_line_with_fallback_style(area, &line, fallback);
    }

    /// Handle one input event.
    pub const fn handle_event(
        &self,
        area: Rect,
        state: &mut ButtonState,
        event: &Event,
    ) -> ButtonOutcome {
        if state.interaction.disabled {
            return ButtonOutcome::Ignored;
        }
        match event {
            Event::Key(stroke) => self.handle_key(*state, *stroke),
            Event::Mouse(mouse) => self.handle_mouse(area, state, *mouse),
            Event::Resize(_) | Event::Paste(_) | Event::Focus(_) | Event::Tick | Event::User(_) => {
                ButtonOutcome::Ignored
            }
        }
    }

    const fn style_for(&self, state: ButtonState) -> Style {
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

    const fn handle_key(&self, state: ButtonState, stroke: KeyStroke) -> ButtonOutcome {
        if !state.interaction.focused || !stroke.modifiers.is_empty() {
            return ButtonOutcome::Ignored;
        }
        match stroke.key {
            KeyCode::Enter if self.policy.enter_activates => ButtonOutcome::Pressed,
            KeyCode::Space | KeyCode::Char(' ') if self.policy.space_activates => {
                ButtonOutcome::Pressed
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
            | KeyCode::F(_) => ButtonOutcome::Ignored,
        }
    }

    const fn handle_mouse(
        &self,
        area: Rect,
        state: &mut ButtonState,
        mouse: MouseEvent,
    ) -> ButtonOutcome {
        if !self.policy.mouse.enabled {
            return ButtonOutcome::Ignored;
        }
        let contains = area.contains(mouse.position);
        match mouse.kind {
            MouseEventKind::Move if self.policy.mouse.hover => set_hovered(state, contains),
            MouseEventKind::Down(MouseButton::Left) if self.policy.mouse.click && contains => {
                state.interaction.pressed = true;
                state.interaction.focused = true;
                ButtonOutcome::Redraw
            }
            MouseEventKind::Up(MouseButton::Left) if state.interaction.pressed => {
                state.interaction.pressed = false;
                state.interaction.hovered = contains;
                if contains {
                    ButtonOutcome::Pressed
                } else {
                    ButtonOutcome::Redraw
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if state.interaction.pressed => {
                set_hovered(state, contains)
            }
            MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::Move
            | MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => ButtonOutcome::Ignored,
        }
    }
}

const fn set_hovered(state: &mut ButtonState, hovered: bool) -> ButtonOutcome {
    if state.interaction.hovered == hovered {
        ButtonOutcome::Handled
    } else {
        state.interaction.hovered = hovered;
        ButtonOutcome::Redraw
    }
}

#[cfg(test)]
mod tests {
    use bmux_keyboard::{KeyCode, KeyStroke};
    use bmux_tui::buffer::Buffer;
    use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};

    use super::{Button, ButtonOutcome, ButtonState};

    #[test]
    fn renders_button_label() {
        let button = Button::new("Save");
        let mut state = ButtonState::new();
        state.set_focused(true);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 1));
        let mut frame = Frame::new(&mut buffer);

        button.render(Rect::new(0, 0, 10, 1), &state, &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("[ Save ]  "));
    }

    #[test]
    fn focused_enter_presses_button() {
        let button = Button::new("Save");
        let mut state = ButtonState::new();
        state.set_focused(true);

        let outcome = button.handle_event(
            Rect::new(0, 0, 10, 1),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Enter)),
        );

        assert_eq!(outcome, ButtonOutcome::Pressed);
    }

    #[test]
    fn mouse_click_inside_presses_button() {
        let button = Button::new("Save");
        let mut state = ButtonState::new();
        let area = Rect::new(0, 0, 10, 1);

        let down = button.handle_event(
            area,
            &mut state,
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(1, 0),
            )),
        );
        let up = button.handle_event(
            area,
            &mut state,
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Up(MouseButton::Left),
                Point::new(1, 0),
            )),
        );

        assert_eq!(down, ButtonOutcome::Redraw);
        assert_eq!(up, ButtonOutcome::Pressed);
    }

    #[test]
    fn disabled_button_ignores_events() {
        let button = Button::new("Save");
        let mut state = ButtonState::new();
        state.set_disabled(true);
        state.set_focused(true);

        let outcome = button.handle_event(
            Rect::new(0, 0, 10, 1),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Enter)),
        );

        assert_eq!(outcome, ButtonOutcome::Ignored);
    }
}
