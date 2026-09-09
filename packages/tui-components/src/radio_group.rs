//! Configurable radio-group component.

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

/// One selectable radio-group option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadioOption {
    /// Stable option id chosen by the caller.
    pub id: String,
    /// Visible option label.
    pub label: String,
    /// Whether this option is disabled independently from the whole group.
    pub disabled: bool,
}

impl RadioOption {
    /// Create an enabled radio option.
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

/// Visual styles for a radio group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RadioGroupStyles {
    /// Style used for enabled inactive options.
    pub normal: Style,
    /// Style used for the focused option.
    pub focused: Style,
    /// Style used for the hovered option.
    pub hovered: Style,
    /// Style used while an option is pressed.
    pub pressed: Style,
    /// Style used for disabled options or groups.
    pub disabled: Style,
}

impl Default for RadioGroupStyles {
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

/// Keyboard behavior for a radio group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RadioGroupKeyboardPolicy {
    flags: u8,
}

impl RadioGroupKeyboardPolicy {
    const ARROW_NAVIGATION: u8 = 1 << 0;
    const WRAP_NAVIGATION: u8 = 1 << 1;
    const ENTER_SELECTS: u8 = 1 << 2;
    const SPACE_SELECTS: u8 = 1 << 3;

    /// Common interactive radio-group keyboard behavior.
    #[must_use]
    pub const fn interactive() -> Self {
        Self {
            flags: Self::ARROW_NAVIGATION
                | Self::WRAP_NAVIGATION
                | Self::ENTER_SELECTS
                | Self::SPACE_SELECTS,
        }
    }

    /// Return true when arrow keys move focus.
    #[must_use]
    pub const fn arrow_navigation(self) -> bool {
        self.flags & Self::ARROW_NAVIGATION != 0
    }

    /// Return true when focus wraps at group ends.
    #[must_use]
    pub const fn wrap_navigation(self) -> bool {
        self.flags & Self::WRAP_NAVIGATION != 0
    }

    /// Return true when Enter selects the focused option.
    #[must_use]
    pub const fn enter_selects(self) -> bool {
        self.flags & Self::ENTER_SELECTS != 0
    }

    /// Return true when Space selects the focused option.
    #[must_use]
    pub const fn space_selects(self) -> bool {
        self.flags & Self::SPACE_SELECTS != 0
    }
}

impl Default for RadioGroupKeyboardPolicy {
    fn default() -> Self {
        Self::interactive()
    }
}

/// Configurable radio-group behavior policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RadioGroupPolicy {
    /// Keyboard behavior.
    pub keyboard: RadioGroupKeyboardPolicy,
    /// Mouse behavior.
    pub mouse: ComponentMousePolicy,
}

impl RadioGroupPolicy {
    /// Common interactive radio-group behavior.
    #[must_use]
    pub const fn interactive() -> Self {
        Self {
            keyboard: RadioGroupKeyboardPolicy::interactive(),
            mouse: ComponentMousePolicy::button(),
        }
    }
}

impl Default for RadioGroupPolicy {
    fn default() -> Self {
        Self::interactive()
    }
}

/// Runtime radio-group state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RadioGroupState {
    /// Common group interaction flags.
    pub interaction: InteractionState,
    selected: Option<usize>,
    focused: Option<usize>,
    hovered: Option<usize>,
    pressed: Option<usize>,
}

impl RadioGroupState {
    /// Create radio-group state.
    #[must_use]
    pub const fn new(selected: Option<usize>) -> Self {
        Self {
            interaction: InteractionState::new(),
            selected,
            focused: selected,
            hovered: None,
            pressed: None,
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
    }

    /// Return focused option index.
    #[must_use]
    pub const fn focused(self) -> Option<usize> {
        self.focused
    }

    /// Set focused option index.
    pub const fn set_focused(&mut self, focused: Option<usize>) {
        self.focused = focused;
        self.interaction.focused = focused.is_some();
    }

    /// Set disabled state for the whole group.
    pub const fn set_disabled(&mut self, disabled: bool) {
        self.interaction.disabled = disabled;
        if disabled {
            self.hovered = None;
            self.pressed = None;
        }
    }
}

/// Outcome from radio-group input handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RadioGroupOutcome {
    /// Event was not handled.
    Ignored,
    /// Visual state changed without changing selected value.
    Redraw,
    /// Focus moved to the contained option index.
    Focused(usize),
    /// Selection changed to the contained option index.
    Selected(usize),
}

/// Canonical component-lifecycle radio group.
pub struct RadioGroupComponent<'a, 'state> {
    id: LayoutId,
    group: RadioGroup<'a>,
    state: &'state Cell<RadioGroupState>,
    fallback: Style,
}

impl<'a, 'state> RadioGroupComponent<'a, 'state> {
    /// Create a radio group with stable identity and caller-owned state.
    #[must_use]
    pub fn new(
        id: impl Into<LayoutId>,
        options: &'a [RadioOption],
        state: &'state Cell<RadioGroupState>,
    ) -> Self {
        Self {
            id: id.into(),
            group: RadioGroup::new(options),
            state,
            fallback: Style::new(),
        }
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: RadioGroupPolicy) -> Self {
        self.group.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: RadioGroupStyles) -> Self {
        self.group.styles = styles;
        self
    }

    /// Set the style inherited by otherwise unstyled cells in each option row.
    #[must_use]
    pub const fn fallback_style(mut self, fallback: Style) -> Self {
        self.fallback = fallback;
        self
    }
}

impl RadioGroupComponent<'_, '_> {
    fn mouse_hit(
        &self,
        layout: &LayoutNode,
        cx: &EventCx<'_>,
        area: Rect,
        position: bmux_tui::geometry::Point,
    ) -> Option<usize> {
        if !area.contains(position) {
            return None;
        }
        let row_rect = |index| {
            cx.visible_rect(bmux_tui::component::LogicalRect::new(
                0,
                index,
                layout.size.width,
                1,
            ))
        };
        let count = self
            .group
            .options
            .len()
            .min(layout.size.height.try_into().unwrap_or(usize::MAX));
        let mut low = 0;
        let mut high = count;
        // Projected row bottoms are monotonic, including rows clipped above
        // the viewport. Find the first row extending below the pointer.
        while low < high {
            let middle = low + (high - low) / 2;
            if row_rect(middle.try_into().unwrap_or(u64::MAX)).bottom() <= position.y {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        (low < count
            && self.group.is_enabled_option(low)
            && row_rect(low.try_into().unwrap_or(u64::MAX)).contains(position))
        .then_some(low)
    }
}

impl Component for RadioGroupComponent<'_, '_> {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        for option in self.group.options {
            option.id.hash(&mut layout);
            option.label.hash(&mut layout);
        }

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        format!("{:?}", self.group.policy).hash(&mut paint);
        format!("{:?}", self.group.styles).hash(&mut paint);
        format!("{:?}", self.fallback).hash(&mut paint);
        for option in self.group.options {
            option.disabled.hash(&mut paint);
        }
        format!("{:?}", self.state.get()).hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let width = self
            .group
            .options
            .iter()
            .map(|option| {
                u16::try_from(bmux_tui::text_width::display_width(&option.label))
                    .unwrap_or(u16::MAX)
                    .saturating_add(4)
            })
            .max()
            .unwrap_or_default();
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(
                width.into(),
                self.group.options.len().try_into().unwrap_or(u64::MAX),
            )),
        )
        .with_metadata(LayoutMetadata::new().semantic("radio-group"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        if layout.size.width == 0 || layout.size.height == 0 {
            return;
        }
        let state = self.state.get();
        let viewport = cx.area();
        let start = usize::try_from(viewport.y.max(0)).unwrap_or(usize::MAX);
        let end = usize::try_from(viewport.y.saturating_add(i64::from(viewport.height)).max(0))
            .unwrap_or(usize::MAX)
            .min(layout.size.height.try_into().unwrap_or(usize::MAX))
            .min(self.group.options.len());
        for index in start..end {
            let option = &self.group.options[index];
            let row = i64::try_from(index).unwrap_or(i64::MAX);
            cx.with_child(
                0,
                row,
                LocalRect::new(0, 0, layout.size.width.try_into().unwrap_or(u16::MAX), 1),
                |cx| {
                    cx.write_line_with_fallback_style(
                        LocalRect::new(0, 0, layout.size.width.try_into().unwrap_or(u16::MAX), 1),
                        &self.group.line(index, option, state),
                        self.fallback,
                    );
                    cx.push_hit(
                        SceneRegion::new(
                            format!("{}:{}", self.id.as_str(), option.id),
                            Rect::new(0, 0, layout.size.width.try_into().unwrap_or(u16::MAX), 1),
                        )
                        .role(HitRole::Action)
                        .pointer_events(self.group.policy.mouse.enabled)
                        .hoverable(self.group.policy.mouse.enabled && self.group.policy.mouse.hover)
                        .focusable(true)
                        .enabled(!state.interaction.disabled && !option.disabled),
                    );
                },
            );
        }
        if start >= end {
            return;
        }
        let height = u16::try_from(end - start).unwrap_or(u16::MAX);
        cx.with_child(
            0,
            i64::try_from(start).unwrap_or(i64::MAX),
            LocalRect::new(
                0,
                0,
                layout.size.width.try_into().unwrap_or(u16::MAX),
                height,
            ),
            |cx| {
                cx.push_semantic(SemanticRegion::new(
                    self.id.as_str(),
                    Rect::new(
                        0,
                        0,
                        layout.size.width.try_into().unwrap_or(u16::MAX),
                        height,
                    ),
                    "radio-group",
                ));
                cx.push_damage(LocalRect::new(
                    0,
                    0,
                    layout.size.width.try_into().unwrap_or(u16::MAX),
                    height,
                ));
            },
        );
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
                self.group
                    .options
                    .len()
                    .min(layout.size.height.try_into().unwrap_or(usize::MAX))
                    .try_into()
                    .unwrap_or(u64::MAX),
            ))
            .intersection(area);
        // Reconcile caller-owned state even while hidden, without activating.
        let event = if area.is_empty() && !matches!(event, Event::Focus(_) | Event::Resize(_)) {
            &Event::Tick
        } else {
            event
        };
        let initial = self.state.get();
        let mut state = initial;
        let outcome = if let Event::Mouse(mouse) = event {
            self.group.normalize_state(&mut state);
            if state.interaction.disabled {
                RadioGroupOutcome::Ignored
            } else {
                let hit = self.mouse_hit(layout, cx, area, mouse.position);
                self.group.handle_mouse_hit(&mut state, *mouse, hit)
            }
        } else {
            self.group.handle_event(area, &mut state, event)
        };
        self.state.set(state);
        match RadioGroup::normalized_outcome(initial, state, outcome) {
            RadioGroupOutcome::Ignored => EventOutcome::Ignored,
            RadioGroupOutcome::Redraw
            | RadioGroupOutcome::Focused(_)
            | RadioGroupOutcome::Selected(_) => EventOutcome::Redraw,
        }
    }
}

/// Configurable radio-group control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RadioGroup<'a> {
    options: &'a [RadioOption],
    policy: RadioGroupPolicy,
    styles: RadioGroupStyles,
}

impl<'a> RadioGroup<'a> {
    /// Create a radio group over caller-owned options.
    #[must_use]
    pub fn new(options: &'a [RadioOption]) -> Self {
        Self {
            options,
            policy: RadioGroupPolicy::default(),
            styles: RadioGroupStyles::default(),
        }
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: RadioGroupPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: RadioGroupStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Return required render size.
    #[must_use]
    pub fn size(&self) -> (u16, u16) {
        let width = self
            .options
            .iter()
            .map(|option| bmux_tui::text_width::display_width(&option.label))
            .max()
            .unwrap_or(0);
        (
            u16::try_from(width).unwrap_or(u16::MAX).saturating_add(4),
            u16::try_from(self.options.len()).unwrap_or(u16::MAX),
        )
    }

    /// Handle one input event.
    pub fn handle_event(
        &self,
        area: Rect,
        state: &mut RadioGroupState,
        event: &Event,
    ) -> RadioGroupOutcome {
        let initial = *state;
        self.normalize_state(state);
        if state.interaction.disabled {
            return Self::normalized_outcome(initial, *state, RadioGroupOutcome::Ignored);
        }
        let outcome = match event {
            Event::Key(stroke) => self.handle_key(state, *stroke),
            Event::Mouse(mouse) => self.handle_mouse(area, state, *mouse),
            Event::Focus(bmux_tui::event::FocusEvent::Lost) | Event::Resize(_) => {
                let changed = state.hovered.take().is_some() | state.pressed.take().is_some();
                if changed {
                    RadioGroupOutcome::Redraw
                } else {
                    RadioGroupOutcome::Ignored
                }
            }
            Event::Paste(_) | Event::Focus(_) | Event::Tick | Event::User(_) => {
                RadioGroupOutcome::Ignored
            }
        };
        Self::normalized_outcome(initial, *state, outcome)
    }

    fn normalized_outcome(
        initial: RadioGroupState,
        state: RadioGroupState,
        outcome: RadioGroupOutcome,
    ) -> RadioGroupOutcome {
        if outcome == RadioGroupOutcome::Ignored && initial != state {
            RadioGroupOutcome::Redraw
        } else {
            outcome
        }
    }

    fn line(&self, index: usize, option: &RadioOption, state: RadioGroupState) -> Line {
        let mark = if state.selected == Some(index) {
            '*'
        } else {
            ' '
        };
        Line::from_spans(vec![Span::styled(
            format!("({mark}) {}", option.label),
            self.style_for(index, option, state),
        )])
    }

    fn style_for(&self, index: usize, option: &RadioOption, state: RadioGroupState) -> Style {
        if state.interaction.disabled || option.disabled {
            self.styles.disabled
        } else if state.pressed == Some(index) {
            self.styles.pressed
        } else if state.focused == Some(index) {
            self.styles.focused
        } else if state.hovered == Some(index) {
            self.styles.hovered
        } else {
            self.styles.normal
        }
    }

    fn handle_key(&self, state: &mut RadioGroupState, stroke: KeyStroke) -> RadioGroupOutcome {
        if !stroke.modifiers.is_empty() || self.options.is_empty() {
            return RadioGroupOutcome::Ignored;
        }
        match stroke.key {
            KeyCode::Up | KeyCode::Left if self.policy.keyboard.arrow_navigation() => {
                self.move_focus(state, Direction::Previous)
            }
            KeyCode::Down | KeyCode::Right if self.policy.keyboard.arrow_navigation() => {
                self.move_focus(state, Direction::Next)
            }
            KeyCode::Enter if self.policy.keyboard.enter_selects() => self.select_focused(state),
            KeyCode::Space | KeyCode::Char(' ') if self.policy.keyboard.space_selects() => {
                self.select_focused(state)
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
            | KeyCode::F(_) => RadioGroupOutcome::Ignored,
        }
    }

    fn handle_mouse(
        &self,
        area: Rect,
        state: &mut RadioGroupState,
        mouse: MouseEvent,
    ) -> RadioGroupOutcome {
        let hit = self.hit_index(area, mouse);
        self.handle_mouse_hit(state, mouse, hit)
    }

    fn handle_mouse_hit(
        &self,
        state: &mut RadioGroupState,
        mouse: MouseEvent,
        hit: Option<usize>,
    ) -> RadioGroupOutcome {
        if !self.policy.mouse.enabled {
            return RadioGroupOutcome::Ignored;
        }
        match mouse.kind {
            MouseEventKind::Move if self.policy.mouse.hover => Self::hover(state, hit),
            MouseEventKind::Down(MouseButton::Left) if self.policy.mouse.click => {
                Self::press(state, hit, self.policy.mouse.hover)
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.release(state, hit.filter(|_| self.policy.mouse.click))
            }
            MouseEventKind::Drag(MouseButton::Left) if self.policy.mouse.click => {
                Self::drag(state, hit, self.policy.mouse.hover)
            }
            MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight
            | MouseEventKind::Move => RadioGroupOutcome::Ignored,
        }
    }

    fn hover(state: &mut RadioGroupState, hit: Option<usize>) -> RadioGroupOutcome {
        if state.hovered == hit {
            RadioGroupOutcome::Ignored
        } else {
            state.hovered = hit;
            RadioGroupOutcome::Redraw
        }
    }

    const fn press(
        state: &mut RadioGroupState,
        hit: Option<usize>,
        hover: bool,
    ) -> RadioGroupOutcome {
        let Some(index) = hit else {
            return RadioGroupOutcome::Ignored;
        };
        state.pressed = Some(index);
        state.hovered = if hover { Some(index) } else { None };
        state.set_focused(Some(index));
        RadioGroupOutcome::Redraw
    }

    fn release(&self, state: &mut RadioGroupState, hit: Option<usize>) -> RadioGroupOutcome {
        let was_pressed = state.pressed;
        state.pressed = None;
        if was_pressed.is_some() && was_pressed == hit {
            return self.select_index(state, hit.expect("hit is some when equal to pressed"));
        }
        if was_pressed.is_some() {
            RadioGroupOutcome::Redraw
        } else {
            RadioGroupOutcome::Ignored
        }
    }

    fn drag(state: &mut RadioGroupState, hit: Option<usize>, hover: bool) -> RadioGroupOutcome {
        let pressed = if state.pressed.is_some() { hit } else { None };
        let hovered = hit.filter(|_| hover);
        if state.hovered == hovered && state.pressed == pressed {
            RadioGroupOutcome::Ignored
        } else {
            state.hovered = hovered;
            state.pressed = pressed;
            RadioGroupOutcome::Redraw
        }
    }

    fn select_focused(&self, state: &mut RadioGroupState) -> RadioGroupOutcome {
        let Some(index) = state.focused else {
            return RadioGroupOutcome::Ignored;
        };
        self.select_index(state, index)
    }

    fn select_index(&self, state: &mut RadioGroupState, index: usize) -> RadioGroupOutcome {
        if !self.is_enabled_option(index) || state.selected == Some(index) {
            return RadioGroupOutcome::Ignored;
        }
        state.selected = Some(index);
        RadioGroupOutcome::Selected(index)
    }

    fn move_focus(&self, state: &mut RadioGroupState, direction: Direction) -> RadioGroupOutcome {
        let Some(index) = self.next_enabled_index(state.focused, direction) else {
            return RadioGroupOutcome::Ignored;
        };
        if state.focused == Some(index) {
            RadioGroupOutcome::Ignored
        } else {
            state.set_focused(Some(index));
            RadioGroupOutcome::Focused(index)
        }
    }

    fn next_enabled_index(&self, current: Option<usize>, direction: Direction) -> Option<usize> {
        if self.options.is_empty() {
            return None;
        }
        let start = current.unwrap_or_else(|| match direction {
            Direction::Next => 0,
            Direction::Previous => self.options.len().saturating_sub(1),
        });
        for step in 1..=self.options.len() {
            let candidate = match direction {
                Direction::Next => start.saturating_add(step),
                Direction::Previous => start.wrapping_sub(step),
            };
            let index = if self.policy.keyboard.wrap_navigation() {
                candidate % self.options.len()
            } else if candidate < self.options.len() {
                candidate
            } else {
                return None;
            };
            if self.is_enabled_option(index) {
                return Some(index);
            }
        }
        None
    }

    fn hit_index(&self, area: Rect, mouse: MouseEvent) -> Option<usize> {
        if !area.contains(mouse.position) {
            return None;
        }
        let index = usize::from(mouse.position.y.saturating_sub(area.y));
        if index < self.options.len() && self.is_enabled_option(index) {
            Some(index)
        } else {
            None
        }
    }

    fn normalize_state(&self, state: &mut RadioGroupState) {
        if state.interaction.disabled || !self.policy.mouse.enabled || !self.policy.mouse.click {
            state.pressed = None;
        }
        if state.interaction.disabled || !self.policy.mouse.enabled || !self.policy.mouse.hover {
            state.hovered = None;
        }
        if state
            .selected
            .is_some_and(|index| index >= self.options.len())
        {
            state.selected = None;
        }
        if state
            .focused
            .is_some_and(|index| index >= self.options.len())
        {
            state.set_focused(None);
        }
        if state
            .hovered
            .is_some_and(|index| !self.is_enabled_option(index))
        {
            state.hovered = None;
        }
        if state
            .pressed
            .is_some_and(|index| !self.is_enabled_option(index))
        {
            state.pressed = None;
        }
    }

    fn is_enabled_option(&self, index: usize) -> bool {
        self.options
            .get(index)
            .is_some_and(|option| !option.disabled)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Previous,
    Next,
}

impl crate::theme::ComponentTheme {
    /// Convert this semantic component theme into [`RadioGroupStyles`].
    #[must_use]
    pub fn radio_group_styles(self) -> RadioGroupStyles {
        RadioGroupStyles::from(self)
    }
}

impl From<crate::theme::ComponentTheme> for RadioGroupStyles {
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
    use bmux_keyboard::{KeyCode, KeyStroke};
    use bmux_tui::buffer::Buffer;
    use bmux_tui::component::{
        Component, Constraints, EventCx, LayoutCx, LayoutId, LayoutNode, LogicalSize,
    };
    use bmux_tui::event::{Event, EventOutcome, MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};
    use bmux_tui::paint::PaintCx;

    use super::{RadioGroup, RadioGroupComponent, RadioGroupOutcome, RadioGroupState, RadioOption};

    #[test]
    fn deep_scroll_preserves_radio_rows_and_metadata() {
        let options: Vec<_> = (0..70_003)
            .map(|index| RadioOption::new(format!("option-{index}"), format!("Row {index}")))
            .collect();
        let state = std::cell::Cell::new(RadioGroupState::new(Some(70_001)));
        let component = RadioGroupComponent::new("choice", &options, &state);
        let layout = component.layout(Constraints::for_width(16), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 16, 3));
        let mut frame = Frame::new(&mut buffer);
        PaintCx::new(&mut frame).with_child(
            0,
            -70_000,
            bmux_tui::paint::LocalRect::new(0, 70_000, 16, 3),
            |cx| component.paint(&layout, cx),
        );
        for (row, expected) in ["( ) Row 70000", "(*) Row 70001", "( ) Row 70002"]
            .iter()
            .enumerate()
        {
            assert!(
                frame
                    .buffer()
                    .row_symbols(u16::try_from(row).unwrap())
                    .unwrap()
                    .starts_with(expected)
            );
        }
        assert_eq!(frame.hits().regions().len(), 3);
        assert_eq!(frame.hits().regions()[2].area, Rect::new(0, 2, 16, 1));
        assert_eq!(frame.semantics().regions().len(), 1);
        assert_eq!(frame.semantics().regions()[0].area, Rect::new(0, 0, 16, 3));
        EventCx::new(&layout).with_transform(0, 0, 0, -70_000, Rect::new(0, 0, 16, 3), |cx| {
            for kind in [
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
            ] {
                assert!(
                    component
                        .event(
                            &Event::Mouse(MouseEvent::new(kind, Point::new(1, 2))),
                            &layout,
                            cx
                        )
                        .is_handled()
                );
            }
        });
        assert_eq!(state.get().selected(), Some(70_002));
    }

    #[test]
    fn clipped_group_mouse_selects_the_visible_logical_option() {
        let options = [
            RadioOption::new("a", "A"),
            RadioOption::new("b", "B"),
            RadioOption::new("c", "C"),
        ];
        let state = std::cell::Cell::new(RadioGroupState::new(Some(0)));
        let component = RadioGroupComponent::new("choice", &options, &state);
        let layout = component.layout(Constraints::for_width(8), &mut LayoutCx::new());
        let clip = Rect::new(4, 3, 8, 1);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 16, 6));
        let mut frame = Frame::new(&mut buffer);
        PaintCx::new(&mut frame).with_child(
            4,
            2,
            bmux_tui::paint::LocalRect::new(0, 1, 8, 1),
            |cx| component.paint(&layout, cx),
        );
        assert_eq!(frame.hits().regions().len(), 1);
        assert_eq!(frame.hits().regions()[0].area, clip);
        assert!(frame.buffer().row_symbols(3).unwrap().contains("( ) B"));
        let mut cx = EventCx::new(&layout);
        cx.with_transform(0, 0, 4, 2, clip, |cx| {
            for kind in [
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
            ] {
                assert!(
                    component
                        .event(
                            &Event::Mouse(MouseEvent::new(kind, Point::new(5, 3))),
                            &layout,
                            cx
                        )
                        .is_handled()
                );
            }
        });
        assert_eq!(state.get().selected(), Some(1));
    }

    #[test]
    fn mouse_disabled_group_keeps_keyboard_selection() {
        let options = [RadioOption::new("a", "A"), RadioOption::new("b", "B")];
        let initial = RadioGroupState::new(Some(0));
        let state = std::cell::Cell::new(initial);
        let mut policy = super::RadioGroupPolicy::interactive();
        policy.mouse.enabled = false;
        let component = RadioGroupComponent::new("choice", &options, &state).policy(policy);
        let layout = component.layout(Constraints::for_width(8), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 2));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(frame.hits().focus_targets(None).len(), 2);
        for hit in frame.hits().regions() {
            assert!(hit.enabled && hit.focusable);
            assert!(!hit.pointer_events && !hit.hoverable);
        }
        let mut cx = EventCx::new(&layout);
        for kind in [
            MouseEventKind::Move,
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            assert_eq!(
                component.event(
                    &Event::Mouse(MouseEvent::new(kind, Point::new(1, 1))),
                    &layout,
                    &mut cx
                ),
                EventOutcome::Ignored
            );
            assert_eq!(state.get(), initial);
        }
        assert!(
            component
                .event(
                    &Event::Key(KeyStroke::simple(KeyCode::Down)),
                    &layout,
                    &mut cx
                )
                .is_handled()
        );
        assert!(
            component
                .event(
                    &Event::Key(KeyStroke::simple(KeyCode::Enter)),
                    &layout,
                    &mut cx
                )
                .is_handled()
        );
        assert_eq!(state.get().selected(), Some(1));
    }

    #[test]
    fn empty_group_neither_paints_nor_changes_selection() {
        let options = [RadioOption::new("a", "A"), RadioOption::new("b", "B")];
        let initial = RadioGroupState::new(Some(0));
        let state = std::cell::Cell::new(initial);
        let component = RadioGroupComponent::new("choice", &options, &state);
        for (width, height) in [(0, 2), (8, 0), (0, 0)] {
            let layout = component.layout(
                Constraints::new(width, width, height, Some(height)),
                &mut LayoutCx::new(),
            );
            let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 2));
            let mut frame = Frame::new(&mut buffer);
            component.paint(&layout, &mut PaintCx::new(&mut frame));
            assert!(frame.hits().regions().is_empty());
            assert!(frame.semantics().regions().is_empty());
            assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("        "));
            let mut cx = EventCx::new(&layout);
            for key in [KeyCode::Down, KeyCode::Enter] {
                assert_eq!(
                    component.event(&Event::Key(KeyStroke::simple(key)), &layout, &mut cx),
                    EventOutcome::Ignored
                );
                assert_eq!(state.get(), initial);
            }
        }
    }

    #[test]
    fn focus_loss_and_resize_clear_pointer_state_even_in_empty_layouts() {
        let options = [RadioOption::new("a", "A"), RadioOption::new("b", "B")];
        let mut initial = RadioGroupState::new(Some(0));
        initial.hovered = Some(1);
        initial.pressed = Some(1);
        let state = std::cell::Cell::new(initial);
        let component = RadioGroupComponent::new("choice", &options, &state);
        for (width, height) in [(8, 2), (0, 2), (8, 0), (0, 0)] {
            let layout = component.layout(
                Constraints::new(width, width, height, Some(height)),
                &mut LayoutCx::new(),
            );
            for event in [
                Event::Focus(bmux_tui::event::FocusEvent::Lost),
                Event::Resize(Rect::new(0, 0, 8, 2).size()),
            ] {
                state.set(initial);
                let mut cx = EventCx::new(&layout);
                assert_eq!(
                    component.event(&event, &layout, &mut cx),
                    EventOutcome::Redraw
                );
                assert_eq!(state.get().hovered, None);
                assert_eq!(state.get().pressed, None);
                assert_eq!(state.get().selected, initial.selected);
                assert_eq!(state.get().focused, initial.focused);
                assert_eq!(
                    component.event(&event, &layout, &mut cx),
                    EventOutcome::Ignored
                );
            }
        }
    }

    #[test]
    fn renders_selected_option() {
        let options = vec![
            RadioOption::new("small", "Small"),
            RadioOption::new("large", "Large"),
        ];
        let state = std::cell::Cell::new(RadioGroupState::new(Some(1)));
        let component = RadioGroupComponent::new("size", &options, &state);
        let layout = component.layout(
            Constraints::tight(Rect::new(0, 0, 12, 2).size()),
            &mut LayoutCx::new(),
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 2));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));

        assert_eq!(frame.hits().focus_targets(None).len(), 2);
        assert_eq!(frame.hits().regions()[0].area, Rect::new(0, 0, 12, 1));
        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("( ) Small   ")
        );
        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("(*) Large   ")
        );
    }

    #[test]
    fn arrow_key_moves_focus_to_next_enabled_option() {
        let options = vec![
            RadioOption::new("small", "Small"),
            RadioOption::new("medium", "Medium").disabled(true),
            RadioOption::new("large", "Large"),
        ];
        let group = RadioGroup::new(&options);
        let mut state = RadioGroupState::new(Some(0));
        state.set_focused(Some(0));

        let outcome = group.handle_event(
            Rect::new(0, 0, 12, 3),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Down)),
        );

        assert_eq!(outcome, RadioGroupOutcome::Focused(2));
        assert_eq!(state.focused(), Some(2));
    }

    #[test]
    fn focused_space_selects_option() {
        let options = vec![
            RadioOption::new("small", "Small"),
            RadioOption::new("large", "Large"),
        ];
        let group = RadioGroup::new(&options);
        let mut state = RadioGroupState::new(Some(0));
        state.set_focused(Some(1));

        let outcome = group.handle_event(
            Rect::new(0, 0, 12, 2),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Space)),
        );

        assert_eq!(outcome, RadioGroupOutcome::Selected(1));
        assert_eq!(state.selected(), Some(1));
    }

    #[test]
    fn normalization_requests_redraw_for_keyboard_and_projected_mouse_paths() {
        let options = [RadioOption::new("a", "A")];
        let initial = RadioGroupState::new(Some(9));
        let state = std::cell::Cell::new(initial);
        let component = RadioGroupComponent::new("choice", &options, &state);
        let layout = component.layout(
            Constraints::tight(Rect::new(0, 0, 8, 1).size()),
            &mut LayoutCx::new(),
        );
        for event in [
            Event::Tick,
            Event::Mouse(MouseEvent::new(
                MouseEventKind::ScrollDown,
                Point::new(1, 0),
            )),
        ] {
            state.set(initial);
            let mut cx = EventCx::new(&layout);
            assert_eq!(
                component.event(&event, &layout, &mut cx),
                EventOutcome::Redraw
            );
            assert_eq!(state.get().selected(), None);
            assert_eq!(
                component.event(&event, &layout, &mut cx),
                EventOutcome::Ignored
            );
        }
    }

    #[test]
    fn disabling_pressed_option_cancels_activation_after_reenable() {
        let options = [RadioOption::new("a", "A"), RadioOption::new("b", "B")];
        let disabled = [
            RadioOption::new("a", "A"),
            RadioOption::new("b", "B").disabled(true),
        ];
        let group = RadioGroup::new(&options);
        let area = Rect::new(0, 0, 8, 2);
        let mut state = RadioGroupState::new(Some(0));
        group.handle_event(
            area,
            &mut state,
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(1, 1),
            )),
        );
        assert_eq!(state.pressed, Some(1));
        RadioGroup::new(&disabled).handle_event(area, &mut state, &Event::Tick);
        assert_eq!(state.pressed, None);
        assert_eq!(state.hovered, None);
        assert_eq!(
            group.handle_event(
                area,
                &mut state,
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Up(MouseButton::Left),
                    Point::new(1, 1),
                ))
            ),
            RadioGroupOutcome::Ignored
        );
        assert_eq!(state.selected(), Some(0));
    }

    #[test]
    fn disabling_click_before_release_cancels_pending_selection() {
        let options = [RadioOption::new("a", "A"), RadioOption::new("b", "B")];
        let area = Rect::new(0, 0, 8, 2);
        let mut state = RadioGroupState::new(Some(0));
        let group = RadioGroup::new(&options);
        group.handle_event(
            area,
            &mut state,
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(1, 1),
            )),
        );
        assert_eq!(state.pressed, Some(1));
        let mut policy = super::RadioGroupPolicy::default();
        policy.mouse.click = false;
        let release = Event::Mouse(MouseEvent::new(
            MouseEventKind::Up(MouseButton::Left),
            Point::new(1, 1),
        ));
        assert_eq!(
            RadioGroup::new(&options)
                .policy(policy)
                .handle_event(area, &mut state, &release),
            RadioGroupOutcome::Redraw
        );
        assert_eq!(state.pressed, None);
        assert_eq!(state.selected(), Some(0));
        assert_eq!(
            group.handle_event(area, &mut state, &release),
            RadioGroupOutcome::Ignored
        );
        assert_eq!(state.selected(), Some(0));
    }

    #[test]
    fn mouse_click_focuses_and_selects_option() {
        let options = vec![
            RadioOption::new("small", "Small"),
            RadioOption::new("large", "Large"),
        ];
        let group = RadioGroup::new(&options);
        let mut state = RadioGroupState::new(Some(0));
        let area = Rect::new(0, 0, 12, 2);

        let down = group.handle_event(
            area,
            &mut state,
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(1, 1),
            )),
        );
        let up = group.handle_event(
            area,
            &mut state,
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Up(MouseButton::Left),
                Point::new(1, 1),
            )),
        );

        assert_eq!(down, RadioGroupOutcome::Redraw);
        assert_eq!(up, RadioGroupOutcome::Selected(1));
        assert_eq!(state.focused(), Some(1));
        assert_eq!(state.selected(), Some(1));
    }

    #[test]
    fn component_measures_paints_and_registers_options() {
        let options = vec![
            RadioOption::new("small", "Small"),
            RadioOption::new("large", "Large"),
        ];
        let state = std::cell::Cell::new(RadioGroupState::new(Some(1)));
        let component = RadioGroupComponent::new("size", &options, &state);
        let layout = component.layout(Constraints::for_width(12), &mut LayoutCx::new());
        assert_eq!(layout.size, LogicalSize::new(12, 2));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 2));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("(*) Large   ")
        );
        assert_eq!(frame.hits().regions().len(), 2);
        assert_eq!(frame.semantics().regions().len(), 1);
    }

    #[test]
    fn component_routes_events_through_authoritative_layout() {
        let options = vec![
            RadioOption::new("small", "Small"),
            RadioOption::new("large", "Large"),
        ];
        let state = std::cell::Cell::new(RadioGroupState::new(Some(0)));
        state.set({
            let mut value = state.get();
            value.set_focused(Some(0));
            value
        });
        let component = RadioGroupComponent::new("size", &options, &state);
        let layout = LayoutNode::leaf(LayoutId::new("size"), LogicalSize::new(12, 2));
        let mut event_cx = EventCx::new(&layout);
        let outcome = component.event(
            &Event::Key(KeyStroke::simple(KeyCode::Down)),
            &layout,
            &mut event_cx,
        );
        assert_eq!(outcome, EventOutcome::Redraw);
        assert_eq!(state.get().focused(), Some(1));
    }

    #[test]
    fn radio_semantics_exclude_blank_layout_rows() {
        let options = [RadioOption::new("a", "A")];
        let state = std::cell::Cell::new(RadioGroupState::new(None));
        let component = RadioGroupComponent::new("choice", &options, &state);
        let layout = component.layout(Constraints::new(10, 10, 3, Some(3)), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 3));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(frame.semantics().regions().len(), 1);
        assert_eq!(frame.semantics().regions()[0].area, Rect::new(0, 0, 10, 1));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 3));
        let mut frame = Frame::new(&mut buffer);
        PaintCx::new(&mut frame).with_child(
            0,
            -1,
            bmux_tui::paint::LocalRect::new(0, 1, 10, 2),
            |cx| component.paint(&layout, cx),
        );
        assert!(frame.hits().regions().is_empty());
        assert!(frame.semantics().regions().is_empty());
    }

    #[test]
    fn clipped_radio_options_cannot_select_through_blank_layout_rows() {
        let options = [RadioOption::new("a", "A")];
        let mut initial = RadioGroupState::new(None);
        initial.set_focused(Some(0));
        let state = std::cell::Cell::new(initial);
        let component = RadioGroupComponent::new("choice", &options, &state);
        let layout = component.layout(Constraints::new(10, 10, 3, Some(3)), &mut LayoutCx::new());
        EventCx::new(&layout).with_transform(0, 0, 0, 0, Rect::new(0, 1, 10, 2), |cx| {
            assert!(
                cx.find_rect(&layout.id)
                    .is_some_and(|rect| !rect.is_empty())
            );
            assert_eq!(
                component.event(&Event::Key(KeyStroke::simple(KeyCode::Enter)), &layout, cx),
                EventOutcome::Ignored
            );
        });
        assert_eq!(state.get(), initial);
        assert_eq!(
            component.event(
                &Event::Key(KeyStroke::simple(KeyCode::Enter)),
                &layout,
                &mut EventCx::new(&layout),
            ),
            EventOutcome::Redraw
        );
        assert_eq!(state.get().selected, Some(0));
    }

    #[test]
    fn hidden_radio_group_cancels_disabled_pointer_without_selection() {
        let options = [RadioOption::new("a", "A")];
        for (width, height) in [(0, 1), (10, 0), (0, 0)] {
            let mut initial = RadioGroupState::new(None);
            initial.pressed = Some(0);
            initial.hovered = Some(0);
            initial.set_focused(Some(0));
            let state = std::cell::Cell::new(initial);
            let mut component = RadioGroupComponent::new("choice", &options, &state);
            component.group.policy.mouse.enabled = false;
            let layout = component.layout(
                Constraints::new(width, width, height, Some(height)),
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
            assert_eq!(state.get().pressed, None);
            assert_eq!(state.get().hovered, None);
            assert_eq!(state.get().selected, None);
            component.group.policy.mouse.enabled = true;
            let visible = component.layout(Constraints::for_width(10), &mut LayoutCx::new());
            component.event(
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Up(MouseButton::Left),
                    Point::new(1, 0),
                )),
                &visible,
                &mut EventCx::new(&visible),
            );
            assert_eq!(state.get().selected, None);
        }
    }

    #[test]
    fn hover_disabled_radio_press_and_drag_still_select() {
        let options = [RadioOption::new("a", "A"), RadioOption::new("b", "B")];
        let area = Rect::new(0, 0, 10, 2);
        let mut group = RadioGroup::new(&options);
        group.policy.mouse.hover = false;
        let mut state = RadioGroupState::new(None);
        for (kind, row, pressed) in [
            (MouseEventKind::Down(MouseButton::Left), 0, Some(0)),
            (MouseEventKind::Drag(MouseButton::Left), 1, Some(1)),
            (MouseEventKind::Up(MouseButton::Left), 1, None),
        ] {
            group.handle_event(
                area,
                &mut state,
                &Event::Mouse(MouseEvent::new(kind, Point::new(1, row))),
            );
            assert_eq!(state.hovered, None);
            assert_eq!(state.pressed, pressed);
        }
        assert_eq!(state.selected, Some(1));
    }

    #[test]
    fn disabling_radio_click_cancels_press_and_preserves_keyboard_selection() {
        let options = [RadioOption::new("a", "A")];
        let area = Rect::new(0, 0, 10, 1);
        let mut group = RadioGroup::new(&options);
        let mut state = RadioGroupState::new(None);
        state.set_focused(Some(0));
        let mouse = |kind| Event::Mouse(MouseEvent::new(kind, Point::new(1, 0)));
        group.handle_event(
            area,
            &mut state,
            &mouse(MouseEventKind::Down(MouseButton::Left)),
        );
        assert_eq!(state.pressed, Some(0));
        group.policy.mouse.click = false;
        assert_eq!(
            group.handle_event(area, &mut state, &Event::Tick),
            RadioGroupOutcome::Redraw
        );
        assert_eq!(state.pressed, None);
        group.policy.mouse.click = true;
        group.handle_event(
            area,
            &mut state,
            &mouse(MouseEventKind::Up(MouseButton::Left)),
        );
        assert_eq!(state.selected, None);
        group.policy.mouse.click = false;
        assert_eq!(
            group.handle_event(
                area,
                &mut state,
                &Event::Key(KeyStroke::simple(KeyCode::Enter)),
            ),
            RadioGroupOutcome::Selected(0)
        );
        state.hovered = Some(0);
        group.policy.mouse.hover = false;
        assert_eq!(
            group.handle_event(area, &mut state, &Event::Tick),
            RadioGroupOutcome::Redraw
        );
        assert_eq!(state.hovered, None);
        assert_eq!(state.selected, Some(0));
    }

    #[test]
    fn disabling_pointer_input_cancels_pending_radio_press() {
        let options = [RadioOption::new("a", "A")];
        let area = Rect::new(0, 0, 10, 1);
        for disable_control in [false, true] {
            let mut group = RadioGroup::new(&options);
            let mut state = RadioGroupState::new(None);
            let mouse = |kind| Event::Mouse(MouseEvent::new(kind, Point::new(1, 0)));
            group.handle_event(
                area,
                &mut state,
                &mouse(MouseEventKind::Down(MouseButton::Left)),
            );
            assert_eq!(state.pressed, Some(0));
            if disable_control {
                state.interaction.disabled = true;
            } else {
                group.policy.mouse.enabled = false;
            }
            assert_eq!(
                group.handle_event(area, &mut state, &Event::Tick),
                RadioGroupOutcome::Redraw
            );
            assert_eq!(state.pressed, None);
            assert_eq!(state.hovered, None);
            assert_eq!(
                group.handle_event(area, &mut state, &Event::Tick),
                RadioGroupOutcome::Ignored
            );
            state.interaction.disabled = false;
            group.policy.mouse.enabled = true;
            assert_eq!(
                group.handle_event(
                    area,
                    &mut state,
                    &mouse(MouseEventKind::Up(MouseButton::Left))
                ),
                RadioGroupOutcome::Ignored
            );
            assert_eq!(state.selected(), None);
        }
    }

    #[test]
    fn component_disabled_option_is_not_enabled_in_scene() {
        let options = vec![
            RadioOption::new("small", "Small"),
            RadioOption::new("large", "Large").disabled(true),
        ];
        let state = std::cell::Cell::new(RadioGroupState::new(Some(0)));
        let component = RadioGroupComponent::new("size", &options, &state);
        let layout = component.layout(Constraints::for_width(12), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 2));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        assert!(frame.hits().regions()[0].enabled);
        assert!(!frame.hits().regions()[1].enabled);
    }

    #[test]
    fn disabled_group_ignores_events() {
        let options = vec![RadioOption::new("small", "Small")];
        let group = RadioGroup::new(&options);
        let mut state = RadioGroupState::new(Some(0));
        state.set_disabled(true);
        state.set_focused(Some(0));

        let outcome = group.handle_event(
            Rect::new(0, 0, 12, 1),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Space)),
        );

        assert_eq!(outcome, RadioGroupOutcome::Ignored);
        assert_eq!(state.selected(), Some(0));
    }
}
