//! Configurable selectable-list component.
//!
//! Item rows are stacked into one measured content node and scrolled through
//! the shared [`ScrollView`] controller: the viewport layout, offset clamping,
//! wheel/page/scrollbar interaction, and gutter geometry are owned by
//! `scroll_view`, so this module contains no independent row-skip engine.

use std::cell::Cell;
use std::hash::{Hash, Hasher};

use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::component::{
    ChildLayout, Component, ComponentRevision, Constraints, EventCx, LayoutCx, LayoutId,
    LayoutMetadata, LayoutNode, LogicalSize,
};
use bmux_tui::event::{Event, EventOutcome, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::geometry::Rect;
use bmux_tui::hit::{HitRegion as SceneRegion, HitRole};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::semantic::SemanticRegion;
use bmux_tui::style::Modifier;
use bmux_tui::text_width::display_width;

use crate::common::{ComponentMousePolicy, InteractionState, local_area_of, u16_saturating};
use crate::hit_test::{HitRegion, hit_region_at};
use crate::scroll_view::{
    ScrollView, ScrollViewComponent, ScrollViewOutcome, ScrollViewPolicy, ScrollViewState,
};
use crate::scrollbar::ScrollbarStyles;
use crate::scrollbar_layout::ScrollbarAxisLayoutMode;

/// One selectable list item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectableListItem {
    /// Stable item id chosen by the caller.
    pub id: String,
    /// Visible rich item content lines.
    pub lines: Vec<Line>,
    /// Whether this item is disabled independently from the whole list.
    pub disabled: bool,
}

impl SelectableListItem {
    /// Create an enabled selectable-list item.
    #[must_use]
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        let label = label.into();
        Self {
            id: id.into(),
            lines: vec![Line::from(label)],
            disabled: false,
        }
    }

    /// Create an enabled selectable-list item with rich line content.
    #[must_use]
    pub fn rich(id: impl Into<String>, line: impl Into<Line>) -> Self {
        Self {
            id: id.into(),
            lines: vec![line.into()],
            disabled: false,
        }
    }

    /// Create an enabled selectable-list item with multiline rich content.
    #[must_use]
    pub fn multiline(id: impl Into<String>, lines: impl Into<Vec<Line>>) -> Self {
        Self {
            id: id.into(),
            lines: lines.into(),
            disabled: false,
        }
    }

    /// Return rendered item height in rows.
    #[must_use]
    pub fn height(&self) -> usize {
        self.lines.len().max(1)
    }

    /// Return this item with disabled state set.
    #[must_use]
    pub const fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

/// Visual styles for a selectable list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectableListStyles {
    /// Style filling the complete control area, including unused rows and gutter.
    pub background: Style,
    /// Styles for the integrated scrollbar.
    pub scrollbar: ScrollbarStyles,
    /// Style used for enabled inactive items.
    pub normal: Style,
    /// Style used for the focused item.
    pub focused: Style,
    /// Style used for the selected item.
    pub selected: Style,
    /// Style used for the hovered item.
    pub hovered: Style,
    /// Style used while an item is pressed.
    pub pressed: Style,
    /// Style used for disabled items or lists.
    pub disabled: Style,
}

impl Default for SelectableListStyles {
    fn default() -> Self {
        Self {
            background: Style::new(),
            scrollbar: ScrollbarStyles::default(),
            normal: Style::new(),
            focused: Style::new().add_modifier(Modifier::REVERSED),
            selected: Style::new().add_modifier(Modifier::BOLD),
            hovered: Style::new().add_modifier(Modifier::UNDERLINE),
            pressed: Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED),
            disabled: Style::new().add_modifier(Modifier::DIM),
        }
    }
}

/// Keyboard behavior for a selectable list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct SelectableListKeyboardPolicy {
    /// Whether arrow keys move focus between items.
    pub arrows_move_focus: bool,
    /// Whether Home and End move to the first/last enabled item.
    pub home_end_move_focus: bool,
    /// Whether focus wraps at list edges.
    pub wrap: bool,
    /// Whether Enter selects the focused item.
    pub enter_selects: bool,
    /// Whether Space selects the focused item.
    pub space_selects: bool,
}

impl SelectableListKeyboardPolicy {
    /// Common interactive keyboard behavior.
    #[must_use]
    pub const fn interactive() -> Self {
        Self {
            arrows_move_focus: true,
            home_end_move_focus: true,
            wrap: false,
            enter_selects: true,
            space_selects: true,
        }
    }
}

impl Default for SelectableListKeyboardPolicy {
    fn default() -> Self {
        Self::interactive()
    }
}

/// Highlight symbol behavior for selected list items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectableListHighlightPolicy {
    /// Symbol rendered before the selected item.
    pub symbol: &'static str,
    /// Whether non-selected rows reserve equivalent spacing.
    pub repeat_spacing: bool,
}

impl SelectableListHighlightPolicy {
    /// Create a highlight policy.
    #[must_use]
    pub const fn new(symbol: &'static str, repeat_spacing: bool) -> Self {
        Self {
            symbol,
            repeat_spacing,
        }
    }
}

impl Default for SelectableListHighlightPolicy {
    fn default() -> Self {
        Self::new(">", true)
    }
}

/// Configurable selectable-list behavior policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectableListPolicy {
    /// Mouse behavior.
    pub mouse: ComponentMousePolicy,
    /// Keyboard behavior.
    pub keyboard: SelectableListKeyboardPolicy,
    /// Highlight symbol behavior.
    pub highlight: SelectableListHighlightPolicy,
    /// Optional integrated vertical scrollbar mode.
    pub scrollbar: ScrollbarAxisLayoutMode,
}

impl SelectableListPolicy {
    /// Common interactive selectable-list behavior.
    #[must_use]
    pub const fn interactive() -> Self {
        Self {
            mouse: ComponentMousePolicy::button(),
            keyboard: SelectableListKeyboardPolicy::interactive(),
            highlight: SelectableListHighlightPolicy::new(">", true),
            scrollbar: ScrollbarAxisLayoutMode::Hidden,
        }
    }
    /// Return policy with integrated vertical scrollbar mode set.
    #[must_use]
    pub const fn scrollbar(mut self, mode: ScrollbarAxisLayoutMode) -> Self {
        self.scrollbar = mode;
        self
    }

    /// Shared scroll-view policy derived from this list policy.
    ///
    /// Keyboard scrolling is owned by the list's focus navigation and paging
    /// keys, so the shared controller handles wheel and scrollbar input only.
    #[must_use]
    pub const fn scroll_view_policy(self) -> ScrollViewPolicy {
        ScrollViewPolicy {
            keyboard: false,
            mouse_wheel: self.mouse.enabled,
            vertical_scrollbar: self.scrollbar,
            horizontal_scrollbar: ScrollbarAxisLayoutMode::Hidden,
            wheel_rows: 1,
        }
    }
}

impl Default for SelectableListPolicy {
    fn default() -> Self {
        Self::interactive()
    }
}

/// Runtime selectable-list state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectableListState {
    /// Common list interaction flags.
    pub interaction: InteractionState,
    /// Shared logical scroll state.
    pub scroll: ScrollViewState,
    selected: Option<usize>,
    focused: Option<usize>,
    hovered: Option<usize>,
    pressed: Option<usize>,
}

impl SelectableListState {
    /// Create enabled selectable-list state.
    #[must_use]
    pub const fn new(selected: Option<usize>) -> Self {
        Self {
            interaction: InteractionState::new(),
            scroll: ScrollViewState::new(),
            selected,
            focused: selected,
            hovered: None,
            pressed: None,
        }
    }

    /// Return selected item index.
    #[must_use]
    pub const fn selected(self) -> Option<usize> {
        self.selected
    }

    /// Set selected item index.
    pub const fn set_selected(&mut self, selected: Option<usize>) {
        self.selected = selected;
    }

    /// Return focused item index.
    #[must_use]
    pub const fn focused(self) -> Option<usize> {
        self.focused
    }

    /// Set focused item index.
    pub const fn set_focused(&mut self, focused: Option<usize>) {
        self.focused = focused;
        self.interaction.focused = focused.is_some();
    }

    /// Return vertical scroll offset in logical item rows.
    #[must_use]
    pub const fn vertical_scroll(self) -> usize {
        self.scroll.vertical_offset()
    }

    /// Set vertical scroll offset in logical item rows before clamping.
    pub const fn set_vertical_scroll(&mut self, vertical_scroll: usize) {
        self.scroll.set_vertical_offset(vertical_scroll);
    }

    /// Set disabled state for the whole list.
    pub const fn set_disabled(&mut self, disabled: bool) {
        self.interaction.disabled = disabled;
        if disabled {
            self.hovered = None;
            self.pressed = None;
        }
    }
}

/// Outcome from selectable-list input handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectableListOutcome {
    /// Event was not handled.
    Ignored,
    /// Visual state changed without changing selected value.
    Redraw,
    /// Focus moved to the contained item index.
    Focused(usize),
    /// Selection changed to the contained item index.
    Selected(usize),
}

/// Configurable vertical selectable-list control.
///
/// The controller measures item rows into one stacked content node, resolves
/// the viewport through [`ScrollViewComponent::viewport_layout`], and routes
/// wheel, paging, scrollbar, and ensure-visible behavior through the shared
/// [`ScrollView`] controller against that authoritative layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectableList<'a> {
    items: &'a [SelectableListItem],
    policy: SelectableListPolicy,
    styles: SelectableListStyles,
}

impl<'a> SelectableList<'a> {
    /// Create a selectable list over caller-owned items.
    #[must_use]
    pub fn new(items: &'a [SelectableListItem]) -> Self {
        Self {
            items,
            policy: SelectableListPolicy::default(),
            styles: SelectableListStyles::default(),
        }
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: SelectableListPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: SelectableListStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Return required render size.
    ///
    /// The width covers the widest item plus the highlight prefix and a
    /// two-cell margin; the height is the exact stacked item height.
    #[must_use]
    pub fn size(&self) -> (u16, u16) {
        let prefix_width = self.prefix_width();
        let width = self
            .items
            .iter()
            .map(|item| {
                item.lines
                    .iter()
                    .map(Line::width)
                    .max()
                    .unwrap_or(0)
                    .saturating_add(prefix_width)
            })
            .max()
            .unwrap_or(0);

        (
            u16::try_from(width).unwrap_or(u16::MAX).saturating_add(2),
            u16_saturating(self.total_height()),
        )
    }

    /// Shared scroll controller configured from this list's policy and styles.
    #[must_use]
    pub const fn scroll_view(&self) -> ScrollView {
        ScrollView::new()
            .policy(self.policy.scroll_view_policy())
            .scrollbar_styles(self.styles.scrollbar)
    }

    /// Content viewport after reserving the integrated scrollbar gutter.
    #[must_use]
    pub const fn content_area(&self, area: Rect) -> Rect {
        self.scroll_view().content_area(area)
    }

    /// Resolve the authoritative list layout for one terminal area.
    ///
    /// The result has the shape produced by [`Component::layout`] for
    /// [`SelectableListComponent`]: the list node contains one scroll viewport
    /// child whose single child is the stacked item content.
    #[must_use]
    pub fn layout(&self, id: &LayoutId, area: Rect) -> LayoutNode {
        self.layout_with(id, Constraints::tight(area.size()))
    }

    /// Return maximum vertical scroll offset for this area.
    #[must_use]
    pub fn max_vertical_scroll(&self, area: Rect) -> usize {
        resolved_viewport(&self.layout(&LayoutId::new("list"), area))
            .map_or(0, ScrollView::max_vertical_offset)
    }

    /// Clamp caller-owned state to valid scroll bounds for this area.
    pub fn clamp_state(&self, area: Rect, state: &mut SelectableListState) {
        let layout = self.layout(&LayoutId::new("list"), area);
        if let Some(viewport) = resolved_viewport(&layout) {
            self.scroll_view().reconcile(viewport, &mut state.scroll);
        }
    }

    /// Paint visible item rows and the integrated scrollbar through a scoped
    /// local-coordinate context; `area` is expressed in that context's
    /// coordinates.
    ///
    /// Item rows fill their complete width with `fallback` patched by the
    /// item style; content that lies outside the viewport clip is dropped by
    /// the context rather than by this primitive.
    pub fn paint(
        &self,
        area: Rect,
        state: &SelectableListState,
        fallback: Style,
        cx: &mut PaintCx<'_, '_>,
    ) {
        if area.is_empty() {
            return;
        }
        let layout = self.layout(&LayoutId::new("list"), area);
        self.paint_layout(&layout, area, state, fallback, cx);
    }

    pub(crate) fn paint_layout(
        &self,
        layout: &LayoutNode,
        area: Rect,
        state: &SelectableListState,
        fallback: Style,
        cx: &mut PaintCx<'_, '_>,
    ) {
        let Some(viewport) = resolved_viewport(layout) else {
            return;
        };
        let local = Rect::new(0, 0, area.width, area.height);
        let content_area = self.content_area(local);
        cx.with_child(
            i32::from(area.x),
            i64::from(area.y),
            LocalRect::terminal(local),
            |cx| {
                cx.fill(LocalRect::terminal(local), " ", fallback);
                cx.with_child(
                    i32::from(content_area.x),
                    i64::from(content_area.y),
                    LocalRect::new(0, 0, content_area.width, content_area.height),
                    |cx| {
                        ScrollViewComponent::new(
                            viewport.id.clone(),
                            viewport.size,
                            state.scroll,
                            ItemRows {
                                list: self,
                                id: content_id_of(viewport),
                                state: *state,
                                fallback,
                                offset: state.scroll.vertical_offset(),
                                viewport_height: viewport.size.height,
                            },
                        )
                        .paint(viewport, cx);
                    },
                );
                self.scroll_view()
                    .paint_scrollbars(local, viewport, &state.scroll, cx);
            },
        );
    }

    /// Return visible hit regions keyed by stable item id for tests/semantics.
    ///
    /// `area` is the complete list rectangle; regions are reported inside its
    /// content viewport after the current scroll offset and are clipped to
    /// the viewport bottom.
    #[must_use]
    pub fn visible_semantic_regions(
        &self,
        area: Rect,
        state: &SelectableListState,
    ) -> Vec<HitRegion<&'a str>> {
        self.visible_hit_regions(self.content_area(area), state)
            .into_iter()
            .filter_map(|region| {
                self.items
                    .get(region.key)
                    .map(|item| HitRegion::new(item.id.as_str(), region.rect))
            })
            .collect()
    }

    /// Return stable item id at a point, if any visible item contains it.
    #[must_use]
    pub fn semantic_id_at(
        &self,
        area: Rect,
        state: &SelectableListState,
        point: bmux_tui::geometry::Point,
    ) -> Option<&'a str> {
        let regions = self.visible_semantic_regions(area, state);
        hit_region_at(&regions, point).map(|region| region.key)
    }

    /// Handle one input event against the terminal rectangle the list was
    /// painted into.
    pub fn handle_event(
        &self,
        area: Rect,
        state: &mut SelectableListState,
        event: &Event,
    ) -> SelectableListOutcome {
        let layout = self.layout(&LayoutId::new("list"), area);
        self.handle_event_with_layout(&layout, area, state, event)
    }

    fn handle_event_with_layout(
        &self,
        layout: &LayoutNode,
        area: Rect,
        state: &mut SelectableListState,
        event: &Event,
    ) -> SelectableListOutcome {
        self.normalize_state(state);
        if state.interaction.disabled {
            return SelectableListOutcome::Ignored;
        }
        let Some(viewport) = resolved_viewport(layout) else {
            return SelectableListOutcome::Ignored;
        };
        match event {
            Event::Key(stroke) => self.handle_key(viewport, state, *stroke),
            Event::Mouse(mouse) => self.handle_mouse(viewport, area, state, *mouse),
            Event::Resize(_) | Event::Paste(_) | Event::Focus(_) | Event::Tick | Event::User(_) => {
                SelectableListOutcome::Ignored
            }
        }
    }

    pub(crate) fn layout_with(&self, id: &LayoutId, constraints: Constraints) -> LayoutNode {
        let scroll_view = self.scroll_view();
        let (natural_width, _) = self.size();
        let width = constraints
            .constrain(LogicalSize::new(natural_width, 0))
            .width;
        // Gutter reservation depends only on policy and outer width, so resolve
        // it against a tall probe rectangle before the height is known.
        let probe = scroll_view.content_area(Rect::new(0, 0, width, u16::MAX));
        let content = self.content_layout(content_id(id), probe.width);
        let size = constraints.constrain(LogicalSize::new(width, content.size.height));
        let content_area = scroll_view.content_area(local_area_of(size));
        let viewport = ScrollViewComponent::viewport_layout(
            viewport_id(id),
            LogicalSize::new(content_area.width, usize::from(content_area.height)),
            content,
        );
        LayoutNode::with_children(
            id.clone(),
            size,
            vec![ChildLayout::new(
                content_area.x,
                usize::from(content_area.y),
                viewport,
            )],
        )
        .with_metadata(LayoutMetadata::new().semantic("list"))
    }

    /// Exact stacked item content at one content width.
    fn content_layout(&self, id: LayoutId, width: u16) -> LayoutNode {
        LayoutNode::leaf(id, LogicalSize::new(width, self.total_height()))
    }

    fn line(
        &self,
        index: usize,
        item: &SelectableListItem,
        line_index: usize,
        state: SelectableListState,
    ) -> Line {
        let style = self.style_for(index, item, state);
        let mut spans = Vec::new();
        let prefix = if line_index == 0 && state.selected == Some(index) {
            self.policy.highlight.symbol.to_string()
        } else if self.policy.highlight.repeat_spacing {
            " ".repeat(display_width(self.policy.highlight.symbol))
        } else {
            String::new()
        };
        if !prefix.is_empty() || self.policy.highlight.repeat_spacing {
            spans.push(Span::styled(format!("{prefix} "), style));
        }
        spans.extend(
            item.lines
                .get(line_index)
                .into_iter()
                .flat_map(|line| line.spans.iter())
                .map(|span| Span::styled(span.content.clone(), style.patch(span.style))),
        );
        Line::from_spans(spans)
    }

    fn prefix_width(&self) -> usize {
        if self.policy.highlight.repeat_spacing || !self.policy.highlight.symbol.is_empty() {
            display_width(self.policy.highlight.symbol).saturating_add(1)
        } else {
            0
        }
    }

    fn style_for(
        &self,
        index: usize,
        item: &SelectableListItem,
        state: SelectableListState,
    ) -> Style {
        if state.interaction.disabled || item.disabled {
            self.styles.disabled
        } else if state.pressed == Some(index) {
            self.styles.pressed
        } else if state.focused == Some(index) {
            self.styles.focused
        } else if state.hovered == Some(index) {
            self.styles.hovered
        } else if state.selected == Some(index) {
            self.styles.selected
        } else {
            self.styles.normal
        }
    }

    fn handle_key(
        &self,
        viewport: &LayoutNode,
        state: &mut SelectableListState,
        stroke: KeyStroke,
    ) -> SelectableListOutcome {
        if !stroke.modifiers.is_empty() {
            return SelectableListOutcome::Ignored;
        }
        let page = isize::try_from(viewport.size.height.max(1)).unwrap_or(isize::MAX);
        match stroke.key {
            KeyCode::Up if self.policy.keyboard.arrows_move_focus => {
                self.move_focus(viewport, state, Direction::Previous)
            }
            KeyCode::Down if self.policy.keyboard.arrows_move_focus => {
                self.move_focus(viewport, state, Direction::Next)
            }
            KeyCode::Home if self.policy.keyboard.home_end_move_focus => {
                self.focus_edge(viewport, state, true)
            }
            KeyCode::End if self.policy.keyboard.home_end_move_focus => {
                self.focus_edge(viewport, state, false)
            }
            KeyCode::Enter if self.policy.keyboard.enter_selects => {
                self.select_focused(viewport, state)
            }
            KeyCode::Space | KeyCode::Char(' ') if self.policy.keyboard.space_selects => {
                self.select_focused(viewport, state)
            }
            KeyCode::PageUp => Self::scroll_by(viewport, state, page.saturating_neg()),
            KeyCode::PageDown => Self::scroll_by(viewport, state, page),
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
            | KeyCode::Insert
            | KeyCode::F(_) => SelectableListOutcome::Ignored,
        }
    }

    fn handle_mouse(
        &self,
        viewport: &LayoutNode,
        area: Rect,
        state: &mut SelectableListState,
        mouse: MouseEvent,
    ) -> SelectableListOutcome {
        if !self.policy.mouse.enabled {
            return SelectableListOutcome::Ignored;
        }
        let scroll_view = self.scroll_view();
        let event = Event::Mouse(mouse);
        let scrollbar =
            scroll_view.handle_scrollbar_event(area, viewport, &mut state.scroll, &event);
        if scrollbar != ScrollViewOutcome::Ignored || state.scroll.dragging_scrollbar() {
            return SelectableListOutcome::Redraw;
        }
        let content_area = self.content_area(area);
        if matches!(
            mouse.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) {
            return scroll_outcome(scroll_view.handle_event(
                content_area,
                viewport,
                &mut state.scroll,
                &event,
            ));
        }
        let hit = self.hit_index(content_area, state, mouse);
        match mouse.kind {
            MouseEventKind::Move if self.policy.mouse.hover => Self::hover(state, hit),
            MouseEventKind::Down(MouseButton::Left) if self.policy.mouse.click => {
                Self::press(state, hit)
            }
            MouseEventKind::Up(MouseButton::Left) if self.policy.mouse.click => {
                self.release(viewport, state, hit)
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
            | MouseEventKind::Move => SelectableListOutcome::Ignored,
        }
    }

    fn hover(state: &mut SelectableListState, hit: Option<usize>) -> SelectableListOutcome {
        if state.hovered == hit {
            SelectableListOutcome::Ignored
        } else {
            state.hovered = hit;
            SelectableListOutcome::Redraw
        }
    }

    const fn press(state: &mut SelectableListState, hit: Option<usize>) -> SelectableListOutcome {
        let Some(index) = hit else {
            return SelectableListOutcome::Ignored;
        };
        state.pressed = Some(index);
        state.hovered = Some(index);
        state.set_focused(Some(index));
        SelectableListOutcome::Redraw
    }

    fn release(
        &self,
        viewport: &LayoutNode,
        state: &mut SelectableListState,
        hit: Option<usize>,
    ) -> SelectableListOutcome {
        let was_pressed = state.pressed;
        state.pressed = None;
        if let Some(index) = hit.filter(|hit_index| was_pressed == Some(*hit_index)) {
            return self.select_index(viewport, state, index);
        }
        if was_pressed.is_some() {
            SelectableListOutcome::Redraw
        } else {
            SelectableListOutcome::Ignored
        }
    }

    fn drag(state: &mut SelectableListState, hit: Option<usize>) -> SelectableListOutcome {
        let pressed = if state.pressed.is_some() { hit } else { None };
        if state.hovered == hit && state.pressed == pressed {
            SelectableListOutcome::Ignored
        } else {
            state.hovered = hit;
            state.pressed = pressed;
            SelectableListOutcome::Redraw
        }
    }

    fn scroll_by(
        viewport: &LayoutNode,
        state: &mut SelectableListState,
        delta: isize,
    ) -> SelectableListOutcome {
        scroll_outcome(ScrollView::scroll_vertical_by(
            viewport,
            &mut state.scroll,
            delta,
        ))
    }

    fn move_focus(
        &self,
        viewport: &LayoutNode,
        state: &mut SelectableListState,
        direction: Direction,
    ) -> SelectableListOutcome {
        let current = state
            .focused
            .or(state.selected)
            .or_else(|| self.items.iter().position(|item| !item.disabled));
        let Some(current) = current else {
            return SelectableListOutcome::Ignored;
        };
        let Some(next) = self.next_enabled(current, direction) else {
            return SelectableListOutcome::Ignored;
        };
        if next == current {
            return SelectableListOutcome::Ignored;
        }
        state.set_focused(Some(next));
        self.ensure_visible(viewport, state, next);
        SelectableListOutcome::Focused(next)
    }

    fn focus_edge(
        &self,
        viewport: &LayoutNode,
        state: &mut SelectableListState,
        first: bool,
    ) -> SelectableListOutcome {
        let next = if first {
            self.items.iter().position(|item| !item.disabled)
        } else {
            self.items.iter().rposition(|item| !item.disabled)
        };
        let Some(index) = next else {
            return SelectableListOutcome::Ignored;
        };
        if state.focused == Some(index) {
            SelectableListOutcome::Ignored
        } else {
            state.set_focused(Some(index));
            self.ensure_visible(viewport, state, index);
            SelectableListOutcome::Focused(index)
        }
    }

    fn select_focused(
        &self,
        viewport: &LayoutNode,
        state: &mut SelectableListState,
    ) -> SelectableListOutcome {
        let index = state
            .focused
            .or(state.selected)
            .or_else(|| self.items.iter().position(|item| !item.disabled));
        let Some(index) = index else {
            return SelectableListOutcome::Ignored;
        };
        self.select_index(viewport, state, index)
    }

    fn select_index(
        &self,
        viewport: &LayoutNode,
        state: &mut SelectableListState,
        index: usize,
    ) -> SelectableListOutcome {
        if !self.is_enabled_item(index) || state.selected == Some(index) {
            return SelectableListOutcome::Ignored;
        }
        state.selected = Some(index);
        state.set_focused(Some(index));
        self.ensure_visible(viewport, state, index);
        SelectableListOutcome::Selected(index)
    }

    fn ensure_visible(&self, viewport: &LayoutNode, state: &mut SelectableListState, index: usize) {
        if viewport.size.height == 0 {
            return;
        }
        let start = self.item_start(index);
        let height = self.items.get(index).map_or(0, SelectableListItem::height);
        self.scroll_view()
            .ensure_visible(viewport, &mut state.scroll, start, height);
    }

    fn item_start(&self, index: usize) -> usize {
        self.items
            .iter()
            .take(index)
            .map(SelectableListItem::height)
            .sum()
    }

    fn next_enabled(&self, current: usize, direction: Direction) -> Option<usize> {
        if self.items.is_empty() {
            return None;
        }
        let mut index = current;
        for _ in 0..self.items.len() {
            index = match direction {
                Direction::Previous if index == 0 && self.policy.keyboard.wrap => {
                    self.items.len().saturating_sub(1)
                }
                Direction::Previous if index == 0 => return Some(current),
                Direction::Previous => index.saturating_sub(1),
                Direction::Next if index.saturating_add(1) >= self.items.len() => {
                    if self.policy.keyboard.wrap {
                        0
                    } else {
                        return Some(current);
                    }
                }
                Direction::Next => index.saturating_add(1),
            };
            if self.is_enabled_item(index) {
                return Some(index);
            }
        }
        Some(current)
    }

    fn hit_index(
        &self,
        area: Rect,
        state: &SelectableListState,
        mouse: MouseEvent,
    ) -> Option<usize> {
        let regions = self.visible_hit_regions(area, state);
        let index = hit_region_at(&regions, mouse.position)?.key;
        if self.is_enabled_item(index) {
            Some(index)
        } else {
            None
        }
    }

    /// Visible item rectangles inside the content viewport `area` after
    /// applying the logical scroll offset. Rows above the offset are skipped
    /// exactly and the final visible item is clipped to the viewport bottom.
    fn visible_hit_regions(
        &self,
        area: Rect,
        state: &SelectableListState,
    ) -> Vec<HitRegion<usize>> {
        let offset = state.scroll.vertical_offset();
        let viewport_end = offset.saturating_add(usize::from(area.height));
        let mut start = 0usize;
        let mut regions = Vec::new();
        for (index, item) in self.items.iter().enumerate() {
            let end = start.saturating_add(item.height());
            if end <= offset {
                start = end;
                continue;
            }
            if start >= viewport_end {
                break;
            }
            let visible_start = start.max(offset);
            let visible_end = end.min(viewport_end);
            let height = visible_end.saturating_sub(visible_start);
            if height > 0 {
                regions.push(HitRegion::new(
                    index,
                    Rect::new(
                        area.x,
                        area.y
                            .saturating_add(u16_saturating(visible_start.saturating_sub(offset))),
                        area.width,
                        u16_saturating(height),
                    ),
                ));
            }
            start = end;
        }
        regions
    }

    fn total_height(&self) -> usize {
        self.items.iter().map(SelectableListItem::height).sum()
    }

    fn normalize_state(&self, state: &mut SelectableListState) {
        if state
            .selected
            .is_some_and(|index| index >= self.items.len())
        {
            state.selected = None;
        }
        if state.focused.is_some_and(|index| index >= self.items.len()) {
            state.set_focused(None);
        }
        if state.hovered.is_some_and(|index| index >= self.items.len()) {
            state.hovered = None;
        }
        if state.pressed.is_some_and(|index| index >= self.items.len()) {
            state.pressed = None;
        }
    }

    fn is_enabled_item(&self, index: usize) -> bool {
        self.items.get(index).is_some_and(|item| !item.disabled)
    }
}

/// Measured stacked item rows painted in content-local coordinates.
///
/// The rows are the single child of the shared scroll viewport, so the
/// viewport translation and clip decide which rows reach the buffer. Painting
/// is bounded to the rows that intersect the viewport.
struct ItemRows<'a, 'list> {
    list: &'list SelectableList<'a>,
    id: LayoutId,
    state: SelectableListState,
    fallback: Style,
    offset: usize,
    viewport_height: usize,
}

impl Component for ItemRows<'_, '_> {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        self.list
            .content_layout(self.id.clone(), constraints.max_width())
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let width = layout.size.width;
        let end = self.offset.saturating_add(self.viewport_height);
        let mut row = 0usize;
        for (index, item) in self.list.items.iter().enumerate() {
            if row >= end {
                break;
            }
            for line_index in 0..item.height() {
                if row >= self.offset && row < end {
                    cx.write_line_with_fallback_style(
                        LocalRect::new(0, i64::try_from(row).unwrap_or(i64::MAX), width, 1),
                        &self.list.line(index, item, line_index, self.state),
                        self.fallback,
                    );
                }
                row = row.saturating_add(1);
            }
        }
    }
}

/// Canonical component-lifecycle selectable list.
///
/// The component measures the exact stacked item height at the constrained
/// width, paints complete item rows and the integrated scrollbar through the
/// scoped paint context, and registers one composite roving-focus region plus
/// one visible item region per stable item id. Interaction state remains
/// caller-owned through an interior-mutable `Cell`.
pub struct SelectableListComponent<'a, 'state> {
    id: LayoutId,
    list: SelectableList<'a>,
    state: &'state Cell<SelectableListState>,
    fallback: Option<Style>,
}

impl<'a, 'state> SelectableListComponent<'a, 'state> {
    /// Create a selectable list with stable identity and caller-owned state.
    #[must_use]
    pub fn new(
        id: impl Into<LayoutId>,
        items: &'a [SelectableListItem],
        state: &'state Cell<SelectableListState>,
    ) -> Self {
        Self {
            id: id.into(),
            list: SelectableList::new(items),
            state,
            fallback: None,
        }
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: SelectableListPolicy) -> Self {
        self.list.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: SelectableListStyles) -> Self {
        self.list.styles = styles;
        self
    }

    /// Override the row fill style; defaults to the configured background style.
    #[must_use]
    pub const fn fallback_style(mut self, fallback: Style) -> Self {
        self.fallback = Some(fallback);
        self
    }

    /// Stable identity of the shared scroll viewport.
    #[must_use]
    pub fn viewport_id(&self) -> LayoutId {
        viewport_id(&self.id)
    }

    /// Stable identity of the measured item content.
    #[must_use]
    pub fn content_id(&self) -> LayoutId {
        content_id(&self.id)
    }

    /// Stable semantic identifier for one contained item.
    fn item_id(&self, item: &SelectableListItem) -> String {
        format!("{}.{}", self.id.as_str(), item.id)
    }
}

impl Component for SelectableListComponent<'_, '_> {
    fn revision(&self) -> ComponentRevision {
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        for item in self.list.items {
            item.id.hash(&mut layout);
            item.lines.len().hash(&mut layout);
            for line in &item.lines {
                format!("{line:?}").hash(&mut layout);
            }
        }
        self.list.policy.highlight.symbol.hash(&mut layout);
        self.list.policy.highlight.repeat_spacing.hash(&mut layout);
        self.list.policy.scrollbar.hash(&mut layout);

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        for item in self.list.items {
            item.disabled.hash(&mut paint);
        }
        format!("{:?}", self.list.styles).hash(&mut paint);
        format!("{:?}", self.fallback).hash(&mut paint);
        format!("{:?}", self.state.get()).hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        self.list.layout_with(&self.id, constraints)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let area = local_area_of(layout.size);
        if area.is_empty() {
            return;
        }
        let state = self.state.get();
        cx.push_hit(
            SceneRegion::new(self.id.as_str(), area)
                .role(HitRole::ListItem)
                .hoverable(self.list.policy.mouse.hover)
                .focusable(true)
                .enabled(!state.interaction.disabled),
        );
        let content_area = self.list.content_area(area);
        for region in self.list.visible_hit_regions(content_area, &state) {
            let Some(item) = self.list.items.get(region.key) else {
                continue;
            };
            let item_id = self.item_id(item);
            cx.push_hit(
                SceneRegion::new(item_id.clone(), region.rect)
                    .role(HitRole::ListItem)
                    .hoverable(self.list.policy.mouse.hover)
                    .enabled(!state.interaction.disabled && !item.disabled),
            );
            cx.push_semantic(SemanticRegion::new(item_id, region.rect, "list-item"));
        }
        self.list.paint_layout(
            layout,
            area,
            &state,
            self.fallback.unwrap_or(self.list.styles.background),
            cx,
        );
        cx.push_semantic(SemanticRegion::new(self.id.as_str(), area, "list"));
        cx.push_damage(LocalRect::new(0, 0, area.width, area.height));
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        let Some(area) = cx.find_rect(&layout.id) else {
            return EventOutcome::Ignored;
        };
        let mut state = self.state.get();
        let outcome = self
            .list
            .handle_event_with_layout(layout, area, &mut state, event);
        self.state.set(state);
        match outcome {
            SelectableListOutcome::Ignored => EventOutcome::Ignored,
            SelectableListOutcome::Redraw
            | SelectableListOutcome::Focused(_)
            | SelectableListOutcome::Selected(_) => EventOutcome::Redraw,
        }
    }
}

/// Resolve the shared scroll viewport node inside a list layout.
fn resolved_viewport(layout: &LayoutNode) -> Option<&LayoutNode> {
    layout.children.first().map(|child| &child.node)
}

/// Identity of the measured content already resolved inside a viewport node.
fn content_id_of(viewport: &LayoutNode) -> LayoutId {
    viewport.children.first().map_or_else(
        || LayoutId::new("list.content"),
        |child| child.node.id.clone(),
    )
}

fn viewport_id(id: &LayoutId) -> LayoutId {
    LayoutId::new(format!("{}.viewport", id.as_str()))
}

fn content_id(id: &LayoutId) -> LayoutId {
    LayoutId::new(format!("{}.content", id.as_str()))
}

const fn scroll_outcome(outcome: ScrollViewOutcome) -> SelectableListOutcome {
    match outcome {
        ScrollViewOutcome::Ignored => SelectableListOutcome::Ignored,
        ScrollViewOutcome::Scrolled { .. } | ScrollViewOutcome::HorizontalScrolled { .. } => {
            SelectableListOutcome::Redraw
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Previous,
    Next,
}

impl crate::theme::ComponentTheme {
    /// Convert this semantic component theme into [`SelectableListStyles`].
    #[must_use]
    pub fn selectable_list_styles(self) -> SelectableListStyles {
        SelectableListStyles::from(self)
    }
}

impl From<crate::theme::ComponentTheme> for SelectableListStyles {
    fn from(theme: crate::theme::ComponentTheme) -> Self {
        let theme = theme.for_surface(crate::theme::ComponentSurfaceDepth::Normal);
        Self {
            background: theme.surfaces.normal,
            scrollbar: theme.scrollbar_styles(),
            normal: theme.text,
            focused: theme.focused,
            selected: theme.selected,
            hovered: theme.info,
            pressed: theme.selected.add_modifier(bmux_tui::style::Modifier::BOLD),
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
    use bmux_tui::geometry::{Point, Rect};
    use bmux_tui::hit::HitRole;
    use bmux_tui::paint::{LocalRect, PaintCx};
    use bmux_tui::prelude::{Line, Span, Style};

    use crate::scrollbar_layout::ScrollbarAxisLayoutMode;

    use super::{
        SelectableList, SelectableListComponent, SelectableListHighlightPolicy, SelectableListItem,
        SelectableListOutcome, SelectableListPolicy, SelectableListState, SelectableListStyles,
    };

    trait SelectableListTestRender {
        fn render(&self, area: Rect, state: &SelectableListState, frame: &mut Frame<'_>);
    }

    impl SelectableListTestRender for SelectableList<'_> {
        fn render(&self, area: Rect, state: &SelectableListState, frame: &mut Frame<'_>) {
            let state = Cell::new(*state);
            let component = SelectableListComponent {
                id: "test.list".into(),
                list: *self,
                state: &state,
                fallback: None,
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
    fn component_measures_exact_stacked_height_and_registers_geometry() {
        let items = [
            SelectableListItem::new("one", "One"),
            SelectableListItem::multiline("two", [Line::from("Two A"), Line::from("Two B")]),
            SelectableListItem::new("three", "Three").disabled(true),
        ];
        let state = Cell::new(SelectableListState::new(Some(0)));
        let component = SelectableListComponent::new("nav.list", &items, &state);

        let mut cx = LayoutCx::new();
        let layout = component.layout(Constraints::for_width(12), &mut cx);
        assert_eq!(layout.size.width, 12);
        assert_eq!(layout.size.height, 4);
        assert_eq!(cx.measured_nodes(), 1);

        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 6));
        let mut frame = Frame::new(&mut buffer);
        let layout = component.layout(Constraints::tight(Rect::new(0, 0, 12, 3).size()), &mut cx);
        PaintCx::new(&mut frame).with_child(3, 1, LocalRect::new(0, 0, 12, 3), |cx| {
            component.paint(&layout, cx);
        });

        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("   > One            ")
        );
        let regions = frame.hits().regions();
        assert_eq!(regions[0].id.as_str(), "nav.list");
        assert_eq!(regions[0].area, Rect::new(3, 1, 12, 3));
        assert_eq!(regions[0].role, HitRole::ListItem);
        assert!(regions[0].focusable);
        assert_eq!(regions[1].id.as_str(), "nav.list.one");
        assert_eq!(regions[1].area, Rect::new(3, 1, 12, 1));
        assert!(!regions[1].focusable);
        assert_eq!(regions[2].id.as_str(), "nav.list.two");
        assert_eq!(regions[2].area, Rect::new(3, 2, 12, 2));
        assert_eq!(regions.len(), 3, "the clipped third item must not register");
        assert_eq!(frame.hits().focus_targets(None).len(), 1);
        let semantics = frame.semantics().regions();
        assert!(
            semantics
                .iter()
                .any(|region| region.id == "nav.list.two" && region.role == "list-item")
        );
        assert!(
            semantics
                .iter()
                .any(|region| region.id == "nav.list" && region.role == "list")
        );
    }

    #[test]
    fn component_routes_events_through_resolved_layout_and_updates_caller_state() {
        let items = [
            SelectableListItem::new("one", "One"),
            SelectableListItem::new("two", "Two"),
        ];
        let state = Cell::new(SelectableListState::new(Some(0)));
        let component = SelectableListComponent::new("nav.list", &items, &state);
        let layout = component.layout(
            Constraints::tight(Rect::new(0, 0, 12, 2).size()),
            &mut LayoutCx::new(),
        );

        let outcome = component.event(
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(1, 1),
            )),
            &layout,
            &mut EventCx::new(&layout),
        );
        assert_eq!(outcome, EventOutcome::Redraw);
        let outcome = component.event(
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Up(MouseButton::Left),
                Point::new(1, 1),
            )),
            &layout,
            &mut EventCx::new(&layout),
        );
        assert_eq!(outcome, EventOutcome::Redraw);
        assert_eq!(state.get().selected(), Some(1));

        let ignored = component.event(&Event::Tick, &layout, &mut EventCx::new(&layout));
        assert_eq!(ignored, EventOutcome::Ignored);
    }

    #[test]
    fn component_revision_separates_layout_and_paint_changes() {
        let items = [SelectableListItem::new("one", "One")];
        let state = Cell::new(SelectableListState::new(None));
        let component = SelectableListComponent::new("nav.list", &items, &state);
        let before = component.revision();

        state.set(SelectableListState::new(Some(0)));
        let paint_only = component.revision();
        assert_eq!(before.layout, paint_only.layout);
        assert_ne!(before.paint, paint_only.paint);

        let taller = [SelectableListItem::multiline(
            "one",
            [Line::from("One"), Line::from("More")],
        )];
        let relayout = SelectableListComponent::new("nav.list", &taller, &state).revision();
        assert_ne!(before.layout, relayout.layout);
    }

    #[test]
    fn component_paint_is_clipped_by_parent_scope() {
        let items = [
            SelectableListItem::new("one", "One"),
            SelectableListItem::new("two", "Two"),
            SelectableListItem::new("three", "Three"),
        ];
        let state = Cell::new(SelectableListState::new(None));
        let component = SelectableListComponent::new("nav.list", &items, &state);
        let layout = component.layout(Constraints::for_width(8), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 2));
        let mut frame = Frame::new(&mut buffer);
        PaintCx::new(&mut frame).with_child(0, -1, LocalRect::new(0, 1, 8, 2), |cx| {
            component.paint(&layout, cx);
        });

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("  Two   "));
        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("  Three "));
        let regions = frame.hits().regions();
        assert!(regions.iter().all(|region| region.area.y < 2));
        assert!(
            regions
                .iter()
                .all(|region| region.id.as_str() != "nav.list.one")
        );
    }

    #[test]
    fn opaque_theme_fills_complete_list_surface_and_scrollbar_gutter() {
        let items = [SelectableListItem::new("one", "One")];
        let theme = crate::theme::ComponentTheme::opaque_dark();
        let list = SelectableList::new(&items)
            .policy(SelectableListPolicy::interactive().scrollbar(ScrollbarAxisLayoutMode::Gutter))
            .styles(theme.selectable_list_styles());
        let area = Rect::new(0, 0, 12, 4);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        list.render(area, &SelectableListState::new(Some(0)), &mut frame);

        for cell in frame.buffer().cells() {
            assert!(
                cell.style
                    .bg
                    .is_some_and(|background| background != bmux_tui::style::Color::Default),
                "selectable-list left an unpainted cell: {:?}",
                cell.symbol
            );
        }
    }

    #[test]
    fn renders_rich_line_items_preserving_span_style() {
        let item_style = Style::new().fg(bmux_tui::style::Color::Yellow);
        let items = [SelectableListItem::rich(
            "rich",
            Line::from_spans([Span::raw("plain "), Span::styled("rich", item_style)]),
        )];
        let state = SelectableListState::new(None);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 14, 1));
        let mut frame = Frame::new(&mut buffer);

        SelectableList::new(&items).render(Rect::new(0, 0, 14, 1), &state, &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("  plain rich  ")
        );
        assert_eq!(
            frame.buffer().get(Point::new(8, 0)).map(|cell| cell.style),
            Some(SelectableListStyles::default().normal.patch(item_style))
        );
    }

    #[test]
    fn tiny_area_does_not_panic() {
        let items = [SelectableListItem::multiline(
            "tiny",
            [Line::from("first"), Line::from("second")],
        )];
        let list = SelectableList::new(&items);
        let state = SelectableListState::new(Some(0));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 0, 0));
        let mut frame = Frame::new(&mut buffer);

        list.render(Rect::new(0, 0, 0, 0), &state, &mut frame);

        assert_eq!(list.size(), (10, 2));
    }

    #[test]
    fn viewport_scrolls_rendered_rows_and_hit_tests_visible_rows() {
        let items = [
            SelectableListItem::new("one", "One"),
            SelectableListItem::multiline("two", [Line::from("Two A"), Line::from("Two B")]),
            SelectableListItem::new("three", "Three"),
        ];
        let list = SelectableList::new(&items);
        let mut state = SelectableListState::new(None);
        state.set_vertical_scroll(1);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 2));
        let mut frame = Frame::new(&mut buffer);

        list.render(Rect::new(0, 0, 12, 2), &state, &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("  Two A     ")
        );
        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("  Two B     ")
        );
        assert_eq!(list.max_vertical_scroll(Rect::new(0, 0, 12, 2)), 2);

        let outcome = list.handle_event(
            Rect::new(0, 0, 12, 2),
            &mut state,
            &Event::Mouse(MouseEvent::new(MouseEventKind::Move, Point::new(1, 1))),
        );
        assert_eq!(outcome, SelectableListOutcome::Redraw);
        assert_eq!(state.hovered, Some(1));
    }

    #[test]
    fn page_keys_update_vertical_scroll() {
        let items = [
            SelectableListItem::new("one", "One"),
            SelectableListItem::new("two", "Two"),
            SelectableListItem::new("three", "Three"),
        ];
        let list = SelectableList::new(&items);
        let mut state = SelectableListState::new(Some(0));

        assert_eq!(
            list.handle_event(
                Rect::new(0, 0, 12, 1),
                &mut state,
                &Event::Key(KeyStroke::simple(KeyCode::PageDown)),
            ),
            SelectableListOutcome::Redraw
        );
        assert_eq!(state.vertical_scroll(), 1);
    }

    #[test]
    fn exposes_visible_semantic_regions_by_stable_item_id() {
        let items = [
            SelectableListItem::new("one", "One"),
            SelectableListItem::multiline("two", [Line::from("Two"), Line::from("Details")]),
        ];
        let list = SelectableList::new(&items);
        let state = SelectableListState::new(Some(0));

        let regions = list.visible_semantic_regions(Rect::new(2, 3, 10, 3), &state);

        assert_eq!(regions[0].key, "one");
        assert_eq!(regions[0].rect, Rect::new(2, 3, 10, 1));
        assert_eq!(regions[1].key, "two");
        assert_eq!(regions[1].rect, Rect::new(2, 4, 10, 2));
        assert_eq!(
            list.semantic_id_at(Rect::new(2, 3, 10, 3), &state, Point::new(3, 5)),
            Some("two")
        );
    }

    #[test]
    fn renders_integrated_scrollbar() {
        let items = [
            SelectableListItem::new("one", "One"),
            SelectableListItem::new("two", "Two"),
            SelectableListItem::new("three", "Three"),
        ];
        let list = SelectableList::new(&items)
            .policy(SelectableListPolicy::interactive().scrollbar(ScrollbarAxisLayoutMode::Gutter));
        let mut state = SelectableListState::new(Some(0));
        state.set_vertical_scroll(1);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 2));
        let mut frame = Frame::new(&mut buffer);

        list.render(Rect::new(0, 0, 6, 2), &state, &mut frame);

        assert_eq!(frame.buffer().row_symbols(1).as_deref(), Some("  Thr█"));
    }

    #[test]
    fn scrollbar_mouse_updates_vertical_scroll() {
        let items = [
            SelectableListItem::new("one", "One"),
            SelectableListItem::new("two", "Two"),
            SelectableListItem::new("three", "Three"),
        ];
        let list = SelectableList::new(&items)
            .policy(SelectableListPolicy::interactive().scrollbar(ScrollbarAxisLayoutMode::Gutter));
        let mut state = SelectableListState::new(Some(0));

        let outcome = list.handle_event(
            Rect::new(0, 0, 6, 2),
            &mut state,
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(5, 1),
            )),
        );

        assert_eq!(outcome, SelectableListOutcome::Redraw);
        assert!(state.vertical_scroll() > 0);
    }

    #[test]
    fn renders_multiline_items_and_hit_tests_full_item_height() {
        let items = [
            SelectableListItem::multiline("rich", [Line::from("first"), Line::from("second")]),
            SelectableListItem::new("next", "Next"),
        ];
        let list = SelectableList::new(&items);
        let mut state = SelectableListState::new(None);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 3));
        let mut frame = Frame::new(&mut buffer);

        list.render(Rect::new(0, 0, 12, 3), &state, &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("  first     ")
        );
        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("  second    ")
        );
        assert_eq!(
            frame.buffer().row_symbols(2).as_deref(),
            Some("  Next      ")
        );

        let outcome = list.handle_event(
            Rect::new(0, 0, 12, 3),
            &mut state,
            &Event::Mouse(MouseEvent::new(MouseEventKind::Move, Point::new(1, 1))),
        );
        assert_eq!(outcome, SelectableListOutcome::Redraw);
        assert_eq!(state.hovered, Some(0));
    }

    #[test]
    fn custom_highlight_symbol_and_spacing_policy_are_rendered() {
        let items = vec![
            SelectableListItem::new("draft", "Draft"),
            SelectableListItem::new("published", "Published"),
        ];
        let list = SelectableList::new(&items).policy(SelectableListPolicy {
            highlight: SelectableListHighlightPolicy::new("»", false),
            ..SelectableListPolicy::default()
        });
        let mut buffer = Buffer::empty(Rect::new(0, 0, 14, 2));
        let mut frame = Frame::new(&mut buffer);

        list.render(
            Rect::new(0, 0, 14, 2),
            &SelectableListState::new(Some(1)),
            &mut frame,
        );

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("Draft         ")
        );
        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("» Published   ")
        );
    }

    #[test]
    fn keyboard_navigation_keeps_focused_item_visible() {
        let items = [
            SelectableListItem::new("one", "One"),
            SelectableListItem::new("two", "Two"),
            SelectableListItem::new("three", "Three"),
        ];
        let list = SelectableList::new(&items);
        let mut state = SelectableListState::new(Some(0));

        assert_eq!(
            list.handle_event(
                Rect::new(0, 0, 12, 1),
                &mut state,
                &Event::Key(KeyStroke::simple(KeyCode::Down)),
            ),
            SelectableListOutcome::Focused(1)
        );
        assert_eq!(state.vertical_scroll(), 1);

        assert_eq!(
            list.handle_event(
                Rect::new(0, 0, 12, 1),
                &mut state,
                &Event::Key(KeyStroke::simple(KeyCode::Up)),
            ),
            SelectableListOutcome::Focused(0)
        );
        assert_eq!(state.vertical_scroll(), 0);
    }

    #[test]
    fn selection_visibility_respects_multiline_item_heights() {
        let items = [
            SelectableListItem::multiline("one", [Line::from("one-a"), Line::from("one-b")]),
            SelectableListItem::multiline("two", [Line::from("two-a"), Line::from("two-b")]),
        ];
        let list = SelectableList::new(&items);
        let mut state = SelectableListState::new(Some(0));

        assert_eq!(
            list.handle_event(
                Rect::new(0, 0, 12, 2),
                &mut state,
                &Event::Key(KeyStroke::simple(KeyCode::Down)),
            ),
            SelectableListOutcome::Focused(1)
        );
        assert_eq!(state.vertical_scroll(), 2);
    }

    #[test]
    fn scroll_clamps_when_item_count_shrinks() {
        let many = [
            SelectableListItem::new("one", "One"),
            SelectableListItem::new("two", "Two"),
            SelectableListItem::new("three", "Three"),
        ];
        let few = [SelectableListItem::new("one", "One")];
        let mut state = SelectableListState::new(Some(0));
        state.set_vertical_scroll(2);

        SelectableList::new(&many).clamp_state(Rect::new(0, 0, 12, 1), &mut state);
        assert_eq!(state.vertical_scroll(), 2);
        SelectableList::new(&few).clamp_state(Rect::new(0, 0, 12, 1), &mut state);

        assert_eq!(state.vertical_scroll(), 0);
    }

    #[test]
    fn renders_selected_item() {
        let items = vec![
            SelectableListItem::new("draft", "Draft"),
            SelectableListItem::new("published", "Published"),
        ];
        let list = SelectableList::new(&items);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 14, 2));
        let mut frame = Frame::new(&mut buffer);

        list.render(
            Rect::new(0, 0, 14, 2),
            &SelectableListState::new(Some(1)),
            &mut frame,
        );

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("  Draft       ")
        );
        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("> Published   ")
        );
    }

    #[test]
    fn arrow_key_moves_focus_to_next_enabled_item() {
        let items = vec![
            SelectableListItem::new("draft", "Draft"),
            SelectableListItem::new("review", "Review").disabled(true),
            SelectableListItem::new("published", "Published"),
        ];
        let list = SelectableList::new(&items);
        let mut state = SelectableListState::new(Some(0));
        state.set_focused(Some(0));

        let outcome = list.handle_event(
            Rect::new(0, 0, 14, 3),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Down)),
        );

        assert_eq!(outcome, SelectableListOutcome::Focused(2));
        assert_eq!(state.focused(), Some(2));
    }

    #[test]
    fn focused_enter_selects_item() {
        let items = vec![
            SelectableListItem::new("draft", "Draft"),
            SelectableListItem::new("published", "Published"),
        ];
        let list = SelectableList::new(&items);
        let mut state = SelectableListState::new(Some(0));
        state.set_focused(Some(1));

        let outcome = list.handle_event(
            Rect::new(0, 0, 14, 2),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Enter)),
        );

        assert_eq!(outcome, SelectableListOutcome::Selected(1));
        assert_eq!(state.selected(), Some(1));
    }

    #[test]
    fn mouse_click_focuses_and_selects_item() {
        let items = vec![
            SelectableListItem::new("draft", "Draft"),
            SelectableListItem::new("published", "Published"),
        ];
        let list = SelectableList::new(&items);
        let mut state = SelectableListState::new(Some(0));
        let area = Rect::new(0, 0, 14, 2);

        let down = list.handle_event(
            area,
            &mut state,
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(1, 1),
            )),
        );
        let up = list.handle_event(
            area,
            &mut state,
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Up(MouseButton::Left),
                Point::new(1, 1),
            )),
        );

        assert_eq!(down, SelectableListOutcome::Redraw);
        assert_eq!(up, SelectableListOutcome::Selected(1));
        assert_eq!(state.focused(), Some(1));
        assert_eq!(state.selected(), Some(1));
    }

    #[test]
    fn disabled_list_ignores_events() {
        let items = vec![SelectableListItem::new("draft", "Draft")];
        let list = SelectableList::new(&items);
        let mut state = SelectableListState::new(Some(0));
        state.set_disabled(true);
        state.set_focused(Some(0));

        let outcome = list.handle_event(
            Rect::new(0, 0, 14, 1),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Enter)),
        );

        assert_eq!(outcome, SelectableListOutcome::Ignored);
        assert_eq!(state.selected(), Some(0));
    }
}
