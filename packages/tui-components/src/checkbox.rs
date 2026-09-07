//! Configurable checkbox component.

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

use crate::common::{ComponentMousePolicy, InteractionState};

/// Visual styles for a checkbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

/// Canonical component-lifecycle checkbox control.
pub struct CheckboxComponent<'a, 'state> {
    id: LayoutId,
    checkbox: Checkbox<'a>,
    state: &'state Cell<CheckboxState>,
    fallback: Style,
}

impl<'a, 'state> CheckboxComponent<'a, 'state> {
    /// Create a checkbox with stable identity and caller-owned state.
    #[must_use]
    pub fn new(
        id: impl Into<LayoutId>,
        label: &'a str,
        state: &'state Cell<CheckboxState>,
    ) -> Self {
        Self {
            id: id.into(),
            checkbox: Checkbox::new(label),
            state,
            fallback: Style::new(),
        }
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: CheckboxPolicy) -> Self {
        self.checkbox.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: CheckboxStyles) -> Self {
        self.checkbox.styles = styles;
        self
    }

    /// Set the style inherited by otherwise unstyled cells across the assigned row.
    #[must_use]
    pub const fn fallback_style(mut self, fallback: Style) -> Self {
        self.fallback = fallback;
        self
    }
}

impl Component for CheckboxComponent<'_, '_> {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        self.checkbox.label.hash(&mut layout);

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        let policy = self.checkbox.policy;
        (
            policy.mouse.enabled,
            policy.mouse.hover,
            policy.mouse.click,
            policy.enter_toggles,
            policy.space_toggles,
        )
            .hash(&mut paint);
        self.checkbox.styles.hash(&mut paint);
        self.fallback.hash(&mut paint);
        self.state.get().hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let width = u16::try_from(bmux_tui::text_width::display_width(self.checkbox.label))
            .unwrap_or(u16::MAX)
            .saturating_add(4);
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(width, 1)),
        )
        .with_metadata(LayoutMetadata::new().semantic("checkbox"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        if layout.size.width == 0 || layout.size.height == 0 {
            return;
        }
        let state = self.state.get();
        let area = LocalRect::new(0, 0, layout.size.width, 1);
        cx.write_line_with_fallback_style(area, &self.checkbox.line(state), self.fallback);
        cx.push_hit(
            SceneRegion::new(self.id.as_str(), Rect::new(0, 0, layout.size.width, 1))
                .role(HitRole::Action)
                .pointer_events(self.checkbox.policy.mouse.enabled)
                .hoverable(self.checkbox.policy.mouse.enabled && self.checkbox.policy.mouse.hover)
                .focusable(true)
                .enabled(!state.interaction.disabled),
        );
        cx.push_semantic(SemanticRegion::new(
            self.id.as_str(),
            Rect::new(0, 0, layout.size.width, 1),
            "checkbox",
        ));
        cx.push_damage(area);
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        let Some(area) = cx.find_rect(&layout.id) else {
            return EventOutcome::Ignored;
        };
        let area = cx
            .visible_rect(bmux_tui::component::LogicalRect::new(
                0,
                0,
                layout.size.width,
                1,
            ))
            .intersection(area);
        let mut state = self.state.get();
        // Hidden controls still reconcile input policy, but cannot activate.
        let event = if area.is_empty() && !matches!(event, Event::Focus(_) | Event::Resize(_)) {
            &Event::Tick
        } else {
            event
        };
        let outcome = self.checkbox.handle_event(area, &mut state, event);
        self.state.set(state);
        match outcome {
            CheckboxOutcome::Ignored => EventOutcome::Ignored,
            CheckboxOutcome::Redraw | CheckboxOutcome::Toggled(_) => EventOutcome::Redraw,
        }
    }
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

    /// Handle one input event.
    pub const fn handle_event(
        &self,
        area: Rect,
        state: &mut CheckboxState,
        event: &Event,
    ) -> CheckboxOutcome {
        let mut cancelled = false;
        if state.interaction.disabled || !self.policy.mouse.enabled {
            cancelled = state.interaction.hovered || state.interaction.pressed;
            state.interaction.hovered = false;
            state.interaction.pressed = false;
        } else {
            if !self.policy.mouse.hover {
                cancelled |= state.interaction.hovered;
                state.interaction.hovered = false;
            }
            if !self.policy.mouse.click {
                cancelled |= state.interaction.pressed;
                state.interaction.pressed = false;
            }
        }
        if state.interaction.disabled {
            return if cancelled {
                CheckboxOutcome::Redraw
            } else {
                CheckboxOutcome::Ignored
            };
        }
        let outcome = match event {
            Event::Key(stroke) => self.handle_key(state, *stroke),
            Event::Mouse(mouse) => self.handle_mouse(area, state, *mouse),
            Event::Focus(bmux_tui::event::FocusEvent::Lost) | Event::Resize(_) => {
                let changed = state.interaction.hovered || state.interaction.pressed;
                state.interaction.hovered = false;
                state.interaction.pressed = false;
                if changed {
                    CheckboxOutcome::Redraw
                } else {
                    CheckboxOutcome::Ignored
                }
            }
            Event::Paste(_) | Event::Focus(_) | Event::Tick | Event::User(_) => {
                CheckboxOutcome::Ignored
            }
        };
        if cancelled && matches!(outcome, CheckboxOutcome::Ignored) {
            CheckboxOutcome::Redraw
        } else {
            outcome
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
        if !stroke.modifiers.is_empty() {
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
                state.interaction.hovered = self.policy.mouse.hover;
                CheckboxOutcome::Redraw
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let was_pressed = state.interaction.pressed;
                state.interaction.pressed = false;
                if was_pressed && inside && self.policy.mouse.click {
                    toggle(state)
                } else if was_pressed {
                    CheckboxOutcome::Redraw
                } else {
                    CheckboxOutcome::Ignored
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.policy.mouse.click => {
                let pressed = state.interaction.pressed && inside;
                let hovered = inside && self.policy.mouse.hover;
                if state.interaction.hovered != hovered || state.interaction.pressed != pressed {
                    state.interaction.hovered = hovered;
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
    use bmux_tui::geometry::{Point, Rect, Size};
    use bmux_tui::hit::HitRole;
    use bmux_tui::paint::{LocalRect, PaintCx};

    use super::{Checkbox, CheckboxComponent, CheckboxOutcome, CheckboxPolicy, CheckboxState};

    fn paint_checkbox(id: &'static str, area: Rect, state: CheckboxState, frame: &mut Frame<'_>) {
        let state = Cell::new(state);
        let checkbox = CheckboxComponent::new(id, "Enable", &state);
        let layout = checkbox.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
        PaintCx::new(frame).with_child(
            i32::from(area.x),
            i64::from(area.y),
            LocalRect::new(0, 0, area.width, area.height),
            |cx| checkbox.paint(&layout, cx),
        );
    }

    #[test]
    fn tall_layout_only_toggles_the_painted_checkbox_row() {
        let state = Cell::new(CheckboxState::new(false));
        let checkbox = CheckboxComponent::new("enable", "Enable", &state);
        let layout = checkbox.layout(Constraints::new(10, 10, 3, Some(3)), &mut LayoutCx::new());
        for (dy, clip, point) in [
            (2, Rect::new(4, 2, 10, 3), Point::new(5, 3)),
            (1, Rect::new(4, 2, 10, 2), Point::new(5, 2)),
        ] {
            bmux_tui::component::EventCx::new(&layout).with_transform(0, 0, 4, dy, clip, |cx| {
                for kind in [
                    MouseEventKind::Down(MouseButton::Left),
                    MouseEventKind::Up(MouseButton::Left),
                ] {
                    checkbox.event(&Event::Mouse(MouseEvent::new(kind, point)), &layout, cx);
                    assert_eq!(state.get(), CheckboxState::new(false));
                }
            });
        }
        bmux_tui::component::EventCx::new(&layout).with_transform(
            0,
            0,
            4,
            2,
            Rect::new(4, 2, 10, 3),
            |cx| {
                for kind in [
                    MouseEventKind::Down(MouseButton::Left),
                    MouseEventKind::Up(MouseButton::Left),
                ] {
                    checkbox.event(
                        &Event::Mouse(MouseEvent::new(kind, Point::new(5, 2))),
                        &layout,
                        cx,
                    );
                }
            },
        );
        assert!(state.get().checked());
    }

    #[test]
    fn mouse_disabled_checkbox_preserves_keyboard_toggle() {
        let mut initial = CheckboxState::new(false);
        initial.interaction.focused = true;
        let state = Cell::new(initial);
        let mut policy = CheckboxPolicy::interactive();
        policy.mouse.enabled = false;
        let checkbox = CheckboxComponent::new("enable", "Enable", &state).policy(policy);
        let layout = checkbox.layout(Constraints::for_width(10), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 1));
        let mut frame = Frame::new(&mut buffer);
        checkbox.paint(&layout, &mut PaintCx::new(&mut frame));
        let hit = &frame.hits().regions()[0];
        assert!(hit.enabled && hit.focusable);
        assert!(!hit.pointer_events && !hit.hoverable);
        let mut cx = bmux_tui::component::EventCx::new(&layout);
        assert!(
            !checkbox
                .event(
                    &Event::Mouse(MouseEvent::new(
                        MouseEventKind::Down(MouseButton::Left),
                        Point::new(1, 0)
                    )),
                    &layout,
                    &mut cx,
                )
                .is_handled()
        );
        assert_eq!(state.get(), initial);
        assert!(
            checkbox
                .event(
                    &Event::Key(KeyStroke::simple(KeyCode::Enter)),
                    &layout,
                    &mut cx,
                )
                .is_handled()
        );
        assert!(state.get().checked());
    }

    #[test]
    fn focus_loss_and_resize_cancel_click_even_in_empty_layouts() {
        let mut initial = CheckboxState::new(false);
        initial.interaction.pressed = true;
        initial.interaction.hovered = true;
        let state = Cell::new(initial);
        let component = CheckboxComponent::new("check", "Enable", &state);
        let visible = component.layout(Constraints::tight(Size::new(10, 1)), &mut LayoutCx::new());
        for (width, height) in [(10, 1), (0, 1), (10, 0), (0, 0)] {
            let layout = component.layout(
                Constraints::new(width, width, height, Some(height)),
                &mut LayoutCx::new(),
            );
            for event in [
                Event::Focus(bmux_tui::event::FocusEvent::Lost),
                Event::Resize(Size::new(10, 1)),
            ] {
                state.set(initial);
                let mut cx = bmux_tui::component::EventCx::new(&layout);
                assert_eq!(
                    component.event(&event, &layout, &mut cx),
                    bmux_tui::event::EventOutcome::Redraw
                );
                assert!(!state.get().interaction.pressed);
                assert!(!state.get().interaction.hovered);
                let release = Event::Mouse(MouseEvent::new(
                    MouseEventKind::Up(MouseButton::Left),
                    Point::new(1, 0),
                ));
                component.event(
                    &release,
                    &visible,
                    &mut bmux_tui::component::EventCx::new(&visible),
                );
                assert!(!state.get().checked);
            }
        }
    }

    #[test]
    fn clipped_checkbox_row_cannot_toggle_through_blank_layout_rows() {
        let mut initial = CheckboxState::new(false);
        initial.interaction.focused = true;
        let state = Cell::new(initial);
        let component = CheckboxComponent::new("enable", "Enable", &state);
        let layout = component.layout(Constraints::new(10, 10, 3, Some(3)), &mut LayoutCx::new());
        EventCx::new(&layout).with_transform(0, 0, 0, 0, Rect::new(0, 1, 10, 2), |cx| {
            assert!(
                cx.find_rect(&layout.id)
                    .is_some_and(|rect| !rect.is_empty())
            );
            assert!(
                !component
                    .event(&Event::Key(KeyStroke::simple(KeyCode::Enter)), &layout, cx)
                    .is_handled()
            );
        });
        assert_eq!(state.get(), initial);
    }

    #[test]
    fn hidden_checkbox_reconciles_pointer_policy_without_activating() {
        for (width, height) in [(0, 1), (10, 0), (0, 0)] {
            let mut initial = CheckboxState::new(false);
            initial.interaction.focused = true;
            initial.interaction.pressed = true;
            initial.interaction.hovered = true;
            let state = Cell::new(initial);
            let mut policy = CheckboxPolicy::default();
            policy.mouse.enabled = false;
            let checkbox = CheckboxComponent::new("enable", "Enable", &state).policy(policy);
            let layout = checkbox.layout(
                Constraints::new(width, width, height, Some(height)),
                &mut LayoutCx::new(),
            );
            assert_eq!(
                checkbox.event(
                    &Event::Key(KeyStroke::simple(KeyCode::Enter)),
                    &layout,
                    &mut EventCx::new(&layout),
                ),
                EventOutcome::Redraw
            );
            assert!(!state.get().checked);
            assert!(!state.get().interaction.pressed);
            assert!(!state.get().interaction.hovered);
            assert!(state.get().interaction.focused);
            let enabled = CheckboxComponent::new("enable", "Enable", &state);
            let visible = enabled.layout(Constraints::for_width(10), &mut LayoutCx::new());
            enabled.event(
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Up(MouseButton::Left),
                    Point::new(1, 0),
                )),
                &visible,
                &mut EventCx::new(&visible),
            );
            assert!(!state.get().checked);
        }
    }

    #[test]
    fn hover_disabled_checkbox_press_and_drag_still_toggle() {
        let area = Rect::new(0, 0, 10, 1);
        let mut policy = CheckboxPolicy::default();
        policy.mouse.hover = false;
        let checkbox = Checkbox::new("Enable").policy(policy);
        let mut state = CheckboxState::new(false);
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
        ] {
            checkbox.handle_event(
                area,
                &mut state,
                &Event::Mouse(MouseEvent::new(kind, Point::new(1, 0))),
            );
            assert!(state.interaction.pressed);
            assert!(!state.interaction.hovered);
            assert!(!state.checked);
        }
        assert_eq!(
            checkbox.handle_event(
                area,
                &mut state,
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Up(MouseButton::Left),
                    Point::new(1, 0),
                )),
            ),
            CheckboxOutcome::Toggled(true)
        );
        assert!(!state.interaction.pressed);
        assert!(!state.interaction.hovered);
    }

    #[test]
    fn disabling_pointer_input_cancels_pending_checkbox_press() {
        let area = Rect::new(0, 0, 10, 1);
        for disable in 0..3 {
            let mut state = CheckboxState::new(false);
            state.interaction.focused = true;
            let checkbox = Checkbox::new("Enable");
            checkbox.handle_event(
                area,
                &mut state,
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Down(MouseButton::Left),
                    Point::new(1, 0),
                )),
            );
            assert!(state.interaction.pressed);
            let mut policy = CheckboxPolicy::default();
            match disable {
                0 => policy.mouse.enabled = false,
                1 => policy.mouse.click = false,
                _ => state.interaction.disabled = true,
            }
            assert_eq!(
                checkbox
                    .policy(policy)
                    .handle_event(area, &mut state, &Event::Tick),
                CheckboxOutcome::Redraw
            );
            assert!(!state.interaction.pressed);
            assert!(!state.checked);
            state.interaction.disabled = false;
            assert_eq!(
                checkbox.handle_event(
                    area,
                    &mut state,
                    &Event::Mouse(MouseEvent::new(
                        MouseEventKind::Up(MouseButton::Left),
                        Point::new(1, 0),
                    )),
                ),
                CheckboxOutcome::Ignored
            );
            assert!(!state.checked);
            assert_eq!(
                checkbox.policy(policy).handle_event(
                    area,
                    &mut state,
                    &Event::Key(KeyStroke::simple(KeyCode::Enter)),
                ),
                CheckboxOutcome::Toggled(true)
            );
        }
    }

    #[test]
    fn disabling_click_before_release_cancels_pending_toggle() {
        let area = Rect::new(0, 0, 10, 1);
        let mut state = CheckboxState::new(false);
        let down = Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            Point::new(1, 0),
        ));
        Checkbox::new("Enable").handle_event(area, &mut state, &down);
        assert!(state.interaction.pressed);
        let mut policy = CheckboxPolicy::default();
        policy.mouse.click = false;
        let release = Event::Mouse(MouseEvent::new(
            MouseEventKind::Up(MouseButton::Left),
            Point::new(1, 0),
        ));
        assert_eq!(
            Checkbox::new("Enable")
                .policy(policy)
                .handle_event(area, &mut state, &release),
            CheckboxOutcome::Redraw
        );
        assert!(!state.interaction.pressed);
        assert!(!state.checked);
        assert_eq!(
            Checkbox::new("Enable").handle_event(area, &mut state, &release),
            CheckboxOutcome::Ignored
        );
        assert!(!state.checked);
    }

    #[test]
    fn empty_checkbox_does_not_toggle_or_publish_regions() {
        let mut initial = CheckboxState::new(false);
        initial.interaction.focused = true;
        let state = Cell::new(initial);
        let checkbox = CheckboxComponent::new("enable", "Enable", &state);
        for (width, height) in [(0, 1), (10, 0), (0, 0)] {
            let layout = checkbox.layout(
                Constraints::new(width, width, height, Some(height)),
                &mut LayoutCx::new(),
            );
            let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 1));
            let mut frame = Frame::new(&mut buffer);
            checkbox.paint(&layout, &mut PaintCx::new(&mut frame));
            assert!(frame.hits().regions().is_empty());
            assert!(frame.semantics().regions().is_empty());
            assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("          "));
            assert!(
                !checkbox
                    .event(
                        &Event::Key(KeyStroke::simple(KeyCode::Enter)),
                        &layout,
                        &mut bmux_tui::component::EventCx::new(&layout),
                    )
                    .is_handled()
            );
            assert_eq!(state.get(), initial);
        }
    }

    #[test]
    fn renders_checked_and_unchecked_states() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 16, 2));
        let mut frame = Frame::new(&mut buffer);

        paint_checkbox(
            "unchecked",
            Rect::new(0, 0, 16, 1),
            CheckboxState::new(false),
            &mut frame,
        );
        paint_checkbox(
            "checked",
            Rect::new(0, 1, 16, 1),
            CheckboxState::new(true),
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
    fn render_registers_exact_interaction_geometry_and_state() {
        let mut buffer = Buffer::empty(Rect::new(4, 3, 20, 4));
        let mut frame = Frame::new(&mut buffer);
        let enabled = CheckboxState::new(false);
        let mut disabled = CheckboxState::new(true);
        disabled.set_disabled(true);

        paint_checkbox(
            "settings.enable",
            Rect::new(7, 4, 11, 1),
            enabled,
            &mut frame,
        );
        paint_checkbox(
            "settings.disabled",
            Rect::new(7, 5, 13, 1),
            disabled,
            &mut frame,
        );

        let regions = frame.hits().regions();
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0].id.as_str(), "settings.enable");
        assert_eq!(regions[0].area, Rect::new(7, 4, 11, 1));
        assert_eq!(regions[0].role, HitRole::Action);
        assert!(regions[0].focusable);
        assert!(regions[0].enabled);
        assert_eq!(regions[1].id.as_str(), "settings.disabled");
        assert_eq!(regions[1].area, Rect::new(7, 5, 13, 1));
        assert!(!regions[1].enabled);
        assert!(
            frame
                .hits()
                .focus_targets(None)
                .iter()
                .any(|id| { id.as_str() == "settings.enable" })
        );
        assert!(
            !frame
                .hits()
                .focus_targets(None)
                .iter()
                .any(|id| { id.as_str() == "settings.disabled" })
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

    #[test]
    fn canonical_component_uses_one_layout_for_all_channels() {
        let state = Cell::new(CheckboxState::new(true));
        let checkbox = CheckboxComponent::new("settings.enable", "Enable", &state);
        let mut layout_cx = LayoutCx::new();
        let layout = checkbox.layout(Constraints::loose(Size::new(20, 2)), &mut layout_cx);
        assert_eq!(layout.size, bmux_tui::component::LogicalSize::new(10, 1));
        assert_eq!(layout.metadata.semantics, ["checkbox"]);

        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 2));
        let mut frame = Frame::new(&mut buffer);
        checkbox.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(frame.hits().regions()[0].area, Rect::new(0, 0, 10, 1));
        assert_eq!(frame.semantics().regions()[0].area, Rect::new(0, 0, 10, 1));
        assert_eq!(
            frame
                .damage(bmux_tui::damage::DamagePolicy::default())
                .retained_regions(),
            &[Rect::new(0, 0, 10, 1)]
        );
    }

    #[test]
    fn canonical_revision_tracks_each_policy_state_and_style_field() {
        let state = Cell::new(CheckboxState::new(false));
        let baseline = CheckboxComponent::new("enable", "Enable", &state).revision();
        for field in 0..5 {
            let mut policy = CheckboxPolicy::default();
            match field {
                0 => policy.mouse.enabled = !policy.mouse.enabled,
                1 => policy.mouse.hover = !policy.mouse.hover,
                2 => policy.mouse.click = !policy.mouse.click,
                3 => policy.enter_toggles = !policy.enter_toggles,
                _ => policy.space_toggles = !policy.space_toggles,
            }
            let changed = CheckboxComponent::new("enable", "Enable", &state)
                .policy(policy)
                .revision();
            assert_eq!(baseline.layout, changed.layout);
            assert_ne!(baseline.paint, changed.paint, "policy field {field}");
        }
        for field in 0..5 {
            let mut changed_state = CheckboxState::new(false);
            match field {
                0 => changed_state.checked = true,
                1 => changed_state.interaction.focused = true,
                2 => changed_state.interaction.hovered = true,
                3 => changed_state.interaction.pressed = true,
                _ => changed_state.interaction.disabled = true,
            }
            state.set(changed_state);
            let changed = CheckboxComponent::new("enable", "Enable", &state).revision();
            assert_eq!(baseline.layout, changed.layout);
            assert_ne!(baseline.paint, changed.paint, "state field {field}");
        }
        state.set(CheckboxState::new(false));
        for field in 0..6 {
            let mut styles = super::CheckboxStyles::default();
            let accent = bmux_tui::style::Style::new().fg(bmux_tui::style::Color::Red);
            match field {
                0 => styles.normal = accent,
                1 => styles.focused = accent,
                2 => styles.hovered = accent,
                3 => styles.pressed = accent,
                4 => styles.disabled = accent,
                _ => {}
            }
            let mut checkbox = CheckboxComponent::new("enable", "Enable", &state).styles(styles);
            if field == 5 {
                checkbox = checkbox.fallback_style(accent);
            }
            let changed = checkbox.revision();
            assert_eq!(baseline.layout, changed.layout);
            assert_ne!(baseline.paint, changed.paint, "style field {field}");
        }
    }

    #[test]
    fn canonical_component_revision_separates_geometry_and_paint() {
        let state = Cell::new(CheckboxState::new(false));
        let initial = CheckboxComponent::new("enable", "Enable", &state).revision();
        state.set(CheckboxState::new(true));
        let checked = CheckboxComponent::new("enable", "Enable", &state).revision();
        assert_eq!(initial.layout, checked.layout);
        assert_ne!(initial.paint, checked.paint);

        let keyboard = CheckboxComponent::new("enable", "Enable", &state)
            .policy(CheckboxPolicy {
                mouse: crate::common::ComponentMousePolicy::disabled(),
                enter_toggles: true,
                space_toggles: true,
            })
            .revision();
        assert_eq!(checked.layout, keyboard.layout);
        assert_ne!(checked.paint, keyboard.paint);
        assert_ne!(
            initial.layout,
            CheckboxComponent::new("enable", "Enable feature", &state)
                .revision()
                .layout
        );
    }
}
