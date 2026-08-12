//! Generic breadcrumbs / path trail component.

use bmux_keyboard::KeyCode;
use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::hit::{HitId, HitRegion as SceneRegion, HitRole};
use bmux_tui::prelude::{Line, Span};
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui::text_width::display_width;

use crate::common::ComponentMousePolicy;
use crate::hit_test::{HitRegion, hit_region_at};

/// One breadcrumb item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
pub struct Breadcrumbs<'a> {
    items: &'a [BreadcrumbItem<'a>],
    policy: BreadcrumbsPolicy,
    styles: BreadcrumbsStyles,
}

impl<'a> Breadcrumbs<'a> {
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

    /// Set policy.
    #[must_use]
    pub const fn policy(mut self, policy: BreadcrumbsPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set styles.
    #[must_use]
    pub const fn styles(mut self, styles: BreadcrumbsStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Render breadcrumbs and register the composite interaction area.
    pub fn render(&self, area: Rect, state: &BreadcrumbsState, frame: &mut Frame<'_>) {
        let id = frame.next_interaction_id("breadcrumbs");
        self.render_with_id(id, area, state, frame);
    }

    /// Render breadcrumbs with a stable interaction identifier.
    pub fn render_with_id(
        &self,
        id: impl Into<HitId>,
        area: Rect,
        state: &BreadcrumbsState,
        frame: &mut Frame<'_>,
    ) {
        if area.is_empty() {
            return;
        }
        let interactive = self.policy.keyboard || self.policy.mouse.enabled;
        if interactive && self.items.iter().any(|item| !item.disabled) {
            frame.push_hit(
                SceneRegion::new(id, area)
                    .role(HitRole::ListItem)
                    .pointer_events(self.policy.mouse.enabled)
                    .hoverable(self.policy.mouse.hover)
                    .focusable(self.policy.keyboard),
            );
        }
        let mut line = self.line(state);
        if self.policy.truncate {
            line = line.truncate(usize::from(area.width));
        }
        frame.write_line(area, &line);
    }

    /// Handle one event.
    pub fn handle_event(
        &self,
        area: Rect,
        state: &mut BreadcrumbsState,
        event: &Event,
    ) -> BreadcrumbsOutcome<'a> {
        match event {
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
            Event::Key(_)
            | Event::Mouse(_)
            | Event::Resize(_)
            | Event::Paste(_)
            | Event::Focus(_)
            | Event::Tick
            | Event::User(_) => BreadcrumbsOutcome::Ignored,
        }
    }

    fn handle_mouse(
        &self,
        area: Rect,
        state: &mut BreadcrumbsState,
        mouse: MouseEvent,
    ) -> BreadcrumbsOutcome<'a> {
        match mouse.kind {
            MouseEventKind::Move if self.policy.mouse.hover => {
                let hovered = self.item_at(area, mouse.position);
                if hovered == state.hovered {
                    BreadcrumbsOutcome::Ignored
                } else {
                    state.hovered = hovered;
                    BreadcrumbsOutcome::Redraw
                }
            }
            MouseEventKind::Down(MouseButton::Left) if self.policy.mouse.click => {
                state.pressed = self.item_at(area, mouse.position);
                BreadcrumbsOutcome::Redraw
            }
            MouseEventKind::Up(MouseButton::Left) if self.policy.mouse.click => {
                let released = self.item_at(area, mouse.position);
                let pressed = state.pressed.take();
                if released == pressed
                    && let Some(index) = released
                {
                    return self.activate(index).unwrap_or(BreadcrumbsOutcome::Ignored);
                }
                BreadcrumbsOutcome::Redraw
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
        let current = state
            .current
            .unwrap_or(0)
            .min(self.items.len().saturating_sub(1));
        let next = if delta.is_negative() {
            current.saturating_sub(1)
        } else {
            current
                .saturating_add(1)
                .min(self.items.len().saturating_sub(1))
        };
        if next == current || self.items[next].disabled {
            BreadcrumbsOutcome::Ignored
        } else {
            state.current = Some(next);
            BreadcrumbsOutcome::Redraw
        }
    }

    fn activate(&self, index: usize) -> Option<BreadcrumbsOutcome<'a>> {
        self.items.get(index).and_then(|item| {
            (!item.disabled).then_some(BreadcrumbsOutcome::Activated { index, id: item.id })
        })
    }

    fn item_hit_regions(&self, area: Rect) -> Vec<HitRegion<usize>> {
        let mut x = area.x;
        let mut regions = Vec::new();
        for (index, item) in self.items.iter().enumerate() {
            let width = u16_saturating(display_width(item.label));
            regions.push(HitRegion::new(index, Rect::new(x, area.y, width, 1)));
            x = x
                .saturating_add(width)
                .saturating_add(u16_saturating(display_width(self.policy.separator)));
        }
        regions
    }

    fn item_at(&self, area: Rect, position: Point) -> Option<usize> {
        if !area.contains(position) || position.y != area.y {
            return None;
        }
        hit_region_at(&self.item_hit_regions(area), position).map(|region| region.key)
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

fn u16_saturating(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
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
    use bmux_keyboard::{KeyCode, KeyStroke};
    use bmux_tui::buffer::Buffer;
    use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};
    use bmux_tui::hit::HitRole;

    use super::{BreadcrumbItem, Breadcrumbs, BreadcrumbsOutcome, BreadcrumbsState};

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
}
