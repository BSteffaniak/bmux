//! Generic hierarchical tree-view component.

use std::cell::RefCell;
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

use crate::common::{ComponentMousePolicy, InteractionState, u16_saturating};
use crate::hit_test::{HitRegion, hit_region_at, vertical_hit_regions};

/// One tree item in caller-provided preorder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeViewItem {
    /// Stable item id.
    pub id: String,
    /// Visible label.
    pub label: String,
    /// Nesting depth, where zero is root level.
    pub depth: u16,
    /// Whether this item can be expanded/collapsed.
    pub expandable: bool,
    /// Whether this item ignores selection/activation.
    pub disabled: bool,
}

impl TreeViewItem {
    /// Create a tree item.
    #[must_use]
    pub fn new(id: impl Into<String>, label: impl Into<String>, depth: u16) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            depth,
            expandable: false,
            disabled: false,
        }
    }

    /// Return this item marked expandable.
    #[must_use]
    pub const fn expandable(mut self, expandable: bool) -> Self {
        self.expandable = expandable;
        self
    }

    /// Return this item with disabled state set.
    #[must_use]
    pub const fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

/// Keyboard behavior for [`TreeView`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeViewKeyboardPolicy {
    /// Whether keyboard events are accepted.
    pub enabled: bool,
    /// Whether Enter selects the focused item.
    pub enter_selects: bool,
    /// Whether Space toggles expansion.
    pub space_toggles: bool,
}

impl TreeViewKeyboardPolicy {
    /// Keyboard behavior disabled.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            enter_selects: false,
            space_toggles: false,
        }
    }

    /// Standard tree navigation behavior.
    #[must_use]
    pub const fn navigation() -> Self {
        Self {
            enabled: true,
            enter_selects: true,
            space_toggles: true,
        }
    }
}

impl Default for TreeViewKeyboardPolicy {
    fn default() -> Self {
        Self::navigation()
    }
}

/// Behavior policy for [`TreeView`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeViewPolicy {
    /// Keyboard behavior.
    pub keyboard: TreeViewKeyboardPolicy,
    /// Mouse behavior.
    pub mouse: ComponentMousePolicy,
    /// Number of spaces per depth level.
    pub indent_width: u16,
}

impl TreeViewPolicy {
    /// Render-only policy.
    #[must_use]
    pub const fn bare() -> Self {
        Self {
            keyboard: TreeViewKeyboardPolicy::disabled(),
            mouse: ComponentMousePolicy::disabled(),
            indent_width: 2,
        }
    }

    /// Interactive keyboard/mouse policy.
    #[must_use]
    pub const fn interactive() -> Self {
        Self {
            keyboard: TreeViewKeyboardPolicy::navigation(),
            mouse: ComponentMousePolicy::button(),
            indent_width: 2,
        }
    }
}

impl Default for TreeViewPolicy {
    fn default() -> Self {
        Self::interactive()
    }
}

/// Visual styles for [`TreeView`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeViewStyles {
    /// Normal row style.
    pub normal: Style,
    /// Selected row style.
    pub selected: Style,
    /// Hovered row style.
    pub hovered: Style,
    /// Pressed row style.
    pub pressed: Style,
    /// Disabled row style.
    pub disabled: Style,
    /// Disclosure marker style.
    pub marker: Style,
}

impl Default for TreeViewStyles {
    fn default() -> Self {
        Self {
            normal: Style::new().fg(Color::White),
            selected: Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            hovered: Style::new().fg(Color::BrightWhite),
            pressed: Style::new().fg(Color::Black).bg(Color::BrightCyan),
            disabled: Style::new().fg(Color::BrightBlack),
            marker: Style::new().fg(Color::BrightBlack),
        }
    }
}

/// Runtime state for [`TreeView`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TreeViewState {
    selected_visible: Option<usize>,
    expanded: Vec<String>,
    hovered_visible: Option<usize>,
    pressed_visible: Option<usize>,
    /// Generic interaction flags.
    pub interaction: InteractionState,
}

impl TreeViewState {
    /// Create tree-view state with a selected visible row.
    #[must_use]
    pub const fn new(selected_visible: Option<usize>) -> Self {
        Self {
            selected_visible,
            expanded: Vec::new(),
            hovered_visible: None,
            pressed_visible: None,
            interaction: InteractionState::new(),
        }
    }

    /// Set whether this composite currently owns keyboard focus.
    pub const fn set_focused(&mut self, focused: bool) {
        self.interaction.focused = focused;
    }

    /// Set whether the whole tree is disabled.
    pub const fn set_disabled(&mut self, disabled: bool) {
        self.interaction.disabled = disabled;
        if disabled {
            self.hovered_visible = None;
            self.pressed_visible = None;
        }
    }

    /// Return selected visible row.
    #[must_use]
    pub const fn selected_visible(&self) -> Option<usize> {
        self.selected_visible
    }

    /// Return expanded ids.
    #[must_use]
    pub fn expanded_ids(&self) -> &[String] {
        &self.expanded
    }

    /// Set selected visible row.
    pub const fn set_selected_visible(&mut self, selected_visible: Option<usize>) {
        self.selected_visible = selected_visible;
    }

    /// Return whether `id` is expanded.
    #[must_use]
    pub fn is_expanded(&self, id: &str) -> bool {
        self.expanded.iter().any(|expanded| expanded == id)
    }

    /// Set expansion for an id.
    pub fn set_expanded(&mut self, id: &str, expanded: bool) {
        if expanded {
            if !self.is_expanded(id) {
                self.expanded.push(id.to_owned());
            }
        } else {
            self.expanded.retain(|expanded| expanded != id);
        }
    }

    /// Toggle expansion for an id and return true when the state changed.
    pub fn toggle_expanded(&mut self, id: &str) -> bool {
        let expanded = self.is_expanded(id);
        self.set_expanded(id, !expanded);
        true
    }
}

/// Tree-view input outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeViewOutcome {
    /// Event was ignored.
    Ignored,
    /// Visual state changed.
    Redraw,
    /// Focus moved to visible row and source item index.
    Focused { visible: usize, source: usize },
    /// Item was selected by source index.
    Selected { visible: usize, source: usize },
    /// Item expansion changed.
    Toggled {
        visible: usize,
        source: usize,
        expanded: bool,
    },
}

/// Generic hierarchical tree view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeView<'a> {
    items: &'a [TreeViewItem],
    policy: TreeViewPolicy,
    styles: TreeViewStyles,
}

impl<'a> TreeView<'a> {
    /// Create a tree view over caller-owned preorder items.
    #[must_use]
    pub const fn new(items: &'a [TreeViewItem]) -> Self {
        Self {
            items,
            policy: TreeViewPolicy {
                keyboard: TreeViewKeyboardPolicy {
                    enabled: true,
                    enter_selects: true,
                    space_toggles: true,
                },
                mouse: ComponentMousePolicy {
                    enabled: true,
                    hover: true,
                    click: true,
                },
                indent_width: 2,
            },
            styles: TreeViewStyles {
                normal: Style::new(),
                selected: Style::new(),
                hovered: Style::new(),
                pressed: Style::new(),
                disabled: Style::new(),
                marker: Style::new(),
            },
        }
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: TreeViewPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: TreeViewStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Return source indices visible under the current expansion state.
    #[must_use]
    pub fn visible_indices(&self, state: &TreeViewState) -> Vec<usize> {
        visible_indices(self.items, state)
    }

    /// Return the natural size: the widest visible row and one row per
    /// visible item under the current expansion state.
    #[must_use]
    pub fn size(&self, state: &TreeViewState) -> (u16, u16) {
        let visible = self.visible_indices(state);
        let width = visible
            .iter()
            .enumerate()
            .map(|(row, source)| self.row_line(&self.items[*source], state, row).width())
            .max()
            .unwrap_or(0);
        (u16_saturating(width), u16_saturating(visible.len()))
    }

    /// Whether this tree currently exposes any interactive row.
    fn is_interactive(&self, state: &TreeViewState) -> bool {
        let usable = self
            .visible_indices(state)
            .into_iter()
            .any(|source| !self.items[source].disabled);
        usable && (self.policy.keyboard.enabled || self.policy.mouse.enabled)
    }

    /// Paint visible tree rows through a scoped local-coordinate context whose
    /// origin is this tree's top-left corner.
    pub fn paint(&self, area: Rect, state: &TreeViewState, cx: &mut PaintCx<'_, '_>) {
        if area.is_empty() {
            return;
        }
        for (visible, source) in self
            .visible_indices(state)
            .into_iter()
            .take(usize::from(area.height))
            .enumerate()
        {
            let item = &self.items[source];
            let row = LocalRect::new(
                i32::from(area.x),
                i64::from(area.y.saturating_add(u16_saturating(visible))),
                area.width,
                1,
            );
            cx.write_line_with_fallback_style(
                row,
                &self.row_line(item, state, visible),
                self.row_style(item, state, visible),
            );
        }
    }

    /// Handle one event.
    pub fn handle_event(
        &self,
        area: Rect,
        state: &mut TreeViewState,
        event: &Event,
    ) -> TreeViewOutcome {
        if state.interaction.disabled {
            return TreeViewOutcome::Ignored;
        }
        match event {
            Event::Key(stroke) if self.policy.keyboard.enabled && stroke.modifiers.is_empty() => {
                match stroke.key {
                    KeyCode::Up => self.move_selection(state, -1),
                    KeyCode::Down => self.move_selection(state, 1),
                    KeyCode::Left => self.collapse_selected(state),
                    KeyCode::Right => self.expand_selected(state),
                    KeyCode::Enter if self.policy.keyboard.enter_selects => {
                        self.select_selected(state)
                    }
                    KeyCode::Char(' ') if self.policy.keyboard.space_toggles => {
                        self.toggle_selected(state)
                    }
                    _ => TreeViewOutcome::Ignored,
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
            | Event::User(_) => TreeViewOutcome::Ignored,
        }
    }

    fn row_line(&self, item: &TreeViewItem, state: &TreeViewState, visible: usize) -> Line {
        let indent = " ".repeat(usize::from(
            item.depth.saturating_mul(self.policy.indent_width),
        ));
        let marker = if item.expandable {
            if state.is_expanded(&item.id) {
                "▾"
            } else {
                "▸"
            }
        } else {
            " "
        };
        let style = self.row_style(item, state, visible);
        Line::from_spans([
            Span::styled(indent, style),
            Span::styled(marker, self.styles.marker),
            Span::styled(" ", style),
            Span::styled(item.label.clone(), style),
        ])
    }

    fn row_style(&self, item: &TreeViewItem, state: &TreeViewState, visible: usize) -> Style {
        if item.disabled {
            self.styles.disabled
        } else if state.pressed_visible == Some(visible) {
            self.styles.pressed
        } else if state.selected_visible == Some(visible) {
            self.styles.selected
        } else if state.hovered_visible == Some(visible) {
            self.styles.hovered
        } else {
            self.styles.normal
        }
    }

    fn handle_mouse(
        &self,
        area: Rect,
        state: &mut TreeViewState,
        mouse: MouseEvent,
    ) -> TreeViewOutcome {
        match mouse.kind {
            MouseEventKind::Move if self.policy.mouse.hover => {
                let hovered = self.visible_at(area, state, mouse.position);
                if hovered == state.hovered_visible {
                    TreeViewOutcome::Ignored
                } else {
                    state.hovered_visible = hovered;
                    TreeViewOutcome::Redraw
                }
            }
            MouseEventKind::Down(MouseButton::Left) if self.policy.mouse.click => {
                state.pressed_visible = self.visible_at(area, state, mouse.position);
                if state.pressed_visible.is_some() {
                    TreeViewOutcome::Redraw
                } else {
                    TreeViewOutcome::Ignored
                }
            }
            MouseEventKind::Up(MouseButton::Left) if self.policy.mouse.click => {
                let hit = self.visible_at(area, state, mouse.position);
                let pressed = state.pressed_visible.take();
                if let (Some(pressed), Some(hit)) = (pressed, hit)
                    && pressed == hit
                {
                    state.selected_visible = Some(hit);
                    if let Some(source) = self.visible_indices(state).get(hit).copied() {
                        if self.items[source].disabled {
                            return TreeViewOutcome::Redraw;
                        }
                        return TreeViewOutcome::Selected {
                            visible: hit,
                            source,
                        };
                    }
                }
                TreeViewOutcome::Redraw
            }
            MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::Move
            | MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => TreeViewOutcome::Ignored,
        }
    }

    fn visible_hit_regions(&self, area: Rect, state: &TreeViewState) -> Vec<HitRegion<usize>> {
        vertical_hit_regions(area, 0, self.visible_indices(state).iter().map(|_| 1))
    }

    fn visible_at(&self, area: Rect, state: &TreeViewState, position: Point) -> Option<usize> {
        hit_region_at(&self.visible_hit_regions(area, state), position).map(|region| region.key)
    }

    fn move_selection(&self, state: &mut TreeViewState, delta: i32) -> TreeViewOutcome {
        let visible = self.visible_indices(state);
        if visible.is_empty() {
            return TreeViewOutcome::Ignored;
        }
        let current = state
            .selected_visible
            .unwrap_or(0)
            .min(visible.len().saturating_sub(1));
        let next = if delta.is_negative() {
            current.saturating_sub(1)
        } else {
            current
                .saturating_add(1)
                .min(visible.len().saturating_sub(1))
        };
        if next == current {
            return TreeViewOutcome::Ignored;
        }
        state.selected_visible = Some(next);
        TreeViewOutcome::Focused {
            visible: next,
            source: visible[next],
        }
    }

    fn selected_source(&self, state: &TreeViewState) -> Option<(usize, usize)> {
        let visible = state.selected_visible?;
        let source = self.visible_indices(state).get(visible).copied()?;
        Some((visible, source))
    }

    fn expand_selected(&self, state: &mut TreeViewState) -> TreeViewOutcome {
        let Some((visible, source)) = self.selected_source(state) else {
            return TreeViewOutcome::Ignored;
        };
        let item = &self.items[source];
        if !item.expandable || state.is_expanded(&item.id) {
            return TreeViewOutcome::Ignored;
        }
        state.set_expanded(&item.id, true);
        TreeViewOutcome::Toggled {
            visible,
            source,
            expanded: true,
        }
    }

    fn collapse_selected(&self, state: &mut TreeViewState) -> TreeViewOutcome {
        let Some((visible, source)) = self.selected_source(state) else {
            return TreeViewOutcome::Ignored;
        };
        let item = &self.items[source];
        if !item.expandable || !state.is_expanded(&item.id) {
            return TreeViewOutcome::Ignored;
        }
        state.set_expanded(&item.id, false);
        TreeViewOutcome::Toggled {
            visible,
            source,
            expanded: false,
        }
    }

    fn toggle_selected(&self, state: &mut TreeViewState) -> TreeViewOutcome {
        let Some((visible, source)) = self.selected_source(state) else {
            return TreeViewOutcome::Ignored;
        };
        let item = &self.items[source];
        if !item.expandable {
            return TreeViewOutcome::Ignored;
        }
        state.toggle_expanded(&item.id);
        TreeViewOutcome::Toggled {
            visible,
            source,
            expanded: state.is_expanded(&item.id),
        }
    }

    fn select_selected(&self, state: &TreeViewState) -> TreeViewOutcome {
        let Some((visible, source)) = self.selected_source(state) else {
            return TreeViewOutcome::Ignored;
        };
        if self.items[source].disabled {
            TreeViewOutcome::Ignored
        } else {
            TreeViewOutcome::Selected { visible, source }
        }
    }
}

fn visible_indices(items: &[TreeViewItem], state: &TreeViewState) -> Vec<usize> {
    let mut visible = Vec::new();
    let mut ancestor_visible = Vec::<bool>::new();
    for (index, item) in items.iter().enumerate() {
        ancestor_visible.truncate(usize::from(item.depth));
        let parents_visible = ancestor_visible.iter().all(|visible| *visible);
        if parents_visible {
            visible.push(index);
        }
        let children_visible = parents_visible && (!item.expandable || state.is_expanded(&item.id));
        if ancestor_visible.len() == usize::from(item.depth) {
            ancestor_visible.push(children_visible);
        }
    }
    visible
}

/// Canonical component-lifecycle hierarchical tree view.
///
/// The component measures one row per visible item under the caller-owned
/// expansion state, paints rows through the scoped paint context, registers
/// one composite roving-focus region plus one visible row region per stable
/// item id, and routes events through the same resolved layout. Tree state
/// remains caller-owned through an interior-mutable `RefCell`.
pub struct TreeViewComponent<'a, 'state> {
    id: LayoutId,
    tree: TreeView<'a>,
    state: &'state RefCell<TreeViewState>,
}

impl<'a, 'state> TreeViewComponent<'a, 'state> {
    /// Create a tree view with stable identity and caller-owned state.
    #[must_use]
    pub fn new(
        id: impl Into<LayoutId>,
        items: &'a [TreeViewItem],
        state: &'state RefCell<TreeViewState>,
    ) -> Self {
        Self {
            id: id.into(),
            tree: TreeView::new(items),
            state,
        }
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: TreeViewPolicy) -> Self {
        self.tree.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: TreeViewStyles) -> Self {
        self.tree.styles = styles;
        self
    }
}

impl Component for TreeViewComponent<'_, '_> {
    fn revision(&self) -> ComponentRevision {
        let state = self.state.borrow();
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        for item in self.tree.items {
            item.id.hash(&mut layout);
            item.label.hash(&mut layout);
            item.depth.hash(&mut layout);
            item.expandable.hash(&mut layout);
        }
        self.tree.policy.indent_width.hash(&mut layout);
        state.expanded_ids().hash(&mut layout);

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        for item in self.tree.items {
            item.disabled.hash(&mut paint);
        }
        format!("{:?}", self.tree.styles).hash(&mut paint);
        format!("{:?}", *state).hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let (width, height) = self.tree.size(&self.state.borrow());
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(width, usize::from(height))),
        )
        .with_metadata(LayoutMetadata::new().semantic("tree"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let height = u16::try_from(layout.size.height).unwrap_or(u16::MAX);
        let area = Rect::new(0, 0, layout.size.width, height);
        if area.is_empty() {
            return;
        }
        let state = self.state.borrow();
        if self.tree.is_interactive(&state) {
            cx.push_hit(
                SceneRegion::new(self.id.as_str(), area)
                    .role(HitRole::ListItem)
                    .pointer_events(self.tree.policy.mouse.enabled)
                    .hoverable(self.tree.policy.mouse.hover)
                    .focusable(self.tree.policy.keyboard.enabled)
                    .enabled(!state.interaction.disabled),
            );
            for (visible, source) in self
                .tree
                .visible_indices(&state)
                .into_iter()
                .take(usize::from(area.height))
                .enumerate()
            {
                let item = &self.tree.items[source];
                let row = Rect::new(0, u16_saturating(visible), area.width, 1);
                let item_id = format!("{}.{}", self.id.as_str(), item.id);
                cx.push_hit(
                    SceneRegion::new(item_id.clone(), row)
                        .role(HitRole::ListItem)
                        .pointer_events(self.tree.policy.mouse.enabled)
                        .hoverable(self.tree.policy.mouse.hover)
                        .enabled(!state.interaction.disabled && !item.disabled),
                );
                cx.push_semantic(SemanticRegion::new(item_id, row, "tree-item"));
            }
        }
        self.tree.paint(area, &state, cx);
        cx.push_semantic(SemanticRegion::new(self.id.as_str(), area, "tree"));
        cx.push_damage(LocalRect::new(0, 0, area.width, area.height));
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        let Some(area) = cx.find_rect(&layout.id) else {
            return EventOutcome::Ignored;
        };
        let outcome = self
            .tree
            .handle_event(area, &mut self.state.borrow_mut(), event);
        match outcome {
            TreeViewOutcome::Ignored => EventOutcome::Ignored,
            TreeViewOutcome::Redraw
            | TreeViewOutcome::Focused { .. }
            | TreeViewOutcome::Selected { .. }
            | TreeViewOutcome::Toggled { .. } => EventOutcome::Redraw,
        }
    }
}

impl crate::theme::ComponentTheme {
    /// Convert this semantic component theme into [`TreeViewStyles`].
    #[must_use]
    pub fn tree_view_styles(self) -> TreeViewStyles {
        TreeViewStyles::from(self)
    }
}

impl From<crate::theme::ComponentTheme> for TreeViewStyles {
    fn from(theme: crate::theme::ComponentTheme) -> Self {
        let theme = theme.for_surface(crate::theme::ComponentSurfaceDepth::Normal);
        Self {
            normal: theme.text,
            selected: theme.selected.add_modifier(bmux_tui::style::Modifier::BOLD),
            hovered: theme.info,
            pressed: theme.selected,
            disabled: theme.disabled,
            marker: theme.muted,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use bmux_keyboard::{KeyCode, KeyStroke};
    use bmux_tui::buffer::Buffer;
    use bmux_tui::component::{Component, Constraints, EventCx, LayoutCx};
    use bmux_tui::event::{Event, EventOutcome, MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};
    use bmux_tui::hit::HitRole;
    use bmux_tui::paint::{LocalRect, PaintCx};

    use super::{
        TreeView, TreeViewComponent, TreeViewItem, TreeViewOutcome, TreeViewPolicy, TreeViewState,
    };

    fn render_component(component: &TreeViewComponent<'_, '_>, area: Rect, frame: &mut Frame<'_>) {
        let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
        PaintCx::new(frame).with_child(
            i32::from(area.x),
            i64::from(area.y),
            LocalRect::new(0, 0, area.width, area.height),
            |cx| component.paint(&layout, cx),
        );
    }

    #[test]
    fn visible_indices_respect_expansion_state() {
        let items = sample_items();
        let view = TreeView::new(&items);
        let mut state = TreeViewState::new(Some(0));

        assert_eq!(view.visible_indices(&state), vec![0, 3]);

        state.set_expanded("src", true);
        assert_eq!(view.visible_indices(&state), vec![0, 1, 2, 3]);
    }

    #[test]
    fn render_shows_disclosure_markers_and_indentation() {
        let items = sample_items();
        let mut state = TreeViewState::new(Some(0));
        state.set_expanded("src", true);
        let state = RefCell::new(state);
        let component = TreeViewComponent::new("files", &items, &state);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 4));
        let mut frame = Frame::new(&mut buffer);

        render_component(&component, Rect::new(0, 0, 20, 4), &mut frame);

        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("▾ src               ")
        );
        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("    lib.rs          ")
        );
    }

    #[test]
    fn component_measures_visible_rows_and_relayouts_on_expansion() {
        let items = sample_items();
        let state = RefCell::new(TreeViewState::new(Some(0)));
        let component = TreeViewComponent::new("files", &items, &state);
        let mut cx = LayoutCx::new();

        let collapsed = component.layout(Constraints::for_width(20), &mut cx);
        assert_eq!(collapsed.size.height, 2);
        let collapsed_revision = component.revision();

        state.borrow_mut().set_expanded("src", true);
        let expanded = component.layout(Constraints::for_width(20), &mut cx);
        assert_eq!(expanded.size.height, 4);
        assert_eq!(cx.measured_nodes(), 2);
        assert_ne!(collapsed_revision.layout, component.revision().layout);

        state.borrow_mut().set_selected_visible(Some(1));
        let paint_only = component.revision();
        assert_eq!(
            component
                .layout(Constraints::for_width(20), &mut cx)
                .size
                .height,
            4
        );
        assert_ne!(paint_only.paint, collapsed_revision.paint);
    }

    #[test]
    fn render_registers_exact_composite_geometry_and_disabled_state() {
        let items = sample_items();
        let mut enabled = TreeViewState::new(Some(0));
        enabled.set_expanded("src", true);
        let mut disabled = enabled.clone();
        disabled.set_disabled(true);
        let enabled = RefCell::new(enabled);
        let disabled = RefCell::new(disabled);
        let mut buffer = Buffer::empty(Rect::new(3, 2, 24, 10));
        let mut frame = Frame::new(&mut buffer);

        render_component(
            &TreeViewComponent::new("files", &items, &enabled),
            Rect::new(6, 3, 18, 4),
            &mut frame,
        );
        render_component(
            &TreeViewComponent::new("disabled-files", &items, &disabled),
            Rect::new(6, 7, 18, 4),
            &mut frame,
        );

        let regions = frame.hits().regions();
        assert_eq!(regions.len(), 10);
        assert_eq!(regions[0].id.as_str(), "files");
        assert_eq!(regions[0].area, Rect::new(6, 3, 18, 4));
        assert_eq!(regions[0].role, HitRole::ListItem);
        assert!(regions[0].focusable);
        assert!(regions[0].enabled);
        assert_eq!(regions[1].id.as_str(), "files.src");
        assert_eq!(regions[1].area, Rect::new(6, 3, 18, 1));
        assert!(!regions[1].focusable);
        assert_eq!(regions[4].id.as_str(), "files.readme");
        assert_eq!(regions[4].area, Rect::new(6, 6, 18, 1));
        assert_eq!(regions[5].id.as_str(), "disabled-files");
        assert_eq!(regions[5].area, Rect::new(6, 7, 18, 4));
        assert!(!regions[5].enabled);
        assert!(regions[6..].iter().all(|region| !region.enabled));
        assert_eq!(frame.hits().focus_targets(None).len(), 1);
        assert!(
            frame
                .semantics()
                .regions()
                .iter()
                .any(|region| region.id == "files.lib" && region.role == "tree-item")
        );
    }

    #[test]
    fn component_routes_events_through_resolved_layout_and_updates_caller_state() {
        let items = sample_items();
        let mut initial = TreeViewState::new(Some(0));
        initial.set_focused(true);
        let state = RefCell::new(initial);
        let component = TreeViewComponent::new("files", &items, &state);
        let layout = component.layout(
            Constraints::tight(Rect::new(0, 0, 20, 4).size()),
            &mut LayoutCx::new(),
        );

        assert_eq!(
            component.event(
                &Event::Key(KeyStroke::simple(KeyCode::Right)),
                &layout,
                &mut EventCx::new(&layout),
            ),
            EventOutcome::Redraw
        );
        assert!(state.borrow().is_expanded("src"));
        assert_eq!(
            component.event(&Event::Tick, &layout, &mut EventCx::new(&layout)),
            EventOutcome::Ignored
        );
    }

    #[test]
    fn empty_fully_disabled_and_bare_trees_register_nothing() {
        let disabled = [TreeViewItem::new("disabled", "Disabled", 0).disabled(true)];
        let empty: [TreeViewItem; 0] = [];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 3));
        let mut frame = Frame::new(&mut buffer);

        let disabled_state = RefCell::new(TreeViewState::new(Some(0)));
        render_component(
            &TreeViewComponent::new("disabled", &disabled, &disabled_state),
            Rect::new(0, 0, 20, 1),
            &mut frame,
        );
        let empty_state = RefCell::new(TreeViewState::new(None));
        render_component(
            &TreeViewComponent::new("empty", &empty, &empty_state),
            Rect::new(0, 1, 20, 1),
            &mut frame,
        );
        let items = sample_items();
        let bare_state = RefCell::new(TreeViewState::new(Some(0)));
        render_component(
            &TreeViewComponent::new("bare", &items, &bare_state).policy(TreeViewPolicy::bare()),
            Rect::new(0, 2, 20, 1),
            &mut frame,
        );

        assert!(frame.hits().regions().is_empty());
    }

    #[test]
    fn keyboard_navigation_moves_selection() {
        let items = sample_items();
        let view = TreeView::new(&items);
        let mut state = TreeViewState::new(Some(0));
        state.set_focused(true);
        state.set_expanded("src", true);

        let outcome = view.handle_event(
            Rect::new(0, 0, 20, 4),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Down)),
        );

        assert_eq!(
            outcome,
            TreeViewOutcome::Focused {
                visible: 1,
                source: 1
            }
        );
        assert_eq!(state.selected_visible(), Some(1));
    }

    #[test]
    fn directly_dispatched_tree_key_navigates_without_visual_focus() {
        let items = sample_items();
        let view = TreeView::new(&items);
        let mut state = TreeViewState::new(Some(0));
        state.set_expanded("src", true);

        let outcome = view.handle_event(
            Rect::new(0, 0, 20, 4),
            &mut state,
            &Event::Key(KeyStroke::simple(KeyCode::Down)),
        );

        assert_eq!(
            outcome,
            TreeViewOutcome::Focused {
                visible: 1,
                source: 1,
            }
        );
        assert_eq!(state.selected_visible(), Some(1));
    }

    #[test]
    fn right_and_left_expand_and_collapse_selected_item() {
        let items = sample_items();
        let view = TreeView::new(&items);
        let mut state = TreeViewState::new(Some(0));
        state.set_focused(true);

        assert_eq!(
            view.handle_event(
                Rect::new(0, 0, 20, 4),
                &mut state,
                &Event::Key(KeyStroke::simple(KeyCode::Right)),
            ),
            TreeViewOutcome::Toggled {
                visible: 0,
                source: 0,
                expanded: true,
            }
        );
        assert!(state.is_expanded("src"));
        assert_eq!(
            view.handle_event(
                Rect::new(0, 0, 20, 4),
                &mut state,
                &Event::Key(KeyStroke::simple(KeyCode::Left)),
            ),
            TreeViewOutcome::Toggled {
                visible: 0,
                source: 0,
                expanded: false,
            }
        );
    }

    #[test]
    fn enter_selects_enabled_item() {
        let items = sample_items();
        let view = TreeView::new(&items);
        let mut state = TreeViewState::new(Some(1));
        state.set_focused(true);

        assert_eq!(
            view.handle_event(
                Rect::new(0, 0, 20, 4),
                &mut state,
                &Event::Key(KeyStroke::simple(KeyCode::Enter)),
            ),
            TreeViewOutcome::Selected {
                visible: 1,
                source: 3
            }
        );
    }

    #[test]
    fn mouse_click_selects_visible_row() {
        let items = sample_items();
        let view = TreeView::new(&items);
        let mut state = TreeViewState::new(Some(0));
        state.set_expanded("src", true);
        let area = Rect::new(0, 0, 20, 4);

        let _ = view.handle_event(
            area,
            &mut state,
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(1, 2),
            )),
        );
        let outcome = view.handle_event(
            area,
            &mut state,
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Up(MouseButton::Left),
                Point::new(1, 2),
            )),
        );

        assert_eq!(
            outcome,
            TreeViewOutcome::Selected {
                visible: 2,
                source: 2
            }
        );
        assert_eq!(state.selected_visible(), Some(2));
    }

    #[test]
    fn disabled_items_do_not_select() {
        let items = [TreeViewItem::new("disabled", "Disabled", 0).disabled(true)];
        let view = TreeView::new(&items);
        let mut state = TreeViewState::new(Some(0));

        assert_eq!(
            view.handle_event(
                Rect::new(0, 0, 20, 1),
                &mut state,
                &Event::Key(KeyStroke::simple(KeyCode::Enter)),
            ),
            TreeViewOutcome::Ignored
        );
    }

    #[test]
    fn bare_policy_ignores_events() {
        let items = sample_items();
        let view = TreeView::new(&items).policy(TreeViewPolicy::bare());
        let mut state = TreeViewState::new(Some(0));

        assert_eq!(
            view.handle_event(
                Rect::new(0, 0, 20, 4),
                &mut state,
                &Event::Key(KeyStroke::simple(KeyCode::Right)),
            ),
            TreeViewOutcome::Ignored
        );
        assert!(!state.is_expanded("src"));
    }

    fn sample_items() -> [TreeViewItem; 4] {
        [
            TreeViewItem::new("src", "src", 0).expandable(true),
            TreeViewItem::new("lib", "lib.rs", 1),
            TreeViewItem::new("main", "main.rs", 1),
            TreeViewItem::new("readme", "README.md", 0),
        ]
    }
}
