//! Configurable radio-group component.

use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::hit::{HitId, HitRegion as SceneRegion, HitRole};
use bmux_tui::prelude::{Line, Span, Style};
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

    /// Render the radio group.
    pub fn render(&self, area: Rect, state: &RadioGroupState, frame: &mut Frame<'_>) {
        let id = frame.next_interaction_id("radio-group");
        self.render_with_id(id, area, state, frame);
    }

    /// Render and register this composite as one roving-focus tab stop.
    pub fn render_with_id(
        &self,
        id: impl Into<HitId>,
        area: Rect,
        state: &RadioGroupState,
        frame: &mut Frame<'_>,
    ) {
        frame.push_hit(
            SceneRegion::new(id, area)
                .role(HitRole::ListItem)
                .hoverable(self.policy.mouse.hover)
                .focusable(true)
                .enabled(!state.interaction.disabled),
        );
        self.render_body(area, state, frame, Style::new());
    }

    /// Render the radio group with a fallback style filling each option row.
    pub fn render_with_fallback_style(
        &self,
        area: Rect,
        state: &RadioGroupState,
        frame: &mut Frame<'_>,
        fallback: Style,
    ) {
        let id = frame.next_interaction_id("radio-group");
        frame.push_hit(
            SceneRegion::new(id, area)
                .role(HitRole::ListItem)
                .hoverable(self.policy.mouse.hover)
                .focusable(true)
                .enabled(!state.interaction.disabled),
        );
        self.render_body(area, state, frame, fallback);
    }

    fn render_body(
        &self,
        area: Rect,
        state: &RadioGroupState,
        frame: &mut Frame<'_>,
        fallback: Style,
    ) {
        for (index, option) in self
            .options
            .iter()
            .take(usize::from(area.height))
            .enumerate()
        {
            let row = area
                .y
                .saturating_add(u16::try_from(index).unwrap_or(u16::MAX));
            frame.write_line_with_fallback_style(
                Rect::new(area.x, row, area.width, 1),
                &self.line(index, option, *state),
                fallback,
            );
        }
    }

    /// Handle one input event.
    pub fn handle_event(
        &self,
        area: Rect,
        state: &mut RadioGroupState,
        event: &Event,
    ) -> RadioGroupOutcome {
        self.normalize_state(state);
        if state.interaction.disabled {
            return RadioGroupOutcome::Ignored;
        }
        match event {
            Event::Key(stroke) => self.handle_key(state, *stroke),
            Event::Mouse(mouse) => self.handle_mouse(area, state, *mouse),
            Event::Resize(_) | Event::Paste(_) | Event::Focus(_) | Event::Tick | Event::User(_) => {
                RadioGroupOutcome::Ignored
            }
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
        if !self.policy.mouse.enabled {
            return RadioGroupOutcome::Ignored;
        }
        let hit = self.hit_index(area, mouse);
        match mouse.kind {
            MouseEventKind::Move if self.policy.mouse.hover => Self::hover(state, hit),
            MouseEventKind::Down(MouseButton::Left) if self.policy.mouse.click => {
                Self::press(state, hit)
            }
            MouseEventKind::Up(MouseButton::Left) if self.policy.mouse.click => {
                self.release(state, hit)
            }
            MouseEventKind::Drag(MouseButton::Left) if self.policy.mouse.click => {
                Self::drag(state, hit)
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

    const fn press(state: &mut RadioGroupState, hit: Option<usize>) -> RadioGroupOutcome {
        let Some(index) = hit else {
            return RadioGroupOutcome::Ignored;
        };
        state.pressed = Some(index);
        state.hovered = Some(index);
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

    fn drag(state: &mut RadioGroupState, hit: Option<usize>) -> RadioGroupOutcome {
        let pressed = if state.pressed.is_some() { hit } else { None };
        if state.hovered == hit && state.pressed == pressed {
            RadioGroupOutcome::Ignored
        } else {
            state.hovered = hit;
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
            .is_some_and(|index| index >= self.options.len())
        {
            state.hovered = None;
        }
        if state
            .pressed
            .is_some_and(|index| index >= self.options.len())
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
    use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};

    use super::{RadioGroup, RadioGroupOutcome, RadioGroupState, RadioOption};

    #[test]
    fn renders_selected_option() {
        let options = vec![
            RadioOption::new("small", "Small"),
            RadioOption::new("large", "Large"),
        ];
        let group = RadioGroup::new(&options);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 2));
        let mut frame = Frame::new(&mut buffer);

        group.render(
            Rect::new(0, 0, 12, 2),
            &RadioGroupState::new(Some(1)),
            &mut frame,
        );

        assert_eq!(frame.hits().focus_targets(None).len(), 1);
        assert_eq!(frame.hits().regions()[0].area, Rect::new(0, 0, 12, 2));
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
