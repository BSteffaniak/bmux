//! Generic tab bar / segmented selector component.

use std::cell::RefCell;
use std::hash::{Hash, Hasher};

use bmux_keyboard::KeyCode;
use bmux_tui::component::{
    Component, ComponentRevision, Constraints, EventCx, LayoutCx, LayoutId, LayoutMetadata,
    LayoutNode, LogicalSize,
};
use bmux_tui::event::{Event, EventOutcome, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::geometry::Rect;
use bmux_tui::hit::{HitRegion as SceneRegion, HitRole};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span};
use bmux_tui::semantic::SemanticRegion;
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui::text_width::display_width;

use crate::common::{ComponentMousePolicy, InteractionState, u16_saturating};
use crate::hit_test::{HitRegion, hit_region_at};

/// One tab-bar item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabItem<'a> {
    /// Stable tab id chosen by the caller.
    pub id: &'a str,
    /// Visible tab label.
    pub label: Line,
    /// Whether this tab is disabled.
    pub disabled: bool,
}

impl<'a> TabItem<'a> {
    /// Create an enabled tab item.
    #[must_use]
    pub fn new(id: &'a str, label: &'a str) -> Self {
        Self {
            id,
            label: Line::from(label),
            disabled: false,
        }
    }

    /// Create an enabled tab item from rich label content.
    #[must_use]
    pub const fn rich(id: &'a str, label: Line) -> Self {
        Self {
            id,
            label,
            disabled: false,
        }
    }

    /// Return visible label as plain text.
    #[must_use]
    pub fn label(&self) -> String {
        self.label.plain_text()
    }

    /// Return this item with disabled state set.
    #[must_use]
    pub const fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

/// Keyboard behavior for [`TabBar`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabBarKeyboardPolicy {
    /// Whether keyboard events are accepted.
    pub enabled: bool,
    /// Whether navigation wraps at edges.
    pub wrap: bool,
    /// Whether Home/End jump to first/last enabled tab.
    pub home_end: bool,
}

impl TabBarKeyboardPolicy {
    /// Keyboard behavior disabled.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            wrap: false,
            home_end: false,
        }
    }

    /// Standard tab navigation.
    #[must_use]
    pub const fn navigation() -> Self {
        Self {
            enabled: true,
            wrap: true,
            home_end: true,
        }
    }
}

impl Default for TabBarKeyboardPolicy {
    fn default() -> Self {
        Self::navigation()
    }
}

/// Overflow behavior for [`TabBar`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabBarOverflow {
    /// Truncate the rendered tab line to fit.
    Truncate,
    /// Render as much as fits and leave the rest absent.
    Clip,
}

/// Behavior policy for [`TabBar`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabBarPolicy {
    /// Keyboard behavior.
    pub keyboard: TabBarKeyboardPolicy,
    /// Mouse behavior.
    pub mouse: ComponentMousePolicy,
    /// Overflow behavior.
    pub overflow: TabBarOverflow,
    /// Separator between tabs.
    pub separator: &'static str,
    /// Left padding for each tab label.
    pub padding_left: &'static str,
    /// Right padding for each tab label.
    pub padding_right: &'static str,
}

impl TabBarPolicy {
    /// Bare tab rendering with no input handling.
    #[must_use]
    pub const fn bare() -> Self {
        Self {
            keyboard: TabBarKeyboardPolicy::disabled(),
            mouse: ComponentMousePolicy::disabled(),
            overflow: TabBarOverflow::Truncate,
            separator: " ",
            padding_left: " ",
            padding_right: " ",
        }
    }

    /// Interactive keyboard and mouse selection.
    #[must_use]
    pub const fn interactive() -> Self {
        Self {
            keyboard: TabBarKeyboardPolicy::navigation(),
            mouse: ComponentMousePolicy::button(),
            overflow: TabBarOverflow::Truncate,
            separator: " ",
            padding_left: " ",
            padding_right: " ",
        }
    }
    /// Return this policy with custom separator.
    #[must_use]
    pub const fn separator(mut self, separator: &'static str) -> Self {
        self.separator = separator;
        self
    }

    /// Return this policy with custom tab label padding.
    #[must_use]
    pub const fn padding(mut self, left: &'static str, right: &'static str) -> Self {
        self.padding_left = left;
        self.padding_right = right;
        self
    }
}

impl Default for TabBarPolicy {
    fn default() -> Self {
        Self::interactive()
    }
}

/// Visual styles for [`TabBar`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabBarStyles {
    /// Inactive enabled tab style.
    pub normal: Style,
    /// Selected tab style.
    pub selected: Style,
    /// Focused tab style.
    pub focused: Style,
    /// Hovered tab style.
    pub hovered: Style,
    /// Pressed tab style.
    pub pressed: Style,
    /// Disabled tab style.
    pub disabled: Style,
    /// Separator style.
    pub separator: Style,
}

impl Default for TabBarStyles {
    fn default() -> Self {
        Self {
            normal: Style::new().fg(Color::BrightBlack),
            selected: Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            focused: Style::new()
                .fg(Color::White)
                .add_modifier(Modifier::UNDERLINE),
            hovered: Style::new().fg(Color::White),
            pressed: Style::new().fg(Color::Black).bg(Color::BrightCyan),
            disabled: Style::new().fg(Color::BrightBlack),
            separator: Style::new().fg(Color::BrightBlack),
        }
    }
}

/// Runtime tab-bar state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TabBarState {
    selected: Option<usize>,
    hovered: Option<usize>,
    pressed: Option<usize>,
    /// Generic interaction flags.
    pub interaction: InteractionState,
}

impl TabBarState {
    /// Create state with the supplied selected index.
    #[must_use]
    pub const fn new(selected: Option<usize>) -> Self {
        Self {
            selected,
            hovered: None,
            pressed: None,
            interaction: InteractionState::new(),
        }
    }

    /// Return selected tab index.
    #[must_use]
    pub const fn selected(&self) -> Option<usize> {
        self.selected
    }

    /// Set selected tab index.
    pub const fn set_selected(&mut self, selected: Option<usize>) {
        self.selected = selected;
    }

    /// Set whether this composite currently owns keyboard focus.
    pub const fn set_focused(&mut self, focused: bool) {
        self.interaction.focused = focused;
    }

    /// Return hovered tab index.
    #[must_use]
    pub const fn hovered(&self) -> Option<usize> {
        self.hovered
    }
}

/// Outcome from tab-bar input handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabBarOutcome {
    /// Event was ignored.
    Ignored,
    /// Visual state changed.
    Redraw,
    /// Selection changed to tab index.
    Selected(usize),
}

/// Canonical component-lifecycle tab bar.
pub struct TabBarComponent<'a, 'state> {
    id: LayoutId,
    bar: TabBar<'a>,
    state: &'state RefCell<TabBarState>,
}

impl<'a, 'state> TabBarComponent<'a, 'state> {
    /// Create a tab bar with stable identity and caller-owned state.
    #[must_use]
    pub fn new(
        id: impl Into<LayoutId>,
        items: &'a [TabItem<'a>],
        state: &'state RefCell<TabBarState>,
    ) -> Self {
        Self {
            id: id.into(),
            bar: TabBar::new(items),
            state,
        }
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: TabBarPolicy) -> Self {
        self.bar.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: TabBarStyles) -> Self {
        self.bar.styles = styles;
        self
    }
}

impl Component for TabBarComponent<'_, '_> {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        for item in self.bar.items {
            item.id.hash(&mut layout);
            format!("{:?}", item.label).hash(&mut layout);
        }
        self.bar.policy.separator.hash(&mut layout);
        self.bar.policy.padding_left.hash(&mut layout);
        self.bar.policy.padding_right.hash(&mut layout);
        format!("{:?}", self.bar.policy.overflow).hash(&mut layout);

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        format!("{:?}", self.bar.policy.keyboard).hash(&mut paint);
        format!("{:?}", self.bar.policy.mouse).hash(&mut paint);
        format!("{:?}", self.bar.styles).hash(&mut paint);
        for item in self.bar.items {
            item.disabled.hash(&mut paint);
        }
        format!("{:?}", self.state.borrow()).hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let width = u16::try_from(display_width(&self.bar.text())).unwrap_or(u16::MAX);
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(
                width,
                usize::from(!self.bar.items.is_empty()),
            )),
        )
        .with_metadata(LayoutMetadata::new().semantic("tabs"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        if layout.size.width == 0 || layout.size.height == 0 || self.bar.items.is_empty() {
            return;
        }
        let state = self.state.borrow();
        let area = Rect::new(0, 0, layout.size.width, 1);
        cx.push_hit(
            SceneRegion::new(self.id.as_str(), area)
                .role(HitRole::Action)
                .pointer_events(self.bar.policy.mouse.enabled)
                .hoverable(self.bar.policy.mouse.enabled && self.bar.policy.mouse.hover)
                .focusable(true)
                .enabled(!state.interaction.disabled),
        );
        for (index, item_area) in self.bar.hit_rects(area).into_iter().enumerate() {
            let Some(item) = self.bar.items.get(index) else {
                break;
            };
            cx.push_hit(
                SceneRegion::new(format!("{}.{}", self.id.as_str(), item.id), item_area)
                    .role(HitRole::Action)
                    .pointer_events(self.bar.policy.mouse.enabled)
                    .hoverable(self.bar.policy.mouse.enabled && self.bar.policy.mouse.hover)
                    .enabled(!state.interaction.disabled && !item.disabled),
            );
        }
        let line = self.bar.line(&state);
        let line = if matches!(self.bar.policy.overflow, TabBarOverflow::Truncate) {
            line.truncate(usize::from(layout.size.width))
        } else {
            line
        };
        let local = LocalRect::new(0, 0, layout.size.width, 1);
        cx.write_line(local, &line);
        cx.push_semantic(SemanticRegion::new(self.id.as_str(), area, "tabs"));
        cx.push_damage(local);
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        let Some(area) = cx.find_rect(&layout.id) else {
            return EventOutcome::Ignored;
        };
        let area = cx
            .visible_rect(bmux_tui::component::LogicalRect::new(
                0,
                0,
                usize::from(layout.size.width),
                usize::from(!self.bar.items.is_empty()).min(layout.size.height),
            ))
            .intersection(area);
        // Hidden tabs reconcile policy and lifecycle without handling activation.
        let event = if area.is_empty() && !matches!(event, Event::Focus(_) | Event::Resize(_)) {
            &Event::Tick
        } else {
            event
        };
        let mut state = self.state.borrow_mut();
        let outcome = if let Event::Mouse(mouse) = event {
            let fallback = self.bar.handle_event(area, &mut state, &Event::Tick);
            let hit = self
                .bar
                .hit_rects(Rect::new(0, 0, layout.size.width, 1))
                .iter()
                .position(|rect| {
                    cx.visible_rect(bmux_tui::component::LogicalRect::new(
                        rect.x.into(),
                        usize::from(rect.y),
                        usize::from(rect.width),
                        usize::from(rect.height),
                    ))
                    .contains(mouse.position)
                });
            let outcome = if state.interaction.disabled || !self.bar.policy.mouse.enabled {
                TabBarOutcome::Ignored
            } else {
                self.bar.handle_mouse_hit(&mut state, *mouse, hit)
            };
            if outcome == TabBarOutcome::Ignored {
                fallback
            } else {
                outcome
            }
        } else {
            self.bar.handle_event(area, &mut state, event)
        };
        match outcome {
            TabBarOutcome::Ignored => EventOutcome::Ignored,
            TabBarOutcome::Redraw | TabBarOutcome::Selected(_) => EventOutcome::Redraw,
        }
    }
}

/// Generic tab bar / segmented selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabBar<'a> {
    items: &'a [TabItem<'a>],
    policy: TabBarPolicy,
    styles: TabBarStyles,
}

impl<'a> TabBar<'a> {
    /// Create a tab bar over caller-owned items.
    #[must_use]
    pub const fn new(items: &'a [TabItem<'a>]) -> Self {
        Self {
            items,
            policy: TabBarPolicy {
                keyboard: TabBarKeyboardPolicy {
                    enabled: true,
                    wrap: true,
                    home_end: true,
                },
                mouse: ComponentMousePolicy {
                    enabled: true,
                    hover: true,
                    click: true,
                },
                overflow: TabBarOverflow::Truncate,
                separator: " ",
                padding_left: " ",
                padding_right: " ",
            },
            styles: TabBarStyles {
                normal: Style::new(),
                selected: Style::new(),
                focused: Style::new(),
                hovered: Style::new(),
                pressed: Style::new(),
                disabled: Style::new(),
                separator: Style::new(),
            },
        }
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: TabBarPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: TabBarStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Return tab hit rectangles for `area`.
    #[must_use]
    pub fn hit_rects(&self, area: Rect) -> Vec<Rect> {
        let mut rects = Vec::with_capacity(self.items.len());
        let mut x = area.x;
        for (index, item) in self.items.iter().enumerate() {
            if index > 0 {
                x = x.saturating_add(u16_saturating(display_width(self.policy.separator)));
            }
            let width = u16_saturating(tab_label_width_with_policy(item, self.policy));
            rects.push(Rect::new(x, area.y, width, area.height.min(1)));
            x = x.saturating_add(width);
        }
        rects
    }

    /// Return unstyled rendered text.
    #[must_use]
    pub fn text(&self) -> String {
        self.items
            .iter()
            .map(|item| {
                format!(
                    "{}{}{}",
                    self.policy.padding_left,
                    item.label.plain_text(),
                    self.policy.padding_right
                )
            })
            .collect::<Vec<_>>()
            .join(self.policy.separator)
    }

    /// Handle one event.
    pub fn handle_event(
        &self,
        area: Rect,
        state: &mut TabBarState,
        event: &Event,
    ) -> TabBarOutcome {
        if matches!(
            event,
            Event::Focus(bmux_tui::event::FocusEvent::Lost) | Event::Resize(_)
        ) {
            let changed = state.pressed.take().is_some() | state.hovered.take().is_some();
            return if changed {
                TabBarOutcome::Redraw
            } else {
                TabBarOutcome::Ignored
            };
        }
        let previous_pointer = (state.pressed, state.hovered);
        let enabled = |index: usize| {
            !state.interaction.disabled
                && self.policy.mouse.enabled
                && self.items.get(index).is_some_and(|item| !item.disabled)
        };
        state.pressed = state
            .pressed
            .filter(|&index| enabled(index) && self.policy.mouse.click);
        state.hovered = state
            .hovered
            .filter(|&index| enabled(index) && self.policy.mouse.hover);
        let fallback = if previous_pointer == (state.pressed, state.hovered) {
            TabBarOutcome::Ignored
        } else {
            TabBarOutcome::Redraw
        };
        if state.interaction.disabled {
            return fallback;
        }
        let outcome = match event {
            Event::Key(stroke) if self.policy.keyboard.enabled && stroke.modifiers.is_empty() => {
                match stroke.key {
                    KeyCode::Left => self.select_relative(state, -1),
                    KeyCode::Right => self.select_relative(state, 1),
                    KeyCode::Home if self.policy.keyboard.home_end => {
                        self.select_endpoint(state, true)
                    }
                    KeyCode::End if self.policy.keyboard.home_end => {
                        self.select_endpoint(state, false)
                    }
                    _ => TabBarOutcome::Ignored,
                }
            }
            Event::Mouse(mouse) if self.policy.mouse.enabled => {
                self.handle_mouse(area, state, *mouse)
            }
            Event::Key(_)
            | Event::Mouse(_)
            | Event::Resize(_)
            | Event::Paste(_)
            | Event::Focus(_)
            | Event::Tick
            | Event::User(_) => TabBarOutcome::Ignored,
        };
        if outcome == TabBarOutcome::Ignored {
            fallback
        } else {
            outcome
        }
    }

    fn line(&self, state: &TabBarState) -> Line {
        let mut spans = Vec::new();
        for (index, item) in self.items.iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(self.policy.separator, self.styles.separator));
            }
            let style = self.item_style(index, item, state);
            spans.push(Span::styled(self.policy.padding_left, style));
            spans.extend(
                item.label
                    .spans
                    .iter()
                    .map(|span| Span::styled(span.content.clone(), style.patch(span.style))),
            );
            spans.push(Span::styled(self.policy.padding_right, style));
        }
        Line::from_spans(spans)
    }

    fn item_style(&self, index: usize, item: &TabItem<'_>, state: &TabBarState) -> Style {
        if item.disabled {
            self.styles.disabled
        } else if state.pressed == Some(index) {
            self.styles.pressed
        } else if state.selected == Some(index) {
            self.styles.selected
        } else if state.hovered == Some(index) {
            self.styles.hovered
        } else if state.interaction.focused {
            self.styles.focused
        } else {
            self.styles.normal
        }
    }

    fn handle_mouse(
        &self,
        area: Rect,
        state: &mut TabBarState,
        mouse: MouseEvent,
    ) -> TabBarOutcome {
        let hit = self.hit_index(area, mouse.position.x, mouse.position.y);
        self.handle_mouse_hit(state, mouse, hit)
    }

    fn handle_mouse_hit(
        &self,
        state: &mut TabBarState,
        mouse: MouseEvent,
        hit: Option<usize>,
    ) -> TabBarOutcome {
        match mouse.kind {
            MouseEventKind::Move if self.policy.mouse.hover => {
                let hovered = hit.filter(|index| !self.items[*index].disabled);
                if hovered == state.hovered {
                    TabBarOutcome::Ignored
                } else {
                    state.hovered = hovered;
                    TabBarOutcome::Redraw
                }
            }
            MouseEventKind::Down(MouseButton::Left) if self.policy.mouse.click => {
                let pressed = hit.filter(|index| !self.items[*index].disabled);
                let previous = state.pressed;
                state.pressed = pressed;
                if pressed.is_some() || previous != pressed {
                    TabBarOutcome::Redraw
                } else {
                    TabBarOutcome::Ignored
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let pressed = state.pressed.take();
                if self.policy.mouse.click
                    && let (Some(pressed), Some(hit)) = (pressed, hit)
                    && pressed == hit
                    && !self.items[hit].disabled
                {
                    state.selected = Some(hit);
                    return TabBarOutcome::Selected(hit);
                }
                if pressed.is_some() {
                    TabBarOutcome::Redraw
                } else {
                    TabBarOutcome::Ignored
                }
            }
            MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::Move
            | MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => TabBarOutcome::Ignored,
        }
    }

    fn hit_regions(&self, area: Rect) -> Vec<HitRegion<usize>> {
        self.hit_rects(area)
            .into_iter()
            .enumerate()
            .map(|(index, rect)| HitRegion::new(index, rect))
            .collect()
    }

    fn hit_index(&self, area: Rect, x: u16, y: u16) -> Option<usize> {
        hit_region_at(
            &self.hit_regions(area),
            bmux_tui::geometry::Point::new(x, y),
        )
        .map(|region| region.key)
    }

    fn select_relative(&self, state: &mut TabBarState, delta: i32) -> TabBarOutcome {
        let Some(next) = next_enabled(self.items, state.selected, delta, self.policy.keyboard.wrap)
        else {
            return TabBarOutcome::Ignored;
        };
        state.selected = Some(next);
        TabBarOutcome::Selected(next)
    }

    fn select_endpoint(&self, state: &mut TabBarState, first: bool) -> TabBarOutcome {
        let next = if first {
            self.items.iter().position(|item| !item.disabled)
        } else {
            self.items.iter().rposition(|item| !item.disabled)
        };
        let Some(next) = next else {
            return TabBarOutcome::Ignored;
        };
        state.selected = Some(next);
        TabBarOutcome::Selected(next)
    }
}

fn next_enabled(
    items: &[TabItem<'_>],
    selected: Option<usize>,
    delta: i32,
    wrap: bool,
) -> Option<usize> {
    if items.is_empty() {
        return None;
    }
    let mut index = selected.unwrap_or(0).min(items.len().saturating_sub(1));
    for _ in 0..items.len() {
        index = if delta.is_negative() {
            if index == 0 {
                if wrap {
                    items.len().saturating_sub(1)
                } else {
                    return None;
                }
            } else {
                index.saturating_sub(1)
            }
        } else if index + 1 >= items.len() {
            if wrap {
                0
            } else {
                return None;
            }
        } else {
            index.saturating_add(1)
        };
        if !items[index].disabled {
            return Some(index);
        }
    }
    None
}

fn tab_label_width_with_policy(item: &TabItem<'_>, policy: TabBarPolicy) -> usize {
    display_width(policy.padding_left)
        .saturating_add(display_width(&item.label.plain_text()))
        .saturating_add(display_width(policy.padding_right))
}

impl crate::theme::ComponentTheme {
    /// Convert this semantic component theme into [`TabBarStyles`].
    #[must_use]
    pub fn tab_bar_styles(self) -> TabBarStyles {
        TabBarStyles::from(self)
    }
}

impl From<crate::theme::ComponentTheme> for TabBarStyles {
    fn from(theme: crate::theme::ComponentTheme) -> Self {
        let theme = theme.for_surface(crate::theme::ComponentSurfaceDepth::Normal);
        Self {
            normal: theme.muted,
            selected: theme.selected.add_modifier(bmux_tui::style::Modifier::BOLD),
            focused: theme.focused,
            hovered: theme.info,
            pressed: theme.selected,
            disabled: theme.disabled,
            separator: theme.border,
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
    use bmux_tui::paint::{LocalRect, PaintCx};
    use bmux_tui::prelude::{Line, Span};
    use bmux_tui::style::{Color, Style};

    use super::{TabBar, TabBarComponent, TabBarOutcome, TabBarPolicy};
    use crate::tab_bar::{TabBarKeyboardPolicy, TabBarState, TabItem};

    #[test]
    fn left_clipped_disabled_tab_cannot_be_selected() {
        let items = [
            TabItem::new("one", "One"),
            TabItem::new("two", "Two").disabled(true),
        ];
        let state = std::cell::RefCell::new(TabBarState::new(Some(0)));
        let component = TabBarComponent {
            id: "tabs".into(),
            bar: TabBar::new(&items),
            state: &state,
        };
        let layout = component.layout(Constraints::for_width(11), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 1));
        let mut frame = Frame::new(&mut buffer);
        PaintCx::new(&mut frame).with_child(-6, 0, LocalRect::new(0, 0, 11, 1), |cx| {
            component.paint(&layout, cx);
        });
        assert_eq!(frame.hits().regions().len(), 2);
        assert!(!frame.hits().regions()[1].enabled);
        EventCx::new(&layout).with_transform(0, 0, -6, 0, Rect::new(0, 0, 5, 1), |cx| {
            for kind in [
                MouseEventKind::Move,
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
            ] {
                assert_eq!(
                    component.event(
                        &Event::Mouse(MouseEvent::new(kind, Point::new(1, 0))),
                        &layout,
                        cx
                    ),
                    EventOutcome::Ignored
                );
            }
        });
        assert_eq!(state.borrow().selected, Some(0));
        assert_eq!(state.borrow().pressed, None);
        assert_eq!(state.borrow().hovered, None);
    }

    #[test]
    fn left_clipped_tabs_keep_original_mouse_boundaries() {
        let items = [TabItem::new("one", "One"), TabItem::new("two", "Two")];
        let state = std::cell::RefCell::new(TabBarState::new(Some(0)));
        let component = TabBarComponent {
            id: "tabs".into(),
            bar: TabBar::new(&items),
            state: &state,
        };
        let layout = component.layout(Constraints::for_width(11), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 1));
        let mut frame = Frame::new(&mut buffer);
        PaintCx::new(&mut frame).with_child(-6, 0, LocalRect::new(0, 0, 11, 1), |cx| {
            component.paint(&layout, cx);
        });
        let regions = frame.hits().regions();
        assert_eq!(regions.len(), 2, "only the bar and second tab are visible");
        assert_eq!(regions[1].area, Rect::new(0, 0, 5, 1));
        assert!(regions[1].area.contains(Point::new(1, 0)));
        EventCx::new(&layout).with_transform(0, 0, -6, 0, Rect::new(0, 0, 5, 1), |cx| {
            for kind in [
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
            ] {
                component.event(
                    &Event::Mouse(MouseEvent::new(kind, Point::new(1, 0))),
                    &layout,
                    cx,
                );
            }
        });
        assert_eq!(state.borrow().selected, Some(1));
    }

    #[test]
    fn clipped_tab_row_cannot_navigate_through_blank_layout_rows() {
        let items = [TabItem::new("one", "One"), TabItem::new("two", "Two")];
        let state = std::cell::RefCell::new(TabBarState::new(Some(0)));
        let component = TabBarComponent {
            id: "tabs".into(),
            bar: TabBar::new(&items),
            state: &state,
        };
        let layout = component.layout(Constraints::new(10, 10, 3, Some(3)), &mut LayoutCx::new());
        let right = Event::Key(KeyStroke::simple(KeyCode::Right));
        EventCx::new(&layout).with_transform(0, 0, 0, 0, Rect::new(0, 1, 10, 2), |cx| {
            assert!(
                cx.find_rect(&layout.id)
                    .is_some_and(|rect| !rect.is_empty())
            );
            assert_eq!(component.event(&right, &layout, cx), EventOutcome::Ignored);
        });
        assert_eq!(state.borrow().selected, Some(0));
        assert_eq!(
            component.event(&right, &layout, &mut EventCx::new(&layout)),
            EventOutcome::Redraw
        );
        assert_eq!(state.borrow().selected, Some(1));
    }

    #[test]
    fn hidden_tabs_do_not_navigate_but_cancel_disabled_pointer() {
        let items = [TabItem::new("one", "One"), TabItem::new("two", "Two")];
        for (width, height) in [(0, 1), (10, 0), (0, 0)] {
            let state = std::cell::RefCell::new(TabBarState::new(Some(0)));
            let mut component = TabBarComponent {
                id: "tabs".into(),
                bar: TabBar::new(&items),
                state: &state,
            };
            let layout = component.layout(
                Constraints::new(width, width, height, Some(height)),
                &mut LayoutCx::new(),
            );
            let right = Event::Key(KeyStroke::simple(KeyCode::Right));
            assert_eq!(
                component.event(&right, &layout, &mut EventCx::new(&layout)),
                EventOutcome::Ignored
            );
            assert_eq!(state.borrow().selected, Some(0));
            state.borrow_mut().pressed = Some(1);
            component.bar.policy.mouse.enabled = false;
            assert_eq!(
                component.event(&right, &layout, &mut EventCx::new(&layout)),
                EventOutcome::Redraw
            );
            assert_eq!(state.borrow().pressed, None);
            assert_eq!(state.borrow().selected, Some(0));
        }
    }

    #[test]
    fn disabling_tab_pointer_policy_cancels_stale_state() {
        let items = [TabItem::new("one", "One")];
        let mut bar = TabBar::new(&items);
        let mut state = TabBarState::new(None);
        state.pressed = Some(0);
        state.hovered = Some(0);
        let area = Rect::new(0, 0, 10, 1);
        bar.policy.mouse.click = false;
        bar.policy.mouse.hover = false;
        assert_eq!(
            bar.handle_event(area, &mut state, &Event::Tick),
            TabBarOutcome::Redraw
        );
        assert_eq!(state.pressed, None);
        assert_eq!(state.hovered, None);
        assert_eq!(
            bar.handle_event(area, &mut state, &Event::Tick),
            TabBarOutcome::Ignored
        );
        bar.policy.mouse.click = true;
        bar.policy.mouse.hover = true;
        bar.handle_event(
            area,
            &mut state,
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Up(MouseButton::Left),
                Point::new(1, 0),
            )),
        );
        assert_eq!(state.selected, None);
    }

    trait TabBarTestRender {
        fn render(&self, area: Rect, state: &TabBarState, frame: &mut Frame<'_>);
    }

    impl TabBarTestRender for TabBar<'_> {
        fn render(&self, area: Rect, state: &TabBarState, frame: &mut Frame<'_>) {
            let state = std::cell::RefCell::new(state.clone());
            let component = TabBarComponent {
                id: "test.tabs".into(),
                bar: *self,
                state: &state,
            };
            let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
            PaintCx::new(frame).with_child(
                i32::from(area.x),
                i64::from(area.y),
                LocalRect::new(0, 0, area.width, area.height),
                |cx| component.paint(&layout, cx),
            );
        }
    }

    #[test]
    fn disabling_click_before_release_cancels_tab_selection() {
        let items = [TabItem::new("one", "One"), TabItem::new("two", "Two")];
        let mut bar = TabBar::new(&items);
        let mut state = TabBarState::new(Some(1));
        let area = Rect::new(0, 0, 20, 1);
        let mouse = |kind| MouseEvent::new(kind, Point::new(1, 0));
        assert_eq!(
            bar.handle_mouse(
                area,
                &mut state,
                mouse(MouseEventKind::Down(MouseButton::Left))
            ),
            TabBarOutcome::Redraw
        );
        assert_eq!(state.pressed, Some(0));
        bar.policy.mouse.click = false;
        assert_eq!(
            bar.handle_mouse(
                area,
                &mut state,
                mouse(MouseEventKind::Up(MouseButton::Left))
            ),
            TabBarOutcome::Redraw
        );
        assert_eq!(state.pressed, None);
        assert_eq!(state.selected, Some(1));
        bar.policy.mouse.click = true;
        assert_eq!(
            bar.handle_mouse(
                area,
                &mut state,
                mouse(MouseEventKind::Up(MouseButton::Left))
            ),
            TabBarOutcome::Ignored
        );
        assert_eq!(state.selected, Some(1));
    }

    #[test]
    fn focus_loss_cancels_tab_pointer_selection() {
        let items = [TabItem::new("one", "One"), TabItem::new("two", "Two")];
        let bar = TabBar::new(&items);
        let mut state = TabBarState::new(Some(1));
        let area = Rect::new(0, 0, 20, 1);
        let mouse = |kind| Event::Mouse(MouseEvent::new(kind, Point::new(1, 0)));
        bar.handle_event(area, &mut state, &mouse(MouseEventKind::Move));
        bar.handle_event(
            area,
            &mut state,
            &mouse(MouseEventKind::Down(MouseButton::Left)),
        );
        assert_eq!(state.pressed, Some(0));
        assert_eq!(state.hovered, Some(0));
        let focus_event = Event::Focus(bmux_tui::event::FocusEvent::Lost);
        assert_eq!(
            bar.handle_event(area, &mut state, &focus_event),
            TabBarOutcome::Redraw
        );
        assert_eq!(state.pressed, None);
        assert_eq!(state.hovered, None);
        assert_eq!(
            bar.handle_event(area, &mut state, &focus_event),
            TabBarOutcome::Ignored
        );
        assert_eq!(
            bar.handle_event(
                area,
                &mut state,
                &mouse(MouseEventKind::Up(MouseButton::Left))
            ),
            TabBarOutcome::Ignored
        );
        assert_eq!(state.selected, Some(1));
    }

    #[test]
    fn blank_press_cancels_previous_tab_press() {
        let items = [TabItem::new("one", "One")];
        let bar = TabBar::new(&items);
        let mut state = TabBarState::new(None);
        let area = Rect::new(0, 0, 20, 1);
        let mouse = |kind, x| Event::Mouse(MouseEvent::new(kind, Point::new(x, 0)));
        bar.handle_event(
            area,
            &mut state,
            &mouse(MouseEventKind::Down(MouseButton::Left), 1),
        );
        assert_eq!(state.pressed, Some(0));
        assert_eq!(
            bar.handle_event(
                area,
                &mut state,
                &mouse(MouseEventKind::Down(MouseButton::Left), 19)
            ),
            TabBarOutcome::Redraw
        );
        assert_eq!(state.pressed, None);
        assert_eq!(
            bar.handle_event(
                area,
                &mut state,
                &mouse(MouseEventKind::Up(MouseButton::Left), 1)
            ),
            TabBarOutcome::Ignored
        );
        assert_eq!(state.selected, None);
    }

    #[test]
    fn disabled_tab_clears_previous_hover() {
        let items = [
            TabItem::new("one", "One"),
            TabItem::new("two", "Two").disabled(true),
        ];
        let bar = TabBar::new(&items);
        let mut state = TabBarState::new(Some(0));
        let area = Rect::new(0, 0, 20, 1);
        let regions = bar.hit_rects(area);
        let mouse = |index: usize| {
            Event::Mouse(MouseEvent::new(
                MouseEventKind::Move,
                Point::new(regions[index].x, 0),
            ))
        };
        assert_eq!(
            bar.handle_event(area, &mut state, &mouse(0)),
            TabBarOutcome::Redraw
        );
        assert_eq!(state.hovered, Some(0));
        assert_eq!(
            bar.handle_event(area, &mut state, &mouse(1)),
            TabBarOutcome::Redraw
        );
        assert_eq!(state.hovered, None);
        assert_eq!(
            bar.handle_event(area, &mut state, &mouse(1)),
            TabBarOutcome::Ignored
        );
        assert_eq!(state.selected, Some(0));
    }

    #[test]
    fn resize_cancels_pending_tab_press() {
        let items = [TabItem::new("one", "One")];
        let bar = TabBar::new(&items);
        let mut state = TabBarState::new(None);
        let area = Rect::new(0, 0, 20, 1);
        let mouse = |kind| Event::Mouse(MouseEvent::new(kind, Point::new(1, 0)));
        bar.handle_event(area, &mut state, &mouse(MouseEventKind::Move));
        bar.handle_event(
            area,
            &mut state,
            &mouse(MouseEventKind::Down(MouseButton::Left)),
        );
        assert_eq!(state.pressed, Some(0));
        let resize = Event::Resize(area.size());
        assert_eq!(
            bar.handle_event(area, &mut state, &resize),
            TabBarOutcome::Redraw
        );
        assert_eq!(state.pressed, None);
        assert_eq!(state.hovered, None);
        assert_eq!(
            bar.handle_event(area, &mut state, &resize),
            TabBarOutcome::Ignored
        );
        assert_eq!(
            bar.handle_event(
                area,
                &mut state,
                &mouse(MouseEventKind::Up(MouseButton::Left))
            ),
            TabBarOutcome::Ignored
        );
        assert_eq!(state.selected, None);
    }

    #[test]
    fn component_measures_paints_and_registers_tabs() {
        let items = [TabItem::new("one", "One"), TabItem::new("two", "Two")];
        let state = std::cell::RefCell::new(TabBarState::new(Some(1)));
        let component = TabBarComponent::new("tabs", &items, &state);
        let layout = component.layout(Constraints::for_width(11), &mut LayoutCx::new());
        assert_eq!(layout.size, LogicalSize::new(11, 1));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 11, 1));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some(" One   Two ")
        );
        assert_eq!(frame.hits().regions().len(), 3);
        assert_eq!(frame.semantics().regions().len(), 1);
    }

    #[test]
    fn component_routes_selection_through_authoritative_layout() {
        let items = [TabItem::new("one", "One"), TabItem::new("two", "Two")];
        let state = std::cell::RefCell::new(TabBarState::new(Some(0)));
        let component = TabBarComponent::new("tabs", &items, &state);
        let layout = LayoutNode::leaf(LayoutId::new("tabs"), LogicalSize::new(11, 1));
        let mut event_cx = EventCx::new(&layout);
        let outcome = component.event(
            &Event::Key(KeyStroke::simple(KeyCode::Right)),
            &layout,
            &mut event_cx,
        );
        assert_eq!(outcome, EventOutcome::Redraw);
        assert_eq!(state.borrow().selected(), Some(1));
    }

    #[test]
    fn mouse_disabled_component_retains_keyboard_target() {
        let items = [TabItem::new("one", "One")];
        let state = std::cell::RefCell::new(TabBarState::new(Some(0)));
        let mut policy = TabBarPolicy::default();
        policy.mouse.enabled = false;
        let component = TabBarComponent::new("tabs", &items, &state).policy(policy);
        let layout = component.layout(Constraints::for_width(5), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 1));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        let regions = frame.hits().regions();
        assert_eq!(regions.len(), 2);
        assert!(regions[0].focusable);
        for region in regions {
            assert!(region.enabled);
            assert!(!region.pointer_events);
            assert!(!region.hoverable);
        }
    }

    #[test]
    fn component_disabled_tab_is_not_enabled_in_scene() {
        let items = [
            TabItem::new("one", "One"),
            TabItem::new("two", "Two").disabled(true),
        ];
        let state = std::cell::RefCell::new(TabBarState::new(Some(0)));
        let component = TabBarComponent::new("tabs", &items, &state);
        let layout = component.layout(Constraints::for_width(11), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 11, 1));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        assert!(frame.hits().regions()[1].enabled);
        assert!(!frame.hits().regions()[2].enabled);
    }

    #[test]
    fn disabling_tab_cancels_pointer_state_and_requests_redraw() {
        let mut items = [TabItem::new("one", "One")];
        let mut state = TabBarState::new(None);
        let area = Rect::new(0, 0, 5, 1);
        let mouse = |kind| Event::Mouse(MouseEvent::new(kind, Point::new(1, 0)));
        TabBar::new(&items).handle_event(
            area,
            &mut state,
            &mouse(MouseEventKind::Down(MouseButton::Left)),
        );
        assert_eq!(state.pressed, Some(0));
        items[0].disabled = true;
        assert_eq!(
            TabBar::new(&items).handle_event(area, &mut state, &Event::Tick),
            TabBarOutcome::Redraw
        );
        assert_eq!(state.pressed, None);
        assert_eq!(state.hovered, None);
        assert_eq!(
            TabBar::new(&items).handle_event(area, &mut state, &Event::Tick),
            TabBarOutcome::Ignored
        );
        items[0].disabled = false;
        assert_eq!(
            TabBar::new(&items).handle_event(
                area,
                &mut state,
                &mouse(MouseEventKind::Up(MouseButton::Left))
            ),
            TabBarOutcome::Ignored
        );
        assert_eq!(state.selected(), None);
    }

    #[test]
    fn disabling_pointer_input_cancels_pending_tab_press() {
        let items = [TabItem::new("one", "One")];
        let area = Rect::new(0, 0, 5, 1);
        for disable_control in [false, true] {
            let mut bar = TabBar::new(&items);
            let mut state = TabBarState::new(None);
            let mouse = |kind| Event::Mouse(MouseEvent::new(kind, Point::new(1, 0)));
            bar.handle_event(
                area,
                &mut state,
                &mouse(MouseEventKind::Down(MouseButton::Left)),
            );
            assert_eq!(state.pressed, Some(0));
            if disable_control {
                state.interaction.disabled = true;
            } else {
                bar.policy.mouse.enabled = false;
            }
            assert_eq!(
                bar.handle_event(area, &mut state, &Event::Tick),
                TabBarOutcome::Redraw
            );
            assert_eq!(state.pressed, None);
            assert_eq!(state.hovered, None);
            assert_eq!(
                bar.handle_event(area, &mut state, &Event::Tick),
                TabBarOutcome::Ignored
            );
            state.interaction.disabled = false;
            bar.policy.mouse.enabled = true;
            assert_eq!(
                bar.handle_event(
                    area,
                    &mut state,
                    &mouse(MouseEventKind::Up(MouseButton::Left))
                ),
                TabBarOutcome::Ignored
            );
            assert_eq!(state.selected(), None);
        }
    }

    #[test]
    fn renders_selected_tab() {
        let items = [TabItem::new("one", "One"), TabItem::new("two", "Two")];
        let state = TabBarState::new(Some(1));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 11, 1));
        let mut frame = Frame::new(&mut buffer);

        TabBar::new(&items).render(Rect::new(0, 0, 11, 1), &state, &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some(" One   Two ")
        );
    }

    #[test]
    fn renders_rich_tab_label_preserving_span_style() {
        let accent = Style::new().fg(Color::Yellow);
        let items = [TabItem::rich(
            "one",
            Line::from_spans([Span::raw("O"), Span::styled("ne", accent)]),
        )];
        let state = TabBarState::new(Some(0));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 1));
        let mut frame = Frame::new(&mut buffer);

        TabBar::new(&items).render(Rect::new(0, 0, 5, 1), &state, &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some(" One "));
        assert_eq!(
            frame
                .buffer()
                .get(Point::new(2, 0))
                .map(|cell| cell.style.fg),
            Some(Some(Color::Yellow))
        );
    }

    #[test]
    fn custom_separator_and_padding_are_rendered_and_hit_tested() {
        let items = [TabItem::new("one", "One"), TabItem::new("two", "Two")];
        let state = TabBarState::new(Some(0));
        let policy = TabBarPolicy::interactive().separator("|").padding("[", "]");
        let mut buffer = Buffer::empty(Rect::new(0, 0, 11, 1));
        let mut frame = Frame::new(&mut buffer);

        let bar = TabBar::new(&items).policy(policy);
        bar.render(Rect::new(0, 0, 11, 1), &state, &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("[One]|[Two]")
        );
        assert_eq!(bar.hit_rects(Rect::new(0, 0, 11, 1))[1].x, 6);
    }

    #[test]
    fn directly_dispatched_tab_key_navigates_without_visual_focus() {
        let items = [TabItem::new("one", "One"), TabItem::new("two", "Two")];
        let mut state = TabBarState::new(Some(0));

        let outcome = TabBar::new(&items).handle_event(
            Rect::new(0, 0, 20, 1),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Right)),
        );

        assert_eq!(outcome, TabBarOutcome::Selected(1));
        assert_eq!(state.selected(), Some(1));
    }

    #[test]
    fn keyboard_navigation_selects_next_enabled_tab() {
        let items = [
            TabItem::new("one", "One"),
            TabItem::new("two", "Two").disabled(true),
            TabItem::new("three", "Three"),
        ];
        let mut state = TabBarState::new(Some(0));
        state.set_focused(true);

        let outcome = TabBar::new(&items).handle_event(
            Rect::new(0, 0, 20, 1),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Right)),
        );

        assert_eq!(outcome, TabBarOutcome::Selected(2));
        assert_eq!(state.selected(), Some(2));
    }

    #[test]
    fn keyboard_navigation_can_disable_wrapping() {
        let items = [TabItem::new("one", "One"), TabItem::new("two", "Two")];
        let mut state = TabBarState::new(Some(0));
        state.set_focused(true);
        let bar = TabBar::new(&items).policy(TabBarPolicy {
            keyboard: TabBarKeyboardPolicy {
                enabled: true,
                wrap: false,
                home_end: true,
            },
            ..TabBarPolicy::interactive().padding("[", "]")
        });

        let outcome = bar.handle_event(
            Rect::new(0, 0, 20, 1),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Left)),
        );

        assert_eq!(outcome, TabBarOutcome::Ignored);
        assert_eq!(state.selected(), Some(0));
    }

    #[test]
    fn home_and_end_select_endpoints() {
        let items = [TabItem::new("one", "One"), TabItem::new("two", "Two")];
        let mut state = TabBarState::new(Some(0));
        state.set_focused(true);
        let bar = TabBar::new(&items);

        assert_eq!(
            bar.handle_event(
                Rect::new(0, 0, 20, 1),
                &mut state,
                &Event::Key(KeyStroke::simple(KeyCode::End)),
            ),
            TabBarOutcome::Selected(1)
        );
        assert_eq!(
            bar.handle_event(
                Rect::new(0, 0, 20, 1),
                &mut state,
                &Event::Key(KeyStroke::simple(KeyCode::Home)),
            ),
            TabBarOutcome::Selected(0)
        );
    }

    #[test]
    fn mouse_click_selects_tab() {
        let items = [TabItem::new("one", "One"), TabItem::new("two", "Two")];
        let mut state = TabBarState::new(Some(0));
        state.set_focused(true);
        let bar = TabBar::new(&items);
        let area = Rect::new(0, 0, 20, 1);

        assert_eq!(
            bar.handle_event(
                area,
                &mut state,
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Down(MouseButton::Left),
                    Point::new(7, 0),
                )),
            ),
            TabBarOutcome::Redraw
        );
        assert_eq!(
            bar.handle_event(
                area,
                &mut state,
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Up(MouseButton::Left),
                    Point::new(7, 0),
                )),
            ),
            TabBarOutcome::Selected(1)
        );
        assert_eq!(state.selected(), Some(1));
    }

    #[test]
    fn disabled_tab_cannot_be_mouse_selected() {
        let items = [
            TabItem::new("one", "One"),
            TabItem::new("two", "Two").disabled(true),
        ];
        let mut state = TabBarState::new(Some(0));
        state.set_focused(true);
        let bar = TabBar::new(&items);

        assert_eq!(
            bar.handle_event(
                Rect::new(0, 0, 20, 1),
                &mut state,
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Down(MouseButton::Left),
                    Point::new(7, 0),
                )),
            ),
            TabBarOutcome::Ignored
        );
        assert_eq!(state.selected(), Some(0));
    }

    #[test]
    fn render_truncates_overflow() {
        let items = [
            TabItem::new("one", "LongOne"),
            TabItem::new("two", "LongTwo"),
        ];
        let state = TabBarState::new(Some(0));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        let mut frame = Frame::new(&mut buffer);

        TabBar::new(&items).render(Rect::new(0, 0, 8, 1), &state, &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some(" LongOn…"));
    }

    #[test]
    fn bare_policy_ignores_events() {
        let items = [TabItem::new("one", "One"), TabItem::new("two", "Two")];
        let mut state = TabBarState::new(Some(0));
        state.set_focused(true);
        let bar = TabBar::new(&items).policy(TabBarPolicy::bare());

        let outcome = bar.handle_event(
            Rect::new(0, 0, 20, 1),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Right)),
        );

        assert_eq!(outcome, TabBarOutcome::Ignored);
        assert_eq!(state.selected(), Some(0));
    }

    #[test]
    fn tiny_area_does_not_panic() {
        let items = [TabItem::new("one", "One")];
        let state = TabBarState::new(Some(0));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 0, 0));
        let mut frame = Frame::new(&mut buffer);

        TabBar::new(&items).render(Rect::new(0, 0, 0, 0), &state, &mut frame);

        assert!(TabBar::new(&items).hit_rects(Rect::new(0, 0, 0, 0))[0].is_empty());
    }
}
