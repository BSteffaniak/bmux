//! Configurable button component.

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

use crate::common::{ComponentMousePolicy, InteractionState, InteractionStyles};
use crate::hit_test::HitRegion;

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

/// Canonical component-lifecycle button leaf.
///
/// Interaction state remains caller-owned through an interior-mutable `Cell`,
/// allowing the shared component protocol to update it without framework-owned
/// application state.
pub struct ButtonComponent<'a, 'state> {
    id: LayoutId,
    button: Button<'a>,
    state: &'state Cell<ButtonState>,
}

impl<'a, 'state> ButtonComponent<'a, 'state> {
    /// Create a button component with stable identity and caller-owned state.
    #[must_use]
    pub fn new(id: impl Into<LayoutId>, label: &'a str, state: &'state Cell<ButtonState>) -> Self {
        Self {
            id: id.into(),
            button: Button::new(label),
            state,
        }
    }
    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: ButtonPolicy) -> Self {
        self.button.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: ButtonStyles) -> Self {
        self.button.styles = styles;
        self
    }
}

impl Component for ButtonComponent<'_, '_> {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        self.button.label.hash(&mut layout);

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        format!("{:?}", self.button.policy).hash(&mut paint);
        format!("{:?}", self.button.styles).hash(&mut paint);
        format!("{:?}", self.state.get()).hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(self.button.width(), 1)),
        )
        .with_metadata(LayoutMetadata::new().semantic("button"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let state = self.state.get();
        let line = Line::from_spans(vec![Span::styled(
            format!("[ {} ]", self.button.label),
            self.button.style_for(state),
        )]);
        cx.write_line(LocalRect::new(0, 0, layout.size.width, 1), &line);
        cx.push_hit(
            SceneRegion::new(self.id.as_str(), Rect::new(0, 0, layout.size.width, 1))
                .role(HitRole::Action)
                .hoverable(self.button.policy.mouse.hover)
                .focusable(true)
                .enabled(!state.interaction.disabled),
        );
        cx.push_semantic(SemanticRegion::new(
            self.id.as_str(),
            Rect::new(0, 0, layout.size.width, 1),
            "button",
        ));
        cx.push_damage(LocalRect::new(0, 0, layout.size.width, 1));
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        let Some(area) = cx.find_rect(&layout.id) else {
            return EventOutcome::Ignored;
        };
        let mut state = self.state.get();
        let outcome = self.button.handle_event(area, &mut state, event);
        self.state.set(state);
        match outcome {
            ButtonOutcome::Ignored => EventOutcome::Ignored,
            ButtonOutcome::Handled => EventOutcome::Handled,
            ButtonOutcome::Pressed | ButtonOutcome::Redraw => EventOutcome::Redraw,
        }
    }
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
            Event::Key(stroke) => self.handle_key(*stroke),
            Event::Mouse(mouse) => self.handle_mouse(area, state, *mouse),
            Event::Resize(_) | Event::Paste(_) | Event::Focus(_) | Event::Tick | Event::User(_) => {
                ButtonOutcome::Ignored
            }
        }
    }

    const fn style_for(&self, state: ButtonState) -> Style {
        InteractionStyles::new(
            self.styles.normal,
            self.styles.focused,
            self.styles.hovered,
            self.styles.pressed,
            self.styles.disabled,
        )
        .resolve(state.interaction)
    }

    const fn handle_key(&self, stroke: KeyStroke) -> ButtonOutcome {
        if !stroke.modifiers.is_empty() {
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
        let contains = HitRegion::new((), area).contains(mouse.position);
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

impl crate::theme::ComponentTheme {
    /// Convert this semantic component theme into [`ButtonStyles`].
    #[must_use]
    pub fn button_styles(self) -> ButtonStyles {
        ButtonStyles::from(self)
    }
}

impl From<crate::theme::ComponentTheme> for ButtonStyles {
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
    use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};
    use bmux_tui::paint::PaintCx;

    use super::{Button, ButtonComponent, ButtonOutcome, ButtonState};

    #[test]
    fn canonical_component_separates_layout_and_paint_revisions() {
        let state = Cell::new(ButtonState::new());
        let button = ButtonComponent::new("save", "Save", &state);
        let initial = button.revision();
        state.set(ButtonState {
            interaction: crate::common::InteractionState::new().focused(true),
        });
        let focused = button.revision();

        assert_eq!(initial.layout, focused.layout);
        assert_ne!(initial.paint, focused.paint);
        assert_ne!(
            initial.layout,
            ButtonComponent::new("save", "Save changes", &state)
                .revision()
                .layout
        );
        assert_ne!(
            initial.paint,
            ButtonComponent::new("save", "Save", &state)
                .policy(super::ButtonPolicy::keyboard())
                .revision()
                .paint
        );
        assert_ne!(
            initial.paint,
            ButtonComponent::new("save", "Save", &state)
                .styles(super::ButtonStyles {
                    normal: bmux_tui::style::Style::new().fg(bmux_tui::style::Color::Red),
                    ..super::ButtonStyles::default()
                })
                .revision()
                .paint
        );
    }

    #[test]
    fn canonical_component_paints_and_routes_from_one_layout() {
        let state = Cell::new(ButtonState::new());
        let button = ButtonComponent::new("save", "Save", &state);
        let mut layout_cx = LayoutCx::new();
        let layout = button.layout(Constraints::for_width(8), &mut layout_cx);
        let measured = layout_cx.measured_nodes();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        let mut frame = Frame::new(&mut buffer);
        button.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(frame.hits().regions()[0].id.as_str(), "save");
        assert_eq!(frame.semantics().regions()[0].id, "save");
        assert_eq!(frame.semantics().regions()[0].role, "button");
        assert!(
            frame
                .damage(bmux_tui::damage::DamagePolicy::default())
                .is_full()
        );

        let mut event_cx = EventCx::new(&layout);
        let outcome = button.event(
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(1, 0),
            )),
            &layout,
            &mut event_cx,
        );

        assert!(outcome.needs_redraw());
        assert!(state.get().interaction.pressed);
        assert_eq!(layout_cx.measured_nodes(), measured);
    }

    #[test]
    fn renders_button_label() {
        let state = Cell::new(ButtonState::new());
        state.set({
            let mut focused = state.get();
            focused.set_focused(true);
            focused
        });
        let button = ButtonComponent::new("save", "Save", &state);
        let layout = button.layout(Constraints::for_width(10), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 1));
        let mut frame = Frame::new(&mut buffer);

        button.paint(&layout, &mut PaintCx::new(&mut frame));

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
