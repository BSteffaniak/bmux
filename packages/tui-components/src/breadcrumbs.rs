//! Generic breadcrumbs / path trail component.

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
use bmux_tui::prelude::{Line, Span};
use bmux_tui::semantic::SemanticRegion;
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui::text_width::display_width;

use crate::common::u16_saturating;

use crate::common::ComponentMousePolicy;

/// One breadcrumb item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BreadcrumbItem<'a> {
    /// Stable item id.
    pub id: &'a str,
    /// Display label.
    pub label: &'a str,
    /// Disabled items render but cannot be activated.
    pub disabled: bool,
}

impl<'a> BreadcrumbItem<'a> {
    /// Create a breadcrumb item.
    #[must_use]
    pub const fn new(id: &'a str, label: &'a str) -> Self {
        Self {
            id,
            label,
            disabled: false,
        }
    }

    /// Return this item with disabled state set.
    #[must_use]
    pub const fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

/// Runtime breadcrumbs state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct BreadcrumbsState {
    current: Option<usize>,
    hovered: Option<usize>,
    pressed: Option<usize>,
    focused: bool,
}

impl BreadcrumbsState {
    /// Create breadcrumbs state.
    #[must_use]
    pub const fn new(current: Option<usize>) -> Self {
        Self {
            current,
            hovered: None,
            pressed: None,
            focused: false,
        }
    }

    /// Set whether this composite currently owns keyboard focus.
    pub const fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    /// Current item index.
    #[must_use]
    pub const fn current(&self) -> Option<usize> {
        self.current
    }

    /// Set current item index.
    pub const fn set_current(&mut self, current: Option<usize>) {
        self.current = current;
    }

    /// Hovered item index.
    #[must_use]
    pub const fn hovered(&self) -> Option<usize> {
        self.hovered
    }
}

/// Breadcrumb behavior policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BreadcrumbsPolicy {
    /// Separator between items.
    pub separator: &'static str,
    /// Keyboard activation enabled.
    pub keyboard: bool,
    /// Mouse behavior.
    pub mouse: ComponentMousePolicy,
    /// Truncate to available width.
    pub truncate: bool,
}

impl BreadcrumbsPolicy {
    /// Render-only breadcrumbs.
    #[must_use]
    pub const fn bare() -> Self {
        Self {
            separator: " / ",
            keyboard: false,
            mouse: ComponentMousePolicy::disabled(),
            truncate: true,
        }
    }

    /// Interactive breadcrumbs.
    #[must_use]
    pub const fn interactive() -> Self {
        Self {
            separator: " / ",
            keyboard: true,
            mouse: ComponentMousePolicy::button(),
            truncate: true,
        }
    }
}

impl Default for BreadcrumbsPolicy {
    fn default() -> Self {
        Self::interactive()
    }
}

/// Breadcrumb styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BreadcrumbsStyles {
    /// Normal item style.
    pub normal: Style,
    /// Current item style.
    pub current: Style,
    /// Hovered item style.
    pub hovered: Style,
    /// Pressed item style.
    pub pressed: Style,
    /// Disabled item style.
    pub disabled: Style,
    /// Separator style.
    pub separator: Style,
}

impl Default for BreadcrumbsStyles {
    fn default() -> Self {
        Self {
            normal: Style::new().fg(Color::White),
            current: Style::new()
                .fg(Color::BrightCyan)
                .add_modifier(Modifier::BOLD),
            hovered: Style::new().fg(Color::BrightWhite),
            pressed: Style::new().fg(Color::Black).bg(Color::Cyan),
            disabled: Style::new().fg(Color::BrightBlack),
            separator: Style::new().fg(Color::BrightBlack),
        }
    }
}

/// Breadcrumb outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreadcrumbsOutcome<'a> {
    /// Event ignored.
    Ignored,
    /// Visual state changed.
    Redraw,
    /// Item activated.
    Activated { index: usize, id: &'a str },
}

/// Generic breadcrumbs component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Breadcrumbs<'a> {
    items: &'a [BreadcrumbItem<'a>],
    policy: BreadcrumbsPolicy,
    styles: BreadcrumbsStyles,
}

/// Canonical component-lifecycle breadcrumbs control.
pub struct BreadcrumbsComponent<'a, 'state> {
    id: LayoutId,
    breadcrumbs: Breadcrumbs<'a>,
    state: &'state Cell<BreadcrumbsState>,
}

impl<'a, 'state> BreadcrumbsComponent<'a, 'state> {
    /// Create breadcrumbs with stable identity and caller-owned state.
    #[must_use]
    pub fn new(
        id: impl Into<LayoutId>,
        items: &'a [BreadcrumbItem<'a>],
        state: &'state Cell<BreadcrumbsState>,
    ) -> Self {
        Self {
            id: id.into(),
            breadcrumbs: Breadcrumbs::new(items),
            state,
        }
    }

    /// Handle input using resolved component geometry, preserving activation details.
    pub fn handle_event(
        &self,
        event: &Event,
        layout: &LayoutNode,
        cx: &mut EventCx<'_>,
    ) -> BreadcrumbsOutcome<'a> {
        let Some(area) = cx.find_rect(&layout.id) else {
            return BreadcrumbsOutcome::Ignored;
        };
        if area.is_empty() && matches!(event, Event::Key(_)) {
            return BreadcrumbsOutcome::Ignored;
        }
        let mut state = self.state.get();
        let pointer_changed = self.breadcrumbs.cancel_invalid_pointer_targets(&mut state);
        let outcome = if let Event::Mouse(mouse) = event {
            if self.breadcrumbs.policy.mouse.enabled {
                let interactive_width = if self.breadcrumbs.policy.truncate
                    && self.breadcrumbs.intrinsic_width() > usize::from(layout.size.width)
                {
                    u16_saturating(
                        self.breadcrumbs
                            .line(&state)
                            .truncate(usize::from(layout.size.width))
                            .width()
                            .saturating_sub(1),
                    )
                } else {
                    layout.size.width
                };
                let mut x = 0_u16;
                let hit =
                    self.breadcrumbs
                        .items
                        .iter()
                        .enumerate()
                        .find_map(|(index, item)| {
                            let width = u16_saturating(display_width(item.label));
                            let visible = cx.visible_rect(bmux_tui::component::LogicalRect::new(
                                x.into(),
                                0,
                                usize::from(width.min(interactive_width.saturating_sub(x))),
                                usize::from(layout.size.height > 0),
                            ));
                            x = x.saturating_add(width).saturating_add(u16_saturating(
                                display_width(self.breadcrumbs.policy.separator),
                            ));
                            (!item.disabled && visible.contains(mouse.position)).then_some(index)
                        });
                self.breadcrumbs.handle_mouse_hit(&mut state, *mouse, hit)
            } else {
                BreadcrumbsOutcome::Ignored
            }
        } else {
            self.breadcrumbs.handle_event(area, &mut state, event)
        };
        self.state.set(state);
        if pointer_changed && outcome == BreadcrumbsOutcome::Ignored {
            BreadcrumbsOutcome::Redraw
        } else {
            outcome
        }
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: BreadcrumbsPolicy) -> Self {
        self.breadcrumbs.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: BreadcrumbsStyles) -> Self {
        self.breadcrumbs.styles = styles;
        self
    }
}

impl Component for BreadcrumbsComponent<'_, '_> {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        self.breadcrumbs.items.len().hash(&mut layout);
        for item in self.breadcrumbs.items {
            item.id.hash(&mut layout);
            item.label.hash(&mut layout);
        }
        self.breadcrumbs.policy.separator.hash(&mut layout);

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        for item in self.breadcrumbs.items {
            item.disabled.hash(&mut paint);
        }
        let policy = self.breadcrumbs.policy;
        (
            policy.separator,
            policy.keyboard,
            policy.truncate,
            policy.mouse.enabled,
            policy.mouse.hover,
            policy.mouse.click,
        )
            .hash(&mut paint);
        self.breadcrumbs.styles.hash(&mut paint);
        self.state.get().hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let width = u16_saturating(self.breadcrumbs.intrinsic_width());
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(width, 1)),
        )
        .with_metadata(LayoutMetadata::new().semantic("breadcrumbs"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        if layout.size.width == 0 || layout.size.height == 0 {
            return;
        }
        let state = self.state.get();
        let mut line = self.breadcrumbs.line(&state);
        if self.breadcrumbs.policy.truncate && line.width() > usize::from(layout.size.width) {
            line = line.truncate(usize::from(layout.size.width));
        }
        let area = LocalRect::new(0, 0, layout.size.width, 1);
        cx.write_line(area, &line);
        let interactive = self.breadcrumbs.policy.keyboard || self.breadcrumbs.policy.mouse.enabled;
        if interactive && self.breadcrumbs.items.iter().any(|item| !item.disabled) {
            cx.push_hit(
                SceneRegion::new(self.id.as_str(), Rect::new(0, 0, layout.size.width, 1))
                    .role(HitRole::ListItem)
                    .pointer_events(self.breadcrumbs.policy.mouse.enabled)
                    .hoverable(
                        self.breadcrumbs.policy.mouse.enabled
                            && self.breadcrumbs.policy.mouse.hover,
                    )
                    .focusable(self.breadcrumbs.policy.keyboard),
            );
        }
        cx.push_semantic(SemanticRegion::new(
            self.id.as_str(),
            Rect::new(0, 0, layout.size.width, 1),
            "breadcrumbs",
        ));
        cx.push_damage(area);
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        match self.handle_event(event, layout, cx) {
            BreadcrumbsOutcome::Ignored => EventOutcome::Ignored,
            BreadcrumbsOutcome::Redraw | BreadcrumbsOutcome::Activated { .. } => {
                EventOutcome::Redraw
            }
        }
    }
}

impl<'a> Breadcrumbs<'a> {
    fn intrinsic_width(&self) -> usize {
        let separator_width = display_width(self.policy.separator);
        self.items
            .iter()
            .enumerate()
            .fold(0_usize, |width, (index, item)| {
                width
                    .saturating_add(display_width(item.label))
                    .saturating_add(if index == 0 { 0 } else { separator_width })
            })
    }

    /// Create breadcrumbs over caller-owned items.
    #[must_use]
    pub const fn new(items: &'a [BreadcrumbItem<'a>]) -> Self {
        Self {
            items,
            policy: BreadcrumbsPolicy {
                separator: " / ",
                keyboard: true,
                mouse: ComponentMousePolicy {
                    enabled: true,
                    hover: true,
                    click: true,
                },
                truncate: true,
            },
            styles: BreadcrumbsStyles {
                normal: Style::new(),
                current: Style::new(),
                hovered: Style::new(),
                pressed: Style::new(),
                disabled: Style::new(),
                separator: Style::new(),
            },
        }
    }

    /// Handle one event.
    pub fn handle_event(
        &self,
        area: Rect,
        state: &mut BreadcrumbsState,
        event: &Event,
    ) -> BreadcrumbsOutcome<'a> {
        let pointer_changed = self.cancel_invalid_pointer_targets(state);
        let outcome = match event {
            Event::Key(stroke) if self.policy.keyboard && stroke.modifiers.is_empty() => {
                match stroke.key {
                    KeyCode::Left => self.move_current(state, -1),
                    KeyCode::Right => self.move_current(state, 1),
                    KeyCode::Enter => state
                        .current
                        .and_then(|index| self.activate(index))
                        .unwrap_or(BreadcrumbsOutcome::Ignored),
                    _ => BreadcrumbsOutcome::Ignored,
                }
            }
            Event::Mouse(mouse) if self.policy.mouse.enabled => {
                self.handle_mouse(area, state, *mouse)
            }
            Event::Focus(bmux_tui::event::FocusEvent::Lost) | Event::Resize(_) => {
                let changed = state.pressed.take().is_some() | state.hovered.take().is_some();
                if changed {
                    BreadcrumbsOutcome::Redraw
                } else {
                    BreadcrumbsOutcome::Ignored
                }
            }
            Event::Key(_)
            | Event::Mouse(_)
            | Event::Paste(_)
            | Event::Focus(bmux_tui::event::FocusEvent::Gained)
            | Event::Tick
            | Event::User(_) => BreadcrumbsOutcome::Ignored,
        };
        if pointer_changed && outcome == BreadcrumbsOutcome::Ignored {
            BreadcrumbsOutcome::Redraw
        } else {
            outcome
        }
    }

    fn cancel_invalid_pointer_targets(&self, state: &mut BreadcrumbsState) -> bool {
        let previous = (state.pressed, state.hovered);
        let enabled = |index: usize| {
            self.policy.mouse.enabled && self.items.get(index).is_some_and(|item| !item.disabled)
        };
        state.pressed = state.pressed.filter(|&index| enabled(index));
        state.hovered = state.hovered.filter(|&index| enabled(index));
        previous != (state.pressed, state.hovered)
    }

    fn handle_mouse(
        &self,
        area: Rect,
        state: &mut BreadcrumbsState,
        mouse: MouseEvent,
    ) -> BreadcrumbsOutcome<'a> {
        self.handle_mouse_hit(state, mouse, self.item_at(area, mouse.position))
    }

    fn handle_mouse_hit(
        &self,
        state: &mut BreadcrumbsState,
        mouse: MouseEvent,
        hit: Option<usize>,
    ) -> BreadcrumbsOutcome<'a> {
        match mouse.kind {
            MouseEventKind::Move if self.policy.mouse.hover => {
                let hovered = hit;
                if hovered == state.hovered {
                    BreadcrumbsOutcome::Ignored
                } else {
                    state.hovered = hovered;
                    BreadcrumbsOutcome::Redraw
                }
            }
            MouseEventKind::Down(MouseButton::Left) if self.policy.mouse.click => {
                state.pressed = hit;
                BreadcrumbsOutcome::Redraw
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let released = hit;
                let pressed = state.pressed.take();
                if self.policy.mouse.click
                    && released == pressed
                    && let Some(index) = released
                {
                    return self.activate(index).unwrap_or(BreadcrumbsOutcome::Ignored);
                }
                if pressed.is_some() {
                    BreadcrumbsOutcome::Redraw
                } else {
                    BreadcrumbsOutcome::Ignored
                }
            }
            MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::Move
            | MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => BreadcrumbsOutcome::Ignored,
        }
    }

    fn move_current(&self, state: &mut BreadcrumbsState, delta: i32) -> BreadcrumbsOutcome<'a> {
        if self.items.is_empty() {
            return BreadcrumbsOutcome::Ignored;
        }
        let current = state.current.filter(|&index| index < self.items.len());
        let next = if delta.is_negative() {
            (0..current.unwrap_or(self.items.len()))
                .rev()
                .find(|&index| !self.items[index].disabled)
        } else {
            (current.map_or(0, |index| index + 1)..self.items.len())
                .find(|&index| !self.items[index].disabled)
        };
        if let Some(next) = next {
            state.current = Some(next);
            BreadcrumbsOutcome::Redraw
        } else {
            BreadcrumbsOutcome::Ignored
        }
    }

    fn activate(&self, index: usize) -> Option<BreadcrumbsOutcome<'a>> {
        self.items.get(index).and_then(|item| {
            (!item.disabled).then_some(BreadcrumbsOutcome::Activated { index, id: item.id })
        })
    }

    fn item_at(&self, area: Rect, position: Point) -> Option<usize> {
        if !area.contains(position) || position.y != area.y {
            return None;
        }
        let mut offset = usize::from(position.x - area.x);
        let separator_width = display_width(self.policy.separator);
        for (index, item) in self.items.iter().enumerate() {
            let width = display_width(item.label);
            if offset < width {
                return (!item.disabled).then_some(index);
            }
            offset -= width;
            if offset < separator_width {
                return None;
            }
            offset -= separator_width;
        }
        None
    }

    fn line(&self, state: &BreadcrumbsState) -> Line {
        let mut spans = Vec::new();
        for (index, item) in self.items.iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(self.policy.separator, self.styles.separator));
            }
            spans.push(Span::styled(
                item.label,
                self.item_style(index, item, state),
            ));
        }
        Line::from_spans(spans)
    }

    fn item_style(
        &self,
        index: usize,
        item: &BreadcrumbItem<'_>,
        state: &BreadcrumbsState,
    ) -> Style {
        if item.disabled {
            self.styles.disabled
        } else if state.pressed == Some(index) {
            self.styles.pressed
        } else if state.hovered == Some(index) {
            self.styles.hovered
        } else if state.current == Some(index) {
            self.styles.current
        } else {
            self.styles.normal
        }
    }
}

impl crate::theme::ComponentTheme {
    /// Convert this semantic component theme into [`BreadcrumbsStyles`].
    #[must_use]
    pub fn breadcrumbs_styles(self) -> BreadcrumbsStyles {
        BreadcrumbsStyles::from(self)
    }
}

impl From<crate::theme::ComponentTheme> for BreadcrumbsStyles {
    fn from(theme: crate::theme::ComponentTheme) -> Self {
        let theme = theme.for_surface(crate::theme::ComponentSurfaceDepth::Normal);
        Self {
            normal: theme.text,
            current: theme.info.add_modifier(bmux_tui::style::Modifier::BOLD),
            hovered: theme.info,
            pressed: theme.selected,
            disabled: theme.disabled,
            separator: theme.muted,
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
    use bmux_tui::paint::{LocalRect, PaintCx};

    use super::{
        BreadcrumbItem, Breadcrumbs, BreadcrumbsComponent, BreadcrumbsOutcome, BreadcrumbsPolicy,
        BreadcrumbsState,
    };

    trait BreadcrumbsTestRender {
        fn render(&self, area: Rect, state: &BreadcrumbsState, frame: &mut Frame<'_>);
        fn render_with_id(
            &self,
            id: &'static str,
            area: Rect,
            state: &BreadcrumbsState,
            frame: &mut Frame<'_>,
        );
    }

    impl BreadcrumbsTestRender for Breadcrumbs<'_> {
        fn render(&self, area: Rect, state: &BreadcrumbsState, frame: &mut Frame<'_>) {
            self.render_with_id("test.breadcrumbs", area, state, frame);
        }

        fn render_with_id(
            &self,
            id: &'static str,
            area: Rect,
            state: &BreadcrumbsState,
            frame: &mut Frame<'_>,
        ) {
            let state = Cell::new(*state);
            let component = BreadcrumbsComponent {
                id: id.into(),
                breadcrumbs: *self,
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
    fn keyboard_navigation_skips_disabled_items_without_wrapping() {
        let items = [
            BreadcrumbItem::new("home", "Home"),
            BreadcrumbItem::new("disabled", "Disabled").disabled(true),
            BreadcrumbItem::new("page", "Page"),
        ];
        let breadcrumbs = Breadcrumbs::new(&items);
        let mut state = BreadcrumbsState::new(Some(0));
        assert_eq!(
            breadcrumbs.move_current(&mut state, 1),
            BreadcrumbsOutcome::Redraw
        );
        assert_eq!(state.current(), Some(2));
        assert_eq!(
            breadcrumbs.move_current(&mut state, 1),
            BreadcrumbsOutcome::Ignored
        );
        assert_eq!(state.current(), Some(2));
        assert_eq!(
            breadcrumbs.move_current(&mut state, -1),
            BreadcrumbsOutcome::Redraw
        );
        assert_eq!(state.current(), Some(0));
        assert_eq!(
            breadcrumbs.move_current(&mut state, -1),
            BreadcrumbsOutcome::Ignored
        );
        assert_eq!(state.current(), Some(0));
    }

    #[test]
    fn navigation_enters_from_directional_edge_with_missing_or_stale_current() {
        let items = [
            BreadcrumbItem::new("start", "Start").disabled(true),
            BreadcrumbItem::new("home", "Home"),
            BreadcrumbItem::new("page", "Page"),
            BreadcrumbItem::new("end", "End").disabled(true),
        ];
        let breadcrumbs = Breadcrumbs::new(&items);
        for current in [None, Some(items.len()), Some(usize::MAX)] {
            for (direction, expected) in [(1, 1), (-1, 2)] {
                let mut state = BreadcrumbsState::new(current);
                assert_eq!(
                    breadcrumbs.move_current(&mut state, direction),
                    BreadcrumbsOutcome::Redraw
                );
                assert_eq!(state.current(), Some(expected));
            }
        }
        let disabled = [BreadcrumbItem::new("disabled", "Disabled").disabled(true)];
        for items in [&disabled[..], &[][..]] {
            for direction in [-1, 1] {
                let mut state = BreadcrumbsState::new(None);
                assert_eq!(
                    Breadcrumbs::new(items).move_current(&mut state, direction),
                    BreadcrumbsOutcome::Ignored
                );
                assert_eq!(state.current(), None);
            }
        }
    }

    #[test]
    fn focus_loss_cancels_pointer_activation_and_hover() {
        let items = [BreadcrumbItem::new("home", "Home")];
        let breadcrumbs = Breadcrumbs::new(&items);
        let area = Rect::new(0, 0, 10, 1);
        let mut state = BreadcrumbsState::new(Some(0));
        state.pressed = Some(0);
        state.hovered = Some(0);
        assert_eq!(
            breadcrumbs.handle_event(
                area,
                &mut state,
                &Event::Focus(bmux_tui::event::FocusEvent::Lost)
            ),
            BreadcrumbsOutcome::Redraw
        );
        assert_eq!(state.pressed, None);
        assert_eq!(state.hovered(), None);
        assert_eq!(state.current(), Some(0));
        assert_eq!(
            breadcrumbs.handle_event(
                area,
                &mut state,
                &Event::Focus(bmux_tui::event::FocusEvent::Lost)
            ),
            BreadcrumbsOutcome::Ignored
        );
        assert_eq!(
            breadcrumbs.handle_event(
                area,
                &mut state,
                &Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Up(MouseButton::Left),
                    position: Point::new(0, 0),
                    modifiers: bmux_tui::event::MouseModifiers::default(),
                })
            ),
            BreadcrumbsOutcome::Ignored
        );
    }

    #[test]
    fn disabling_click_before_release_cancels_breadcrumb_activation() {
        let items = [BreadcrumbItem::new("home", "Home")];
        let area = Rect::new(0, 0, 8, 1);
        let mut state = BreadcrumbsState::new(Some(0));
        let mouse = |kind| Event::Mouse(bmux_tui::event::MouseEvent::new(kind, Point::new(1, 0)));
        Breadcrumbs::new(&items).handle_event(
            area,
            &mut state,
            &mouse(MouseEventKind::Down(MouseButton::Left)),
        );
        assert_eq!(state.pressed, Some(0));
        let mut policy = super::BreadcrumbsPolicy::default();
        policy.mouse.click = false;
        let release = mouse(MouseEventKind::Up(MouseButton::Left));
        assert_eq!(
            Breadcrumbs {
                policy,
                ..Breadcrumbs::new(&items)
            }
            .handle_event(area, &mut state, &release),
            BreadcrumbsOutcome::Redraw
        );
        assert_eq!(state.pressed, None);
        assert!(!matches!(
            Breadcrumbs::new(&items).handle_event(area, &mut state, &release),
            BreadcrumbsOutcome::Activated { .. }
        ));
    }

    #[test]
    fn release_without_pending_press_is_ignored() {
        let items = [BreadcrumbItem::new("home", "Home")];
        let breadcrumbs = Breadcrumbs::new(&items);
        let mut state = BreadcrumbsState::new(Some(0));
        let initial = state;
        for position in [Point::new(1, 0), Point::new(20, 0)] {
            assert_eq!(
                breadcrumbs.handle_event(
                    Rect::new(0, 0, 8, 1),
                    &mut state,
                    &Event::Mouse(bmux_tui::event::MouseEvent::new(
                        MouseEventKind::Up(MouseButton::Left),
                        position
                    ))
                ),
                BreadcrumbsOutcome::Ignored
            );
            assert_eq!(state, initial);
        }
    }

    #[test]
    fn disabling_item_cancels_pending_press_and_requests_redraw() {
        let mut items = [BreadcrumbItem::new("home", "Home")];
        let area = Rect::new(0, 0, 10, 1);
        let mut state = BreadcrumbsState::new(None);
        let mouse = |kind| Event::Mouse(MouseEvent::new(kind, Point::new(0, 0)));
        Breadcrumbs::new(&items).handle_event(
            area,
            &mut state,
            &mouse(MouseEventKind::Down(MouseButton::Left)),
        );
        assert_eq!(state.pressed, Some(0));
        items[0].disabled = true;
        assert_eq!(
            Breadcrumbs::new(&items).handle_event(area, &mut state, &Event::Tick),
            BreadcrumbsOutcome::Redraw
        );
        assert_eq!(state.pressed, None);
        assert_eq!(state.hovered(), None);
        assert_eq!(
            Breadcrumbs::new(&items).handle_event(area, &mut state, &Event::Tick),
            BreadcrumbsOutcome::Ignored
        );
        items[0].disabled = false;
        assert_eq!(
            Breadcrumbs::new(&items).handle_event(
                area,
                &mut state,
                &mouse(MouseEventKind::Up(MouseButton::Left))
            ),
            BreadcrumbsOutcome::Ignored
        );
    }

    #[test]
    fn disabled_items_do_not_acquire_pointer_state() {
        let items = [BreadcrumbItem::new("disabled", "Disabled").disabled(true)];
        let breadcrumbs = Breadcrumbs::new(&items);
        let area = Rect::new(0, 0, 10, 1);
        let mut state = BreadcrumbsState::new(None);
        for kind in [
            MouseEventKind::Move,
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            let outcome = breadcrumbs.handle_event(
                area,
                &mut state,
                &Event::Mouse(MouseEvent {
                    kind,
                    position: Point::new(0, 0),
                    modifiers: bmux_tui::event::MouseModifiers::default(),
                }),
            );
            assert!(!matches!(outcome, BreadcrumbsOutcome::Activated { .. }));
            assert_eq!(state.hovered(), None);
            assert_eq!(state.pressed, None);
            assert_eq!(state.current(), None);
        }
    }

    #[test]
    fn pointer_scan_respects_wide_labels_separators_and_empty_items() {
        let items = [
            BreadcrumbItem::new("empty", ""),
            BreadcrumbItem::new("wide", "界"),
            BreadcrumbItem::new("last", "X"),
        ];
        let breadcrumbs = Breadcrumbs::new(&items);
        let area = Rect::new(10, 2, 10, 1);
        for (offset, expected) in [
            None,
            None,
            None,
            Some(1),
            Some(1),
            None,
            None,
            None,
            Some(2),
            None,
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(
                breadcrumbs.item_at(area, Point::new(10 + u16::try_from(offset).unwrap(), 2)),
                expected
            );
        }
    }

    #[test]
    fn resize_cancels_pending_pointer_activation() {
        let items = [BreadcrumbItem::new("home", "Home")];
        let breadcrumbs = Breadcrumbs::new(&items);
        let area = Rect::new(0, 0, 10, 1);
        let mut state = BreadcrumbsState::new(Some(0));
        for kind in [
            MouseEventKind::Move,
            MouseEventKind::Down(MouseButton::Left),
        ] {
            breadcrumbs.handle_event(
                area,
                &mut state,
                &Event::Mouse(MouseEvent::new(kind, Point::new(0, 0))),
            );
        }
        assert_eq!(
            breadcrumbs.handle_event(area, &mut state, &Event::Resize(Size::new(20, 2))),
            BreadcrumbsOutcome::Redraw
        );
        assert_eq!(state.hovered(), None);
        assert_eq!(state.pressed, None);
        assert_eq!(state.current(), Some(0));
        assert!(!matches!(
            breadcrumbs.handle_event(
                area,
                &mut state,
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Up(MouseButton::Left),
                    Point::new(0, 0)
                ))
            ),
            BreadcrumbsOutcome::Activated { .. }
        ));
    }

    #[test]
    fn exact_fit_trail_preserves_separator_and_final_item_activation() {
        let items = [
            BreadcrumbItem::new("home", "Home"),
            BreadcrumbItem::new("docs", "Docs"),
        ];
        let state = Cell::new(BreadcrumbsState::new(None));
        let component = BreadcrumbsComponent::new("trail", &items, &state);
        let layout = component.layout(Constraints::for_width(11), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 11, 1));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("Home / Docs")
        );
        for (x, expected) in [(5, None), (10, Some("docs"))] {
            let mut cx = bmux_tui::component::EventCx::new(&layout);
            component.handle_event(
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Down(MouseButton::Left),
                    Point::new(x, 0),
                )),
                &layout,
                &mut cx,
            );
            let outcome = component.handle_event(
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Up(MouseButton::Left),
                    Point::new(x, 0),
                )),
                &layout,
                &mut cx,
            );
            let activated = match outcome {
                BreadcrumbsOutcome::Activated { id, .. } => Some(id),
                _ => None,
            };
            assert_eq!(activated, expected);
        }
    }

    #[test]
    fn oversized_trail_truncates_at_saturated_layout_width() {
        let label = "a".repeat(usize::from(u16::MAX) + 1);
        let items = [BreadcrumbItem::new("long", &label)];
        let state = Cell::new(BreadcrumbsState::new(None));
        let component = BreadcrumbsComponent::new("trail", &items, &state);
        assert_eq!(component.breadcrumbs.intrinsic_width(), label.len());
        let layout = component.layout(Constraints::for_width(u16::MAX), &mut LayoutCx::new());
        assert_eq!(layout.size.width, u16::MAX);
        let mut buffer = Buffer::empty(Rect::new(0, 0, u16::MAX, 1));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        assert!(frame.buffer().row_symbols(0).unwrap().ends_with('…'));
        let mut cx = bmux_tui::component::EventCx::new(&layout);
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            let outcome = component.handle_event(
                &Event::Mouse(MouseEvent::new(kind, Point::new(u16::MAX - 1, 0))),
                &layout,
                &mut cx,
            );
            assert!(!matches!(outcome, BreadcrumbsOutcome::Activated { .. }));
        }
        assert_eq!(state.get().current(), None);
    }

    #[test]
    fn intrinsic_width_matches_styled_trail_width() {
        for labels in [vec![], vec!["界"], vec!["Home", "界", ""]] {
            let items: Vec<_> = labels
                .iter()
                .map(|label| BreadcrumbItem::new(label, label))
                .collect();
            for separator in ["", " / ", "界"] {
                let mut breadcrumbs = Breadcrumbs::new(&items);
                breadcrumbs.policy.separator = separator;
                assert_eq!(
                    breadcrumbs.intrinsic_width(),
                    breadcrumbs.line(&BreadcrumbsState::default()).width()
                );
            }
        }
    }

    #[test]
    fn untruncated_last_visible_cell_remains_clickable() {
        let items = [BreadcrumbItem::new("home", "Home")];
        let state = Cell::new(BreadcrumbsState::new(None));
        let component =
            BreadcrumbsComponent::new("trail", &items, &state).policy(BreadcrumbsPolicy {
                truncate: false,
                ..BreadcrumbsPolicy::default()
            });
        let layout = component.layout(Constraints::for_width(3), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 1));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("Hom"));
        let mut cx = bmux_tui::component::EventCx::new(&layout);
        component.handle_event(
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(2, 0),
            )),
            &layout,
            &mut cx,
        );
        assert_eq!(
            component.handle_event(
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Up(MouseButton::Left),
                    Point::new(2, 0)
                )),
                &layout,
                &mut cx,
            ),
            BreadcrumbsOutcome::Activated {
                index: 0,
                id: "home"
            }
        );
    }

    #[test]
    fn truncated_ellipsis_does_not_activate_hidden_label() {
        for (label, width, expected, interactive_width) in [
            ("Home", 3, "Ho…", 2),
            ("界abc", 4, "界a…", 3),
            ("界界", 2, "… ", 0),
            ("Home", 1, "…", 0),
        ] {
            let items = [BreadcrumbItem::new("home", label)];
            let state = Cell::new(BreadcrumbsState::new(None));
            let component = BreadcrumbsComponent::new("trail", &items, &state);
            let layout = component.layout(Constraints::for_width(width), &mut LayoutCx::new());
            let mut buffer = Buffer::empty(Rect::new(0, 0, width, 1));
            let mut frame = Frame::new(&mut buffer);
            component.paint(&layout, &mut PaintCx::new(&mut frame));
            assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some(expected));
            for x in 0..width {
                bmux_tui::component::EventCx::new(&layout).with_transform(
                    0,
                    0,
                    2,
                    1,
                    Rect::new(3, 1, width.saturating_sub(1), 1),
                    |cx| {
                        component.handle_event(
                            &Event::Mouse(MouseEvent::new(
                                MouseEventKind::Down(MouseButton::Left),
                                Point::new(x + 2, 1),
                            )),
                            &layout,
                            cx,
                        );
                        let outcome = component.handle_event(
                            &Event::Mouse(MouseEvent::new(
                                MouseEventKind::Up(MouseButton::Left),
                                Point::new(x + 2, 1),
                            )),
                            &layout,
                            cx,
                        );
                        assert_eq!(
                            matches!(outcome, BreadcrumbsOutcome::Activated { .. }),
                            x > 0 && x < interactive_width,
                            "label={label:?}, x={x}",
                        );
                    },
                );
            }
        }
    }

    #[test]
    fn disabled_item_changes_reuse_measurement() {
        let state = Cell::new(BreadcrumbsState::new(None));
        let enabled = [BreadcrumbItem::new("home", "Home")];
        let disabled = [BreadcrumbItem::new("home", "Home").disabled(true)];
        let before = BreadcrumbsComponent::new("trail", &enabled, &state);
        let after = BreadcrumbsComponent::new("trail", &disabled, &state);
        let mut cache = bmux_tui::component::LayoutCache::new();
        let mut cx = LayoutCx::new();
        let constraints = Constraints::for_width(10);
        let first = cache.layout("trail".into(), &before, constraints, &mut cx);
        let second = cache.layout("trail".into(), &after, constraints, &mut cx);
        assert_ne!(before.revision(), after.revision());
        assert_eq!(first.size, second.size);
        assert_eq!(cx.measured_nodes(), 1);
        assert_eq!(cache.stats().hits, 1);
        for (component, layout, interactive) in [(&before, &first, true), (&after, &second, false)]
        {
            let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 1));
            let mut frame = Frame::new(&mut buffer);
            component.paint(layout, &mut PaintCx::new(&mut frame));
            assert_eq!(frame.hits().regions().is_empty(), !interactive);
            let mut events = bmux_tui::component::EventCx::new(layout);
            component.handle_event(
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Down(MouseButton::Left),
                    Point::new(0, 0),
                )),
                layout,
                &mut events,
            );
            let outcome = component.handle_event(
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Up(MouseButton::Left),
                    Point::new(0, 0),
                )),
                layout,
                &mut events,
            );
            assert_eq!(
                matches!(outcome, BreadcrumbsOutcome::Activated { .. }),
                interactive
            );
        }
    }

    #[test]
    fn item_revision_tracks_identity_label_and_disabled_state() {
        let state = Cell::new(BreadcrumbsState::new(None));
        let original = [BreadcrumbItem::new("home", "Home")];
        let revision = BreadcrumbsComponent::new("trail", &original, &state).revision();
        for item in [
            BreadcrumbItem::new("other", "Home"),
            BreadcrumbItem::new("home", "Other"),
            BreadcrumbItem::new("home", "Home").disabled(true),
        ] {
            let items = [item];
            assert_ne!(
                BreadcrumbsComponent::new("trail", &items, &state).revision(),
                revision
            );
        }
        assert_eq!(
            BreadcrumbsComponent::new("trail", &original, &state).revision(),
            revision
        );
    }

    #[test]
    fn renders_breadcrumbs() {
        let items = [
            BreadcrumbItem::new("home", "Home"),
            BreadcrumbItem::new("docs", "Docs"),
        ];
        let state = BreadcrumbsState::new(Some(1));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 16, 1));
        let mut frame = Frame::new(&mut buffer);

        Breadcrumbs::new(&items).render(Rect::new(0, 0, 16, 1), &state, &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("Home / Docs     ")
        );
    }

    #[test]
    fn empty_breadcrumbs_ignore_keys_but_clear_pointer_state_on_focus_loss() {
        let items = [
            BreadcrumbItem::new("home", "Home"),
            BreadcrumbItem::new("docs", "Docs"),
        ];
        let initial = BreadcrumbsState {
            current: Some(0),
            hovered: Some(0),
            pressed: Some(0),
            focused: true,
        };
        let state = Cell::new(initial);
        let component = BreadcrumbsComponent::new("location", &items, &state);
        for (width, height) in [(0, 1), (10, 0), (0, 0)] {
            state.set(initial);
            let layout = component.layout(
                Constraints::new(width, width, height, Some(height)),
                &mut LayoutCx::new(),
            );
            let mut cx = bmux_tui::component::EventCx::new(&layout);
            for key in [KeyCode::Enter, KeyCode::Right] {
                assert_eq!(
                    component.handle_event(&Event::Key(KeyStroke::simple(key)), &layout, &mut cx),
                    BreadcrumbsOutcome::Ignored
                );
                assert_eq!(state.get(), initial);
            }
            component.handle_event(
                &Event::Focus(bmux_tui::event::FocusEvent::Lost),
                &layout,
                &mut cx,
            );
            assert_eq!(state.get().hovered, None);
            assert_eq!(state.get().pressed, None);
            assert_eq!(state.get().current, Some(0));
        }
    }

    #[test]
    fn clipped_breadcrumb_click_activates_visible_item() {
        let items = [
            BreadcrumbItem::new("home", "Home"),
            BreadcrumbItem::new("docs", "Docs"),
        ];
        let state = Cell::new(BreadcrumbsState::new(Some(0)));
        let component = BreadcrumbsComponent::new("location", &items, &state);
        let layout = component.layout(Constraints::for_width(11), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 4));
        let mut frame = Frame::new(&mut buffer);
        PaintCx::new(&mut frame).with_child(
            -3,
            2,
            bmux_tui::paint::LocalRect::new(7, 0, 4, 1),
            |cx| component.paint(&layout, cx),
        );
        assert!(frame.buffer().row_symbols(2).unwrap().contains("Docs"));
        bmux_tui::component::EventCx::new(&layout).with_transform(
            0,
            0,
            -3,
            2,
            Rect::new(4, 2, 4, 1),
            |cx| {
                component.handle_event(
                    &Event::Mouse(MouseEvent::new(
                        MouseEventKind::Down(MouseButton::Left),
                        Point::new(4, 2),
                    )),
                    &layout,
                    cx,
                );
                assert_eq!(
                    component.handle_event(
                        &Event::Mouse(MouseEvent::new(
                            MouseEventKind::Up(MouseButton::Left),
                            Point::new(4, 2)
                        )),
                        &layout,
                        cx
                    ),
                    BreadcrumbsOutcome::Activated {
                        index: 1,
                        id: "docs"
                    }
                );
            },
        );
    }

    #[test]
    fn keyboard_only_breadcrumbs_have_no_pointer_or_hover_metadata() {
        let items = [BreadcrumbItem::new("home", "Home")];
        let mut initial = BreadcrumbsState::new(Some(0));
        initial.set_focused(true);
        let state = Cell::new(initial);
        let mut policy = BreadcrumbsPolicy::default();
        policy.mouse.enabled = false;
        let component = BreadcrumbsComponent::new("location", &items, &state).policy(policy);
        let layout = component.layout(Constraints::for_width(10), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 1));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        let hit = &frame.hits().regions()[0];
        assert!(hit.focusable);
        assert!(!hit.pointer_events && !hit.hoverable);
        let mut cx = bmux_tui::component::EventCx::new(&layout);
        component.event(
            &Event::Mouse(MouseEvent::new(MouseEventKind::Move, Point::new(1, 0))),
            &layout,
            &mut cx,
        );
        assert_eq!(state.get(), initial);
        assert!(matches!(
            component.handle_event(
                &Event::Key(KeyStroke::simple(KeyCode::Enter)),
                &layout,
                &mut cx
            ),
            BreadcrumbsOutcome::Activated { .. }
        ));
    }

    #[test]
    fn composed_pointer_routing_cancels_invalid_targets() {
        for removed in [false, true] {
            let enabled = [BreadcrumbItem::new("home", "Home")];
            let disabled = [BreadcrumbItem::new("home", "Home").disabled(true)];
            let state = Cell::new(BreadcrumbsState::new(None));
            let component = BreadcrumbsComponent::new("crumbs", &enabled, &state);
            let layout = component.layout(Constraints::for_width(10), &mut LayoutCx::new());
            let mut cx = bmux_tui::component::EventCx::new(&layout);
            let mouse = |kind| Event::Mouse(MouseEvent::new(kind, Point::new(0, 0)));
            component.handle_event(
                &mouse(MouseEventKind::Down(MouseButton::Left)),
                &layout,
                &mut cx,
            );
            assert_eq!(state.get().pressed, Some(0));
            let changed =
                BreadcrumbsComponent::new("crumbs", if removed { &[] } else { &disabled }, &state);
            assert_eq!(
                changed.handle_event(&mouse(MouseEventKind::Move), &layout, &mut cx),
                BreadcrumbsOutcome::Redraw
            );
            assert_eq!(state.get().pressed, None);
            assert_eq!(state.get().hovered(), None);
            assert_eq!(
                changed.handle_event(&mouse(MouseEventKind::Move), &layout, &mut cx),
                BreadcrumbsOutcome::Ignored
            );
            assert_eq!(
                component.handle_event(
                    &mouse(MouseEventKind::Up(MouseButton::Left)),
                    &layout,
                    &mut cx
                ),
                BreadcrumbsOutcome::Ignored
            );
        }
    }

    #[test]
    fn composed_mouse_disable_cancels_pending_press() {
        let items = [BreadcrumbItem::new("home", "Home")];
        let state = Cell::new(BreadcrumbsState::new(None));
        let component = BreadcrumbsComponent::new("crumbs", &items, &state);
        let layout = component.layout(Constraints::for_width(10), &mut LayoutCx::new());
        let mut cx = bmux_tui::component::EventCx::new(&layout);
        let mouse = |kind| Event::Mouse(MouseEvent::new(kind, Point::new(0, 0)));
        component.handle_event(
            &mouse(MouseEventKind::Down(MouseButton::Left)),
            &layout,
            &mut cx,
        );
        assert_eq!(state.get().pressed, Some(0));
        let mut disabled = BreadcrumbsComponent::new("crumbs", &items, &state);
        disabled.breadcrumbs.policy.mouse.enabled = false;
        assert_eq!(
            disabled.handle_event(&mouse(MouseEventKind::Move), &layout, &mut cx),
            BreadcrumbsOutcome::Redraw
        );
        assert_eq!(state.get().pressed, None);
        assert_eq!(state.get().hovered(), None);
        assert_eq!(
            disabled.handle_event(&Event::Tick, &layout, &mut cx),
            BreadcrumbsOutcome::Ignored
        );
        assert_eq!(
            component.handle_event(
                &mouse(MouseEventKind::Up(MouseButton::Left)),
                &layout,
                &mut cx
            ),
            BreadcrumbsOutcome::Ignored
        );
    }

    #[test]
    fn render_registers_exact_composite_geometry() {
        let items = [
            BreadcrumbItem::new("home", "Home"),
            BreadcrumbItem::new("docs", "Docs"),
        ];
        let state = BreadcrumbsState::new(Some(1));
        let mut buffer = Buffer::empty(Rect::new(3, 2, 20, 3));
        let mut frame = Frame::new(&mut buffer);

        Breadcrumbs::new(&items).render_with_id(
            "location",
            Rect::new(6, 3, 14, 1),
            &state,
            &mut frame,
        );

        let regions = frame.hits().regions();
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].id.as_str(), "location");
        assert_eq!(regions[0].area, Rect::new(6, 3, 14, 1));
        assert_eq!(regions[0].role, HitRole::ListItem);
        assert!(regions[0].focusable);
        assert!(regions[0].pointer_events);
        assert_eq!(frame.hits().focus_targets(None).len(), 1);
    }

    #[test]
    fn empty_or_fully_disabled_breadcrumbs_register_nothing() {
        let disabled = [BreadcrumbItem::new("home", "Home").disabled(true)];
        let empty: [BreadcrumbItem<'_>; 0] = [];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 2));
        let mut frame = Frame::new(&mut buffer);

        Breadcrumbs::new(&disabled).render_with_id(
            "disabled",
            Rect::new(0, 0, 12, 1),
            &BreadcrumbsState::new(Some(0)),
            &mut frame,
        );
        Breadcrumbs::new(&empty).render_with_id(
            "empty",
            Rect::new(0, 1, 12, 1),
            &BreadcrumbsState::new(None),
            &mut frame,
        );

        assert!(frame.hits().regions().is_empty());
    }

    #[test]
    fn keyboard_moves_current_item() {
        let items = [
            BreadcrumbItem::new("home", "Home"),
            BreadcrumbItem::new("docs", "Docs"),
        ];
        let mut state = BreadcrumbsState::new(Some(0));
        state.set_focused(true);

        assert_eq!(
            Breadcrumbs::new(&items).handle_event(
                Rect::new(0, 0, 16, 1),
                &mut state,
                &Event::Key(KeyStroke::simple(KeyCode::Right)),
            ),
            BreadcrumbsOutcome::Redraw
        );
        assert_eq!(state.current(), Some(1));
    }

    #[test]
    fn directly_dispatched_breadcrumbs_key_navigates_without_visual_focus() {
        let items = [
            BreadcrumbItem::new("home", "Home"),
            BreadcrumbItem::new("docs", "Docs"),
        ];
        let mut state = BreadcrumbsState::new(Some(0));

        let outcome = Breadcrumbs::new(&items).handle_event(
            Rect::new(0, 0, 16, 1),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Right)),
        );

        assert_eq!(outcome, BreadcrumbsOutcome::Redraw);
        assert_eq!(state.current(), Some(1));
    }

    #[test]
    fn enter_activates_current_item() {
        let items = [
            BreadcrumbItem::new("home", "Home"),
            BreadcrumbItem::new("docs", "Docs"),
        ];
        let mut state = BreadcrumbsState::new(Some(1));
        state.set_focused(true);

        assert_eq!(
            Breadcrumbs::new(&items).handle_event(
                Rect::new(0, 0, 16, 1),
                &mut state,
                &Event::Key(KeyStroke::simple(KeyCode::Enter)),
            ),
            BreadcrumbsOutcome::Activated {
                index: 1,
                id: "docs"
            }
        );
    }

    #[test]
    fn mouse_click_activates_item() {
        let items = [
            BreadcrumbItem::new("home", "Home"),
            BreadcrumbItem::new("docs", "Docs"),
        ];
        let mut state = BreadcrumbsState::new(None);
        let breadcrumbs = Breadcrumbs::new(&items);
        let area = Rect::new(0, 0, 16, 1);

        assert_eq!(
            breadcrumbs.handle_event(
                area,
                &mut state,
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Down(MouseButton::Left),
                    Point::new(7, 0)
                )),
            ),
            BreadcrumbsOutcome::Redraw
        );
        assert_eq!(
            breadcrumbs.handle_event(
                area,
                &mut state,
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Up(MouseButton::Left),
                    Point::new(7, 0)
                )),
            ),
            BreadcrumbsOutcome::Activated {
                index: 1,
                id: "docs"
            }
        );
    }

    #[test]
    fn disabled_items_do_not_activate() {
        let items = [BreadcrumbItem::new("home", "Home").disabled(true)];
        let mut state = BreadcrumbsState::new(Some(0));
        state.set_focused(true);

        assert_eq!(
            Breadcrumbs::new(&items).handle_event(
                Rect::new(0, 0, 8, 1),
                &mut state,
                &Event::Key(KeyStroke::simple(KeyCode::Enter)),
            ),
            BreadcrumbsOutcome::Ignored
        );
    }

    #[test]
    fn truncates_to_area() {
        let items = [
            BreadcrumbItem::new("home", "Home"),
            BreadcrumbItem::new("docs", "Documentation"),
        ];
        let state = BreadcrumbsState::new(None);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        let mut frame = Frame::new(&mut buffer);

        Breadcrumbs::new(&items).render(Rect::new(0, 0, 8, 1), &state, &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("Home / …"));
    }

    #[test]
    fn component_pointer_activation_uses_translated_layout() {
        let items = [
            BreadcrumbItem::new("home", "Home"),
            BreadcrumbItem::new("docs", "Docs"),
        ];
        let state = Cell::new(BreadcrumbsState::new(None));
        let component = BreadcrumbsComponent::new("trail", &items, &state);
        let layout = component.layout(Constraints::tight(Size::new(11, 1)), &mut LayoutCx::new());
        let dispatch = |kind, position| {
            bmux_tui::component::EventCx::new(&layout).with_transform(
                0,
                0,
                20,
                3,
                Rect::new(20, 3, 11, 1),
                |cx| {
                    component.handle_event(
                        &Event::Mouse(MouseEvent::new(kind, position)),
                        &layout,
                        cx,
                    )
                },
            )
        };
        dispatch(MouseEventKind::Down(MouseButton::Left), Point::new(27, 3));
        assert_eq!(
            dispatch(MouseEventKind::Up(MouseButton::Left), Point::new(27, 3)),
            BreadcrumbsOutcome::Activated {
                index: 1,
                id: "docs"
            }
        );
        dispatch(MouseEventKind::Down(MouseButton::Left), Point::new(7, 0));
        assert!(!matches!(
            dispatch(MouseEventKind::Up(MouseButton::Left), Point::new(7, 0)),
            BreadcrumbsOutcome::Activated { .. }
        ));
        assert_eq!(state.get().pressed, None);
    }

    #[test]
    fn canonical_component_uses_one_layout_for_all_channels() {
        let items = [
            BreadcrumbItem::new("home", "Home"),
            BreadcrumbItem::new("docs", "Docs"),
        ];
        let state = Cell::new(BreadcrumbsState::new(Some(0)));
        let breadcrumbs = BreadcrumbsComponent::new("trail", &items, &state);
        let mut layout_cx = LayoutCx::new();
        let layout = breadcrumbs.layout(Constraints::loose(Size::new(20, 2)), &mut layout_cx);
        assert_eq!(layout.size, bmux_tui::component::LogicalSize::new(11, 1));
        assert_eq!(layout.metadata.semantics, ["breadcrumbs"]);

        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 2));
        let mut frame = Frame::new(&mut buffer);
        breadcrumbs.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(frame.hits().regions()[0].area, Rect::new(0, 0, 11, 1));
        assert_eq!(frame.semantics().regions()[0].area, Rect::new(0, 0, 11, 1));
        assert_eq!(
            frame
                .damage(bmux_tui::damage::DamagePolicy::default())
                .retained_regions(),
            &[Rect::new(0, 0, 11, 1)]
        );
    }

    #[test]
    fn canonical_component_revision_separates_geometry_and_paint() {
        let items = [BreadcrumbItem::new("home", "Home")];
        let state = Cell::new(BreadcrumbsState::new(Some(0)));
        let initial = BreadcrumbsComponent::new("trail", &items, &state).revision();
        let bare = BreadcrumbsComponent::new("trail", &items, &state)
            .policy(BreadcrumbsPolicy::bare())
            .revision();
        assert_eq!(initial.layout, bare.layout);
        assert_ne!(initial.paint, bare.paint);

        let longer = [BreadcrumbItem::new("home", "Homepage")];
        assert_ne!(
            initial.layout,
            BreadcrumbsComponent::new("trail", &longer, &state)
                .revision()
                .layout
        );
    }
}
