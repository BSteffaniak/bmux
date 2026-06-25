//! Generic hierarchical tree-view component.

use bmux_keyboard::KeyCode;
use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::prelude::{Line, Span};
use bmux_tui::style::{Color, Modifier, Style};

use crate::common::{ComponentMousePolicy, InteractionState};
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

    /// Render visible tree rows.
    pub fn render(&self, area: Rect, state: &TreeViewState, frame: &mut Frame<'_>) {
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
            let row = Rect::new(
                area.x,
                area.y.saturating_add(u16_saturating(visible)),
                area.width,
                1,
            );
            frame.write_line_with_fallback_style(
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
            Event::Key(stroke) if self.policy.keyboard.enabled => match stroke.key {
                KeyCode::Up => self.move_selection(state, -1),
                KeyCode::Down => self.move_selection(state, 1),
                KeyCode::Left => self.collapse_selected(state),
                KeyCode::Right => self.expand_selected(state),
                KeyCode::Enter if self.policy.keyboard.enter_selects => self.select_selected(state),
                KeyCode::Char(' ') if self.policy.keyboard.space_toggles => {
                    self.toggle_selected(state)
                }
                _ => TreeViewOutcome::Ignored,
            },
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

fn u16_saturating(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use bmux_keyboard::{KeyCode, KeyStroke};
    use bmux_tui::buffer::Buffer;
    use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};

    use super::{TreeView, TreeViewItem, TreeViewOutcome, TreeViewPolicy, TreeViewState};

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
        let view = TreeView::new(&items);
        let mut state = TreeViewState::new(Some(0));
        state.set_expanded("src", true);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 4));
        let mut frame = Frame::new(&mut buffer);

        view.render(Rect::new(0, 0, 20, 4), &state, &mut frame);

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
    fn keyboard_navigation_moves_selection() {
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
                source: 1
            }
        );
        assert_eq!(state.selected_visible(), Some(1));
    }

    #[test]
    fn right_and_left_expand_and_collapse_selected_item() {
        let items = sample_items();
        let view = TreeView::new(&items);
        let mut state = TreeViewState::new(Some(0));

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
