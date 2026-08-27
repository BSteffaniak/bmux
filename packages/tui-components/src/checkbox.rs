//! Configurable checkbox component.

use std::cell::Cell;
use std::hash::{Hash, Hasher};

use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::component::{
    Component, ComponentRevision, Constraints, EventCx, LayoutCx, LayoutId, LayoutMetadata,
    LayoutNode, LogicalSize,
};
use bmux_tui::event::{Event, EventOutcome, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::hit::{HitId, HitRegion as SceneRegion, HitRole};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::semantic::SemanticRegion;
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

/// Canonical component-lifecycle checkbox control.
pub struct CheckboxComponent<'a, 'state> {
    id: LayoutId,
    checkbox: Checkbox<'a>,
    state: &'state Cell<CheckboxState>,
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
}

impl Component for CheckboxComponent<'_, '_> {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        self.checkbox.label.hash(&mut layout);

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        format!("{:?}", self.checkbox.policy).hash(&mut paint);
        format!("{:?}", self.checkbox.styles).hash(&mut paint);
        format!("{:?}", self.state.get()).hash(&mut paint);
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
        cx.write_line(area, &self.checkbox.line(state));
        cx.push_hit(
            SceneRegion::new(self.id.as_str(), Rect::new(0, 0, layout.size.width, 1))
                .role(HitRole::Action)
                .hoverable(self.checkbox.policy.mouse.hover)
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
        let mut state = self.state.get();
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

    /// Render the checkbox and register its default interaction semantics.
    ///
    /// Use [`Self::render_with_id`] when focus must survive responsive reflow
    /// or callers route events by semantic identity.
    pub fn render(&self, area: Rect, state: &CheckboxState, frame: &mut Frame<'_>) {
        let id = frame.next_interaction_id("checkbox");
        self.render_with_id(id, area, state, frame);
    }

    /// Render the checkbox with a stable interaction identifier.
    pub fn render_with_id(
        &self,
        id: impl Into<HitId>,
        area: Rect,
        state: &CheckboxState,
        frame: &mut Frame<'_>,
    ) {
        self.register_interaction(id, area, *state, frame);
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
        let id = frame.next_interaction_id("checkbox");
        self.render_with_id_and_fallback_style(id, area, state, frame, fallback);
    }

    /// Render with a stable interaction identifier and fallback style.
    pub fn render_with_id_and_fallback_style(
        &self,
        id: impl Into<HitId>,
        area: Rect,
        state: &CheckboxState,
        frame: &mut Frame<'_>,
        fallback: Style,
    ) {
        self.register_interaction(id, area, *state, frame);
        frame.write_line_with_fallback_style(area, &self.line(*state), fallback);
    }

    fn register_interaction(
        &self,
        id: impl Into<HitId>,
        area: Rect,
        state: CheckboxState,
        frame: &mut Frame<'_>,
    ) {
        frame.push_hit(
            SceneRegion::new(id, area)
                .role(HitRole::Action)
                .hoverable(self.policy.mouse.hover)
                .focusable(true)
                .enabled(!state.interaction.disabled),
        );
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
    use bmux_tui::component::{Component, Constraints, LayoutCx};
    use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect, Size};
    use bmux_tui::hit::HitRole;
    use bmux_tui::paint::PaintCx;

    use super::{Checkbox, CheckboxComponent, CheckboxOutcome, CheckboxPolicy, CheckboxState};

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
    fn render_registers_exact_interaction_geometry_and_state() {
        let mut buffer = Buffer::empty(Rect::new(4, 3, 20, 4));
        let mut frame = Frame::new(&mut buffer);
        let checkbox = Checkbox::new("Enable");
        let enabled = CheckboxState::new(false);
        let mut disabled = CheckboxState::new(true);
        disabled.set_disabled(true);

        checkbox.render_with_id(
            "settings.enable",
            Rect::new(7, 4, 11, 1),
            &enabled,
            &mut frame,
        );
        checkbox.render_with_id_and_fallback_style(
            "settings.disabled",
            Rect::new(7, 5, 13, 1),
            &disabled,
            &mut frame,
            bmux_tui::style::Style::new(),
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
