//! Generic hierarchical tree-view component.
//!
//! Items are caller-owned preorder rows. Each row has a plain label or a
//! styled [`Line`] label, and optional right-aligned trailing decorations
//! (counts, badges) laid out by display width with an explicit narrow-width
//! policy ([`TreeViewTrailingLayout`]).
//!
//! The component scales to very large trees: painting and interaction
//! registration visit only the rows inside the paint clip, pointer hits map a
//! position to its logical row directly, and a caller-supplied
//! [`TreeViewComponent::items_revision`] lets the visible-row projection and
//! intrinsic width be reused across frames instead of being recomputed.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use bmux_keyboard::KeyCode;
use bmux_tui::component::{
    Component, ComponentRevision, Constraints, EventCx, LayoutCx, LayoutId, LayoutMetadata,
    LayoutNode, LogicalRect, LogicalSize,
};
use bmux_tui::event::{Event, EventOutcome, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::geometry::Rect;
use bmux_tui::hit::{HitRegion as SceneRegion, HitRole};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span};
use bmux_tui::semantic::SemanticRegion;
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui::text::{TrailingAlign, TrailingPlacement, place_trailing};

use crate::common::{ComponentMousePolicy, InteractionState, u16_saturating};

/// One tree item in caller-provided preorder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeViewItem {
    /// Stable item id.
    pub id: String,
    /// Visible plain label. Items built with a styled label keep its plain
    /// text here.
    pub label: String,
    /// Optional styled label, painted and measured instead of `label`.
    ///
    /// Span styles are patched over the row's state style (normal, selected,
    /// hovered, pressed). Disabled rows patch the disabled style over them.
    pub rich_label: Option<Line>,
    /// Right-aligned trailing decorations, such as counts or badges, in
    /// display order.
    ///
    /// Decorations are never cut: when a row is too narrow for all of them
    /// and its minimum label width, whole decorations are hidden starting
    /// with the first (leftmost). See [`TreeViewTrailingLayout`].
    pub trailing: Vec<Line>,
    /// Nesting depth, where zero is root level.
    pub depth: u16,
    /// Whether this item can be expanded/collapsed.
    pub expandable: bool,
    /// Whether this item ignores selection/activation.
    pub disabled: bool,
}

impl TreeViewItem {
    /// Create a tree item with a plain label.
    #[must_use]
    pub fn new(id: impl Into<String>, label: impl Into<String>, depth: u16) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            rich_label: None,
            trailing: Vec::new(),
            depth,
            expandable: false,
            disabled: false,
        }
    }

    /// Create a tree item with a styled label.
    #[must_use]
    pub fn rich(id: impl Into<String>, label: impl Into<Line>, depth: u16) -> Self {
        Self::new(id, String::new(), depth).rich_label(label)
    }

    /// Return this item with a styled label. `label` is set to its plain text.
    #[must_use]
    pub fn rich_label(mut self, label: impl Into<Line>) -> Self {
        let label = label.into();
        self.label = label.plain_text();
        self.rich_label = Some(label);
        self
    }

    /// Return this item with `decorations` as its trailing decorations.
    #[must_use]
    pub fn trailing(mut self, decorations: impl IntoIterator<Item = Line>) -> Self {
        self.trailing = decorations.into_iter().collect();
        self
    }

    /// Return this item with one more trailing decoration after the existing ones.
    #[must_use]
    pub fn trailing_decoration(mut self, decoration: impl Into<Line>) -> Self {
        self.trailing.push(decoration.into());
        self
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

    fn label_width(&self) -> usize {
        self.rich_label.as_ref().map_or_else(
            || bmux_tui::text_width::display_width(&self.label),
            Line::width,
        )
    }
}

/// How a label that does not fit its row is shortened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TreeViewLabelOverflow {
    /// Cut the label at the row edge, or before the trailing decorations,
    /// without a marker. A wide grapheme that would straddle the cut is
    /// omitted.
    #[default]
    Clip,
    /// Truncate the label with an ellipsis.
    Ellipsis,
}

/// A bar marking the selected row, drawn in a column reserved at the start of
/// every row (before the indentation).
///
/// Every row reserves the symbol's display width; the selected row shows the
/// symbol in `style` patched over its row style, other rows show spaces in
/// their row style. Selection then stays visible without relying on color
/// alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TreeViewSelectionBar {
    /// Symbol drawn on the selected row, for example `▌`.
    pub symbol: &'static str,
    /// Style of the symbol, patched over the selected row's style.
    pub style: Style,
}

impl TreeViewSelectionBar {
    /// A selection bar of `symbol` in `style`.
    #[must_use]
    pub const fn new(symbol: &'static str, style: Style) -> Self {
        Self { symbol, style }
    }

    fn width(self) -> u16 {
        u16::try_from(bmux_tui::text_width::display_width(self.symbol)).unwrap_or(u16::MAX)
    }
}

/// Placement of right-aligned trailing decorations and label overflow.
///
/// For each row, the room after the indentation, disclosure marker, and
/// separator holds the label and the decorations. Decorations are shown only
/// while `min(label width, min_label_width) + gap + decorations width` fits
/// the room; otherwise whole decorations are hidden starting with the first
/// one, so the label never overlaps a decoration and no decoration is cut.
/// The label then takes the remaining room, shortened by `label_overflow`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TreeViewTrailingLayout {
    /// Minimum cells between the label and the first shown decoration.
    pub gap: u16,
    /// Cells between consecutive decorations.
    pub spacing: u16,
    /// Label cells kept visible before decorations are hidden. A label
    /// narrower than this only needs its own width.
    pub min_label_width: u16,
    /// How the label is shortened when it does not fit.
    pub label_overflow: TreeViewLabelOverflow,
}

impl TreeViewTrailingLayout {
    /// One-cell gap and spacing, a four-cell minimum label, and clipped labels.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            gap: 1,
            spacing: 1,
            min_label_width: 4,
            label_overflow: TreeViewLabelOverflow::Clip,
        }
    }

    /// Return this layout with `gap` cells between label and decorations.
    #[must_use]
    pub const fn gap(mut self, gap: u16) -> Self {
        self.gap = gap;
        self
    }

    /// Return this layout with `spacing` cells between decorations.
    #[must_use]
    pub const fn spacing(mut self, spacing: u16) -> Self {
        self.spacing = spacing;
        self
    }

    /// Return this layout keeping `width` label cells before hiding decorations.
    #[must_use]
    pub const fn min_label_width(mut self, width: u16) -> Self {
        self.min_label_width = width;
        self
    }

    /// Return this layout with the given label overflow behavior.
    #[must_use]
    pub const fn label_overflow(mut self, overflow: TreeViewLabelOverflow) -> Self {
        self.label_overflow = overflow;
        self
    }
}

impl Default for TreeViewTrailingLayout {
    fn default() -> Self {
        Self::new()
    }
}

/// Keyboard behavior for [`TreeViewComponent`].
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

/// Behavior policy for [`TreeViewComponent`].
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

/// Visual styles for [`TreeViewComponent`].
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

/// Visible-row projection of one item slice under one expansion state.
#[derive(Debug)]
struct TreeViewProjection {
    items_revision: Option<u64>,
    items_len: usize,
    expansion_generation: u64,
    /// Source indices in visible row order.
    visible: Vec<usize>,
    /// Whether any visible row is enabled.
    any_enabled: bool,
}

/// Width-cache key: everything besides the projection that affects width.
type TreeViewWidthKey = (u16, TreeViewTrailingLayout);

/// Disposable derived data kept beside [`TreeViewState`]. It never takes part
/// in equality, serialization, or snapshots.
#[derive(Clone, Default)]
struct TreeViewCache {
    /// Advanced on every expansion change, keying cached projections.
    expansion_generation: u64,
    /// Lookup index over the expanded ids, built on first keyed mutation.
    expanded_index: Option<BTreeSet<String>>,
    /// Projection retained for a caller-supplied items revision.
    projection: Option<Arc<TreeViewProjection>>,
    /// Intrinsic content width of the retained projection.
    width: Option<(TreeViewWidthKey, usize)>,
    /// A keyed selection known to be absent from the retained projection.
    hidden_selection: Option<String>,
}

impl PartialEq for TreeViewCache {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for TreeViewCache {}

impl std::fmt::Debug for TreeViewCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TreeViewCache").finish_non_exhaustive()
    }
}

/// Runtime state for [`TreeViewComponent`].
///
/// Expansion is keyed by item id. Selection is either a visible row
/// ([`Self::set_selected_visible`]) or keyed by item id
/// ([`Self::set_selected_id`]); keyboard and pointer selection made by the
/// component are keyed, so the selected item keeps its selection when rows
/// above it appear or disappear.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeViewState {
    selected_visible: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    selected_id: Option<String>,
    expanded: Vec<String>,
    hovered_visible: Option<usize>,
    pressed_visible: Option<usize>,
    /// Generic interaction flags.
    pub interaction: InteractionState,
    #[serde(skip)]
    cache: TreeViewCache,
}

impl TreeViewState {
    /// Capture native caller-owned state for an in-memory control rebuild.
    /// This is not a stable wire or durable storage contract. Reapply canonical
    /// values and current keyboard focus after restoring a snapshot.
    #[must_use]
    pub fn snapshot(&self) -> serde_json::Value {
        crate::common::state_snapshot(self)
    }

    /// Restore a bounded native rebuild snapshot, including private interaction
    /// state. Malformed snapshots return an error without changing this state.
    ///
    /// # Errors
    /// Returns an error for malformed, oversized, or incompatible snapshots.
    pub fn restore_snapshot(&mut self, snapshot: &serde_json::Value) -> Result<(), String> {
        let restored = crate::common::decode_state_snapshot(snapshot)?;
        *self = restored;
        Ok(())
    }

    /// Create tree-view state with a selected visible row.
    #[must_use]
    pub const fn new(selected_visible: Option<usize>) -> Self {
        Self {
            selected_visible,
            selected_id: None,
            expanded: Vec::new(),
            hovered_visible: None,
            pressed_visible: None,
            interaction: InteractionState::new(),
            cache: TreeViewCache {
                expansion_generation: 0,
                expanded_index: None,
                projection: None,
                width: None,
                hidden_selection: None,
            },
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

    /// Return the selected visible row.
    ///
    /// For a keyed selection this is the row resolved the last time a
    /// component projected this state; [`TreeViewComponent::selected_visible`]
    /// resolves it against the current items.
    #[must_use]
    pub const fn selected_visible(&self) -> Option<usize> {
        self.selected_visible
    }

    /// Return the selected item id when the selection is keyed.
    #[must_use]
    pub fn selected_id(&self) -> Option<&str> {
        self.selected_id.as_deref()
    }

    /// Return expanded ids.
    #[must_use]
    pub fn expanded_ids(&self) -> &[String] {
        &self.expanded
    }

    /// Select a visible row by position. This clears any keyed selection, so
    /// the selection stays on the row position when rows change.
    pub fn set_selected_visible(&mut self, selected_visible: Option<usize>) {
        self.selected_visible = selected_visible;
        self.selected_id = None;
        self.cache.hidden_selection = None;
    }

    /// Select the item with stable `id`.
    ///
    /// The component resolves the visible row whenever it projects this
    /// state, so the selection follows the item across expansion and item
    /// changes. While the item is hidden under a collapsed ancestor or absent,
    /// no row is selected and the id is kept, so the row is selected again
    /// when it reappears.
    pub fn set_selected_id(&mut self, id: impl Into<String>) {
        self.selected_id = Some(id.into());
        self.selected_visible = None;
        self.cache.hidden_selection = None;
    }

    /// Clear both positional and keyed selection.
    pub fn clear_selection(&mut self) {
        self.set_selected_visible(None);
    }

    /// Return whether `id` is expanded.
    #[must_use]
    pub fn is_expanded(&self, id: &str) -> bool {
        self.valid_expanded_index().map_or_else(
            || self.expanded.iter().any(|expanded| expanded == id),
            |index| index.contains(id),
        )
    }

    /// Set expansion for an id.
    ///
    /// Expanding is logarithmic in the number of expanded ids after the first
    /// keyed mutation builds a lookup index; collapsing is linear.
    pub fn set_expanded(&mut self, id: &str, expanded: bool) {
        let index = self.expanded_index_mut();
        if index.contains(id) == expanded {
            return;
        }
        if expanded {
            index.insert(id.to_owned());
            self.expanded.push(id.to_owned());
        } else {
            index.remove(id);
            self.expanded.retain(|expanded| expanded != id);
        }
        self.expansion_changed();
    }

    /// Replace the expanded ids in one step. Duplicates are ignored and the
    /// first occurrence keeps its order.
    pub fn set_expanded_ids(&mut self, ids: impl IntoIterator<Item = String>) {
        let mut index = BTreeSet::new();
        let mut expanded = Vec::new();
        for id in ids {
            if index.insert(id.clone()) {
                expanded.push(id);
            }
        }
        if expanded == self.expanded {
            self.cache.expanded_index = Some(index);
            return;
        }
        self.expanded = expanded;
        self.cache.expanded_index = Some(index);
        self.expansion_changed();
    }

    /// Toggle expansion for an id and return true when the state changed.
    pub fn toggle_expanded(&mut self, id: &str) -> bool {
        let expanded = self.is_expanded(id);
        self.set_expanded(id, !expanded);
        true
    }

    fn valid_expanded_index(&self) -> Option<&BTreeSet<String>> {
        self.cache
            .expanded_index
            .as_ref()
            .filter(|index| index.len() == self.expanded.len())
    }

    fn expanded_index_mut(&mut self) -> &mut BTreeSet<String> {
        if self.valid_expanded_index().is_none() {
            self.cache.expanded_index = Some(self.expanded.iter().cloned().collect());
        }
        self.cache.expanded_index.get_or_insert_with(BTreeSet::new)
    }

    const fn expansion_changed(&mut self) {
        self.cache.expansion_generation = self.cache.expansion_generation.wrapping_add(1);
        // Visible row indices can now identify different items.
        self.hovered_visible = None;
        self.pressed_visible = None;
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
struct TreeView<'a> {
    items: &'a [TreeViewItem],
    items_revision: Option<u64>,
    policy: TreeViewPolicy,
    styles: TreeViewStyles,
    trailing_layout: TreeViewTrailingLayout,
    focus_style: Style,
    toggle_on_click: bool,
    selection_bar: Option<TreeViewSelectionBar>,
}

impl<'a> TreeView<'a> {
    /// Create a tree view over caller-owned preorder items.
    #[must_use]
    pub const fn new(items: &'a [TreeViewItem]) -> Self {
        Self {
            items,
            items_revision: None,
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
            focus_style: crate::common::DEFAULT_FOCUS_STYLE,
            styles: TreeViewStyles {
                normal: Style::new(),
                selected: Style::new(),
                hovered: Style::new(),
                pressed: Style::new(),
                disabled: Style::new(),
                marker: Style::new(),
            },
            trailing_layout: TreeViewTrailingLayout::new(),
            toggle_on_click: false,
            selection_bar: None,
        }
    }

    /// Resolve the visible-row projection, reusing the retained one when the
    /// caller-supplied items revision and the expansion state are unchanged,
    /// and reconcile a keyed selection against it.
    fn projection(&self, state: &mut TreeViewState) -> Arc<TreeViewProjection> {
        let generation = state.cache.expansion_generation;
        let retained = self.items_revision.and_then(|revision| {
            state
                .cache
                .projection
                .as_ref()
                .filter(|projection| {
                    projection.items_revision == Some(revision)
                        && projection.items_len == self.items.len()
                        && projection.expansion_generation == generation
                })
                .map(Arc::clone)
        });
        let projection = retained.unwrap_or_else(|| {
            let visible = visible_indices(self.items, state);
            let any_enabled = visible.iter().any(|&source| !self.items[source].disabled);
            let projection = Arc::new(TreeViewProjection {
                items_revision: self.items_revision,
                items_len: self.items.len(),
                expansion_generation: generation,
                visible,
                any_enabled,
            });
            state.cache.projection = self
                .items_revision
                .is_some()
                .then(|| Arc::clone(&projection));
            state.cache.width = None;
            state.cache.hidden_selection = None;
            projection
        });
        self.reconcile_selection(state, &projection);
        projection
    }

    fn reconcile_selection(&self, state: &mut TreeViewState, projection: &TreeViewProjection) {
        let Some(key) = state.selected_id.as_deref() else {
            return;
        };
        let current = state
            .selected_visible
            .and_then(|row| projection.visible.get(row))
            .is_some_and(|&source| self.items[source].id == key);
        if current {
            return;
        }
        if state.cache.hidden_selection.as_deref() == Some(key) {
            state.selected_visible = None;
            return;
        }
        let row = projection
            .visible
            .iter()
            .position(|&source| self.items[source].id == key);
        if row.is_none() && self.items_revision.is_some() {
            state.cache.hidden_selection = Some(key.to_owned());
        }
        state.selected_visible = row;
    }

    fn content_width(&self, state: &mut TreeViewState, projection: &TreeViewProjection) -> usize {
        let key = (self.policy.indent_width, self.trailing_layout);
        if self.items_revision.is_some()
            && let Some((cached, width)) = state.cache.width
            && cached == key
        {
            return width;
        }
        let width = projection
            .visible
            .iter()
            .map(|&source| self.intrinsic_row_width(&self.items[source]))
            .max()
            .unwrap_or(0);
        if self.items_revision.is_some() {
            state.cache.width = Some((key, width));
        }
        width
    }

    fn intrinsic_row_width(&self, item: &TreeViewItem) -> usize {
        let trailing = decorations_width(&item.trailing, usize::from(self.trailing_layout.spacing));
        usize::from(self.bar_width())
            .saturating_add(usize::from(
                item.depth.saturating_mul(self.policy.indent_width),
            ))
            .saturating_add(2) // disclosure marker and separating space
            .saturating_add(item.label_width())
            .saturating_add(if trailing == 0 {
                0
            } else {
                usize::from(self.trailing_layout.gap).saturating_add(trailing)
            })
    }

    fn logical_size(&self, state: &mut TreeViewState, constraints: Constraints) -> LogicalSize {
        let projection = self.projection(state);
        // A tight width does not depend on content, so labels are not measured.
        let width = if constraints.min_width() == constraints.max_width() {
            constraints.max_width()
        } else {
            u16_saturating(self.content_width(state, &projection)).into()
        };
        LogicalSize::new(
            width,
            projection.visible.len().try_into().unwrap_or(u64::MAX),
        )
    }

    fn handle_key_event(&self, state: &mut TreeViewState, event: &Event) -> TreeViewOutcome {
        if state.interaction.disabled {
            return TreeViewOutcome::Ignored;
        }
        match event {
            Event::Key(stroke) if self.policy.keyboard.enabled && stroke.modifiers.is_empty() => {
                let projection = self.projection(state);
                let visible = projection.visible.as_slice();
                match stroke.key {
                    KeyCode::Up => self.move_selection(state, visible, -1),
                    KeyCode::Down => self.move_selection(state, visible, 1),
                    KeyCode::Left => self.collapse_selected(state, visible),
                    KeyCode::Right => self.expand_selected(state, visible),
                    KeyCode::Enter if self.policy.keyboard.enter_selects => {
                        self.select_selected(state, visible)
                    }
                    KeyCode::Char(' ') if self.policy.keyboard.space_toggles => {
                        self.toggle_selected(state, visible)
                    }
                    _ => TreeViewOutcome::Ignored,
                }
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

    /// Cells reserved for the selection bar at the start of every row.
    fn bar_width(&self) -> u16 {
        self.selection_bar.map_or(0, TreeViewSelectionBar::width)
    }

    fn row_line(
        &self,
        item: &TreeViewItem,
        state: &TreeViewState,
        visible: usize,
        width: u16,
    ) -> Line {
        let Some(bar) = self.selection_bar else {
            return self.row_body(item, state, visible, width);
        };
        let bar_width = bar.width().min(width);
        let style = self.row_style(item, state, visible);
        let selected = state.selected_visible == Some(visible) && bar_width == bar.width();
        let bar_span = if selected {
            Span::styled(bar.symbol, style.patch(bar.style))
        } else {
            Span::styled(" ".repeat(usize::from(bar_width)), style)
        };
        let mut line = self.row_body(item, state, visible, width - bar_width);
        line.spans.insert(0, bar_span);
        line
    }

    /// The row after the selection bar: indentation, marker, label and decorations.
    fn row_body(
        &self,
        item: &TreeViewItem,
        state: &TreeViewState,
        visible: usize,
        width: u16,
    ) -> Line {
        let indent_width = item
            .depth
            .saturating_mul(self.policy.indent_width)
            .min(width);
        let style = self.row_style(item, state, visible);
        let indent = " ".repeat(usize::from(indent_width));
        if indent_width == width {
            return Line::from_spans([Span::styled(indent, style)]);
        }
        let marker = if item.expandable {
            if state.is_expanded(&item.id) {
                "▾"
            } else {
                "▸"
            }
        } else {
            " "
        };
        let disabled = item.disabled || state.interaction.disabled;
        let marker_style = if disabled {
            self.styles.disabled
        } else {
            self.styles.marker
        };
        let remaining = width - indent_width;
        let separator = if remaining > 1 { " " } else { "" };
        let mut spans = vec![
            Span::styled(indent, style),
            Span::styled(marker, marker_style),
            Span::styled(separator, style),
        ];
        if remaining > 2 {
            spans.extend(self.row_content(
                item,
                style,
                disabled.then_some(self.styles.disabled),
                usize::from(remaining - 2),
            ));
        } else {
            spans.push(Span::styled(String::new(), style));
        }
        Line::from_spans(spans)
    }

    /// Lay out the label and trailing decorations inside `room` cells.
    fn row_content(
        &self,
        item: &TreeViewItem,
        style: Style,
        disabled: Option<Style>,
        room: usize,
    ) -> Vec<Span> {
        let layout = self.trailing_layout;
        let decorations = self.shown_decorations(item, room);
        if decorations.is_empty()
            && item.rich_label.is_none()
            && layout.label_overflow == TreeViewLabelOverflow::Clip
        {
            // The painter clips the plain label at the row edge.
            return vec![Span::styled(item.label.clone(), style)];
        }
        let label = item
            .rich_label
            .clone()
            .unwrap_or_else(|| Line::from_spans([Span::styled(item.label.clone(), style)]));
        let gap = usize::from(layout.gap);
        let spacing = usize::from(layout.spacing);
        let trailing_width = decorations_width(decorations, spacing);
        let label_room = if decorations.is_empty() {
            room
        } else {
            room.saturating_sub(gap.saturating_add(trailing_width))
        };
        let label = match layout.label_overflow {
            TreeViewLabelOverflow::Clip => label.viewport(0, label_room),
            TreeViewLabelOverflow::Ellipsis => label.truncate(label_room),
        };
        let mut line = if decorations.is_empty() {
            label
        } else {
            let mut trailing = Vec::new();
            for (index, decoration) in decorations.iter().enumerate() {
                if index > 0 && spacing > 0 {
                    trailing.push(Span::raw(" ".repeat(spacing)));
                }
                trailing.extend(decoration.spans.iter().cloned());
            }
            place_trailing(
                &label,
                &Line::from_spans(trailing),
                room,
                TrailingPlacement::new(TrailingAlign::Right)
                    .keep_visible(true)
                    .gap(gap),
            )
        };
        if let Some(disabled) = disabled {
            line = line.patch_style(disabled);
        }
        line.spans
    }

    /// Return the trailing decorations shown in `room` cells: the longest
    /// suffix of the item's decorations that leaves the minimum label width.
    fn shown_decorations<'item>(&self, item: &'item TreeViewItem, room: usize) -> &'item [Line] {
        if item.trailing.is_empty() {
            return &[];
        }
        let layout = self.trailing_layout;
        let label_need = item
            .label_width()
            .min(usize::from(layout.min_label_width))
            .saturating_add(usize::from(layout.gap));
        let spacing = usize::from(layout.spacing);
        let mut width = 0_usize;
        let mut first = item.trailing.len();
        for (index, decoration) in item.trailing.iter().enumerate().rev() {
            let added = if index + 1 == item.trailing.len() {
                decoration.width()
            } else {
                decoration.width().saturating_add(spacing)
            };
            if label_need.saturating_add(width).saturating_add(added) > room {
                break;
            }
            width = width.saturating_add(added);
            first = index;
        }
        &item.trailing[first..]
    }

    fn row_style(&self, item: &TreeViewItem, state: &TreeViewState, visible: usize) -> Style {
        if item.disabled || state.interaction.disabled {
            self.styles.disabled
        } else if state.pressed_visible == Some(visible) {
            self.styles.pressed
        } else if state.selected_visible == Some(visible) {
            if state.interaction.focused {
                self.styles.selected.patch(self.focus_style)
            } else {
                self.styles.selected
            }
        } else if state.hovered_visible == Some(visible) {
            self.styles.hovered
        } else {
            self.styles.normal
        }
    }

    fn handle_mouse_hit(
        &self,
        state: &mut TreeViewState,
        mouse: MouseEvent,
        hit: Option<usize>,
    ) -> TreeViewOutcome {
        match mouse.kind {
            MouseEventKind::Move if self.policy.mouse.hover => {
                let hovered = hit;
                if hovered == state.hovered_visible {
                    TreeViewOutcome::Ignored
                } else {
                    state.hovered_visible = hovered;
                    TreeViewOutcome::Redraw
                }
            }
            MouseEventKind::Down(MouseButton::Left) if self.policy.mouse.click => {
                let previous = state.pressed_visible;
                state.pressed_visible = hit;
                if hit.is_some() || previous != hit {
                    TreeViewOutcome::Redraw
                } else {
                    TreeViewOutcome::Ignored
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some(pressed) = state.pressed_visible.take() else {
                    return TreeViewOutcome::Ignored;
                };
                if self.policy.mouse.click
                    && let Some(hit) = hit
                    && pressed == hit
                    && let Some(source) = self.projection(state).visible.get(hit).copied()
                {
                    if self.items[source].disabled {
                        return TreeViewOutcome::Redraw;
                    }
                    let item = &self.items[source];
                    select(state, hit, item);
                    if self.toggle_on_click && item.expandable {
                        state.toggle_expanded(&item.id);
                        return TreeViewOutcome::Toggled {
                            visible: hit,
                            source,
                            expanded: state.is_expanded(&item.id),
                        };
                    }
                    return TreeViewOutcome::Selected {
                        visible: hit,
                        source,
                    };
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

    fn move_selection(
        &self,
        state: &mut TreeViewState,
        visible: &[usize],
        delta: i32,
    ) -> TreeViewOutcome {
        if visible.is_empty() {
            return TreeViewOutcome::Ignored;
        }
        let current = state.selected_visible.filter(|&row| row < visible.len());
        let next = current.map_or(0, |current| {
            if delta.is_negative() {
                current.saturating_sub(1)
            } else {
                current.saturating_add(1).min(visible.len() - 1)
            }
        });
        if current == Some(next) {
            return TreeViewOutcome::Ignored;
        }
        select(state, next, &self.items[visible[next]]);
        TreeViewOutcome::Focused {
            visible: next,
            source: visible[next],
        }
    }

    fn selected_source(state: &TreeViewState, visible: &[usize]) -> Option<(usize, usize)> {
        let row = state.selected_visible?;
        let source = visible.get(row).copied()?;
        Some((row, source))
    }

    fn expand_selected(&self, state: &mut TreeViewState, visible: &[usize]) -> TreeViewOutcome {
        let Some((visible, source)) = Self::selected_source(state, visible) else {
            return TreeViewOutcome::Ignored;
        };
        let item = &self.items[source];
        if item.disabled || !item.expandable || state.is_expanded(&item.id) {
            return TreeViewOutcome::Ignored;
        }
        state.set_expanded(&item.id, true);
        TreeViewOutcome::Toggled {
            visible,
            source,
            expanded: true,
        }
    }

    fn collapse_selected(&self, state: &mut TreeViewState, visible: &[usize]) -> TreeViewOutcome {
        let Some((visible, source)) = Self::selected_source(state, visible) else {
            return TreeViewOutcome::Ignored;
        };
        let item = &self.items[source];
        if item.disabled || !item.expandable || !state.is_expanded(&item.id) {
            return TreeViewOutcome::Ignored;
        }
        state.set_expanded(&item.id, false);
        TreeViewOutcome::Toggled {
            visible,
            source,
            expanded: false,
        }
    }

    fn toggle_selected(&self, state: &mut TreeViewState, visible: &[usize]) -> TreeViewOutcome {
        let Some((visible, source)) = Self::selected_source(state, visible) else {
            return TreeViewOutcome::Ignored;
        };
        let item = &self.items[source];
        if item.disabled || !item.expandable {
            return TreeViewOutcome::Ignored;
        }
        state.toggle_expanded(&item.id);
        TreeViewOutcome::Toggled {
            visible,
            source,
            expanded: state.is_expanded(&item.id),
        }
    }

    fn select_selected(&self, state: &TreeViewState, visible: &[usize]) -> TreeViewOutcome {
        let Some((visible, source)) = Self::selected_source(state, visible) else {
            return TreeViewOutcome::Ignored;
        };
        if self.items[source].disabled {
            TreeViewOutcome::Ignored
        } else {
            TreeViewOutcome::Selected { visible, source }
        }
    }
}

/// Record a component-made selection, keyed by the item's id.
fn select(state: &mut TreeViewState, row: usize, item: &TreeViewItem) {
    state.selected_visible = Some(row);
    if state.selected_id.as_deref() != Some(item.id.as_str()) {
        state.selected_id = Some(item.id.clone());
    }
    state.cache.hidden_selection = None;
}

/// Total width of `decorations` separated by `spacing` cells.
fn decorations_width(decorations: &[Line], spacing: usize) -> usize {
    decorations
        .iter()
        .map(Line::width)
        .sum::<usize>()
        .saturating_add(spacing.saturating_mul(decorations.len().saturating_sub(1)))
}

fn visible_indices(items: &[TreeViewItem], state: &TreeViewState) -> Vec<usize> {
    let expanded = state
        .expanded
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut visible = Vec::new();
    let mut ancestor_visible = Vec::<bool>::new();
    for (index, item) in items.iter().enumerate() {
        ancestor_visible.truncate(usize::from(item.depth));
        // Each entry already includes all of its ancestors' visibility.
        let parents_visible = ancestor_visible.last().copied().unwrap_or(true);
        if parents_visible {
            visible.push(index);
        }
        let children_visible =
            parents_visible && (!item.expandable || expanded.contains(item.id.as_str()));
        if ancestor_visible.len() == usize::from(item.depth) {
            ancestor_visible.push(children_visible);
        }
    }
    visible
}

fn hash_line(line: &Line, hasher: &mut impl Hasher) {
    line.spans.len().hash(hasher);
    for span in &line.spans {
        span.content.hash(hasher);
        span.style.hash(hasher);
    }
}

/// Canonical component-lifecycle hierarchical tree view.
///
/// The component measures one row per visible item under the caller-owned
/// expansion state, paints rows through the scoped paint context, registers
/// one composite roving-focus region plus one visible row region per stable
/// item id, and routes events through the same resolved layout. Tree state
/// remains caller-owned through an interior-mutable `RefCell`.
///
/// Painting, hit registration, and semantic regions cover only the rows in
/// the paint clip, and a pointer position maps to its logical row directly,
/// so a scrolled tree with tens of thousands of rows does per-frame work
/// proportional to its visible rows once the projection is retained through
/// [`Self::items_revision`].
pub struct TreeViewComponent<'a, 'state> {
    pointer_intent: bmux_tui::pointer::PointerIntent,
    id: LayoutId,
    tree: TreeView<'a>,
    state: &'state RefCell<TreeViewState>,
}

impl<'a, 'state> TreeViewComponent<'a, 'state> {
    /// Override pointer presentation without changing input routing or mouse policy.
    #[must_use]
    pub const fn pointer_intent(mut self, intent: bmux_tui::pointer::PointerIntent) -> Self {
        self.pointer_intent = intent;
        self
    }

    /// Return source-item indices in the component's current visible row order.
    ///
    /// Uses the same expansion state and projection as layout, painting, and input.
    #[must_use]
    pub fn visible_indices(&self) -> Vec<usize> {
        self.projection().visible.clone()
    }

    /// Return the number of visible rows.
    #[must_use]
    pub fn visible_count(&self) -> usize {
        self.projection().visible.len()
    }

    /// Return the source-item index painted at visible `row`.
    #[must_use]
    pub fn visible_source(&self, row: usize) -> Option<usize> {
        self.projection().visible.get(row).copied()
    }

    /// Return the visible row of the item with stable `id`, or `None` when it
    /// is hidden or absent. Linear in the visible rows.
    #[must_use]
    pub fn visible_row_of(&self, id: &str) -> Option<usize> {
        let projection = self.projection();
        projection
            .visible
            .iter()
            .position(|&source| self.tree.items[source].id == id)
    }

    /// Return the selected visible row, resolving a keyed selection against
    /// the current items and expansion state.
    #[must_use]
    pub fn selected_visible(&self) -> Option<usize> {
        let _ = self.projection();
        self.state.borrow().selected_visible
    }

    /// Return the local logical rectangle of visible `row` in `layout`, for
    /// ensure-visible requests to an enclosing scroll view. Returns `None`
    /// for rows outside the visible projection or the resolved height.
    #[must_use]
    pub fn row_rect(&self, layout: &LayoutNode, row: usize) -> Option<LogicalRect> {
        let row_y = u64::try_from(row).ok()?;
        (row < self.visible_count() && row_y < layout.size.height)
            .then(|| LogicalRect::new(0, row_y, layout.size.width, 1))
    }

    /// Create a tree view with stable identity and caller-owned state.
    #[must_use]
    pub fn new(
        id: impl Into<LayoutId>,
        items: &'a [TreeViewItem],
        state: &'state RefCell<TreeViewState>,
    ) -> Self {
        Self {
            id: id.into(),
            pointer_intent: bmux_tui::pointer::PointerIntent::Auto,
            tree: TreeView::new(items),
            state,
        }
    }

    /// Declare a caller-owned revision of the item slice.
    ///
    /// The caller must change `revision` whenever any item changes (ids,
    /// labels, decorations, depth, expandability, or disabled state). While
    /// it is unchanged, the component reuses the visible-row projection and
    /// intrinsic width retained in [`TreeViewState`] across frames until the
    /// expansion state changes, and its revision hashes `revision` instead of
    /// every item. Without a revision every projection is recomputed, which
    /// is linear in the item count.
    #[must_use]
    pub const fn items_revision(mut self, revision: u64) -> Self {
        self.tree.items_revision = Some(revision);
        self
    }

    /// Set the trailing-decoration and label-overflow layout.
    #[must_use]
    pub const fn trailing_layout(mut self, layout: TreeViewTrailingLayout) -> Self {
        self.tree.trailing_layout = layout;
        self
    }

    /// Make a completed click on an expandable row toggle its expansion as well as select
    /// it, reporting [`TreeViewOutcome::Toggled`] instead of [`TreeViewOutcome::Selected`].
    /// Off by default: clicks only select, and expansion changes come from the keyboard.
    #[must_use]
    pub const fn toggle_on_click(mut self, toggle: bool) -> Self {
        self.tree.toggle_on_click = toggle;
        self
    }

    fn projection(&self) -> Arc<TreeViewProjection> {
        self.tree.projection(&mut self.state.borrow_mut())
    }

    /// Handle an event using resolved geometry while preserving the typed action.
    pub fn handle_event(
        &self,
        event: &Event,
        layout: &LayoutNode,
        cx: &mut EventCx<'_>,
    ) -> TreeViewOutcome {
        let Some(area) = cx.find_rect(&layout.id) else {
            return TreeViewOutcome::Ignored;
        };
        let mut state = self.state.borrow_mut();
        if let Event::Mouse(mouse) = event {
            let handles_mouse = match mouse.kind {
                MouseEventKind::Move => self.tree.policy.mouse.hover,
                MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left) => {
                    self.tree.policy.mouse.click
                }
                _ => false,
            };
            if state.interaction.disabled || !self.tree.policy.mouse.enabled || !handles_mouse {
                return TreeViewOutcome::Ignored;
            }
            let rows = self.tree.projection(&mut state).visible.len();
            let hit = Self::hit_row(layout, cx, area, rows, mouse.position);
            self.tree.handle_mouse_hit(&mut state, *mouse, hit)
        } else {
            self.tree.handle_key_event(&mut state, event)
        }
    }

    /// Map a terminal point to the logical row under it, using the same
    /// translation and clip as painting.
    fn hit_row(
        layout: &LayoutNode,
        cx: &EventCx<'_>,
        area: Rect,
        rows: usize,
        position: bmux_tui::geometry::Point,
    ) -> Option<usize> {
        let bounds = cx.visible_rect(LogicalRect::new(
            0,
            0,
            layout.size.width,
            layout.size.height,
        ));
        if !area.contains(position) || !bounds.contains(position) {
            return None;
        }
        let (x, y) = cx.local_point(position);
        let column = u64::try_from(x).ok()?;
        let row = u64::try_from(y).ok()?;
        if column >= layout.size.width || row >= layout.size.height {
            return None;
        }
        usize::try_from(row).ok().filter(|&row| row < rows)
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: TreeViewPolicy) -> Self {
        self.tree.policy = policy;
        self
    }

    /// Set the active-item keyboard-focus overlay (`theme.focused` for themed
    /// focus, `Style::new()` to opt out). Selection and layout are unaffected.
    #[must_use]
    pub const fn focus_style(mut self, style: Style) -> Self {
        self.tree.focus_style = style;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: TreeViewStyles) -> Self {
        self.tree.styles = styles;
        self
    }

    /// Reserve a column at the start of every row for a bar marking the
    /// selected row. Labels, decorations and the intrinsic width move right
    /// by the bar's width.
    #[must_use]
    pub const fn selection_bar(mut self, bar: TreeViewSelectionBar) -> Self {
        self.tree.selection_bar = Some(bar);
        self
    }
}

impl Component for TreeViewComponent<'_, '_> {
    fn revision(&self) -> ComponentRevision {
        if self.tree.items_revision.is_some() || self.state.borrow().selected_id.is_some() {
            // Resolve a keyed selection so the paint revision reflects it.
            let _ = self.projection();
        }
        let state = self.state.borrow();
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        if let Some(revision) = self.tree.items_revision {
            revision.hash(&mut layout);
            self.tree.items.len().hash(&mut layout);
        } else {
            for item in self.tree.items {
                item.id.hash(&mut layout);
                item.label.hash(&mut layout);
                item.rich_label.is_some().hash(&mut layout);
                if let Some(label) = &item.rich_label {
                    hash_line(label, &mut layout);
                }
                item.trailing.len().hash(&mut layout);
                for decoration in &item.trailing {
                    hash_line(decoration, &mut layout);
                }
                item.depth.hash(&mut layout);
                item.expandable.hash(&mut layout);
            }
        }
        self.tree.policy.indent_width.hash(&mut layout);
        self.tree.trailing_layout.hash(&mut layout);
        self.tree.selection_bar.hash(&mut layout);
        state.expanded_ids().hash(&mut layout);

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        format!("{:?}", self.pointer_intent).hash(&mut paint);
        if let Some(revision) = self.tree.items_revision {
            revision.hash(&mut paint);
        } else {
            for item in self.tree.items {
                item.disabled.hash(&mut paint);
            }
        }
        self.tree.focus_style.hash(&mut paint);
        self.tree.styles.normal.hash(&mut paint);
        self.tree.styles.selected.hash(&mut paint);
        self.tree.styles.hovered.hash(&mut paint);
        self.tree.styles.pressed.hash(&mut paint);
        self.tree.styles.disabled.hash(&mut paint);
        self.tree.styles.marker.hash(&mut paint);
        self.tree.policy.keyboard.enabled.hash(&mut paint);
        self.tree.policy.keyboard.enter_selects.hash(&mut paint);
        self.tree.policy.keyboard.space_toggles.hash(&mut paint);
        self.tree.policy.mouse.enabled.hash(&mut paint);
        self.tree.policy.mouse.hover.hash(&mut paint);
        self.tree.policy.mouse.click.hash(&mut paint);
        self.tree.toggle_on_click.hash(&mut paint);
        state.selected_visible.hash(&mut paint);
        state.selected_id.hash(&mut paint);
        state.expanded.hash(&mut paint);
        state.hovered_visible.hash(&mut paint);
        state.pressed_visible.hash(&mut paint);
        state.interaction.focused.hash(&mut paint);
        state.interaction.hovered.hash(&mut paint);
        state.interaction.pressed.hash(&mut paint);
        state.interaction.disabled.hash(&mut paint);
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let size = self
            .tree
            .logical_size(&mut self.state.borrow_mut(), constraints);
        LayoutNode::leaf(self.id.clone(), constraints.constrain(size))
            .with_metadata(LayoutMetadata::new().semantic("tree"))
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let clip = cx.area();
        let start = usize::try_from(clip.y.max(0))
            .unwrap_or(usize::MAX)
            .min(layout.size.height.try_into().unwrap_or(usize::MAX));
        let end = usize::try_from(clip.y.saturating_add(i64::from(clip.height)).max(0))
            .unwrap_or(usize::MAX)
            .min(layout.size.height.try_into().unwrap_or(usize::MAX));
        if start >= end || layout.size.width == 0 || clip.width == 0 {
            return;
        }
        let area = Rect::new(
            0,
            0,
            layout.size.width.try_into().unwrap_or(u16::MAX),
            u16_saturating(end - start),
        );
        let projection = self.projection();
        let state = self.state.borrow();
        cx.with_child(
            0,
            i64::try_from(start).unwrap_or(i64::MAX),
            LocalRect::new(clip.x, 0, clip.width, area.height),
            |cx| {
                let interactive = (self.tree.policy.keyboard.enabled
                    || self.tree.policy.mouse.enabled)
                    && projection.any_enabled;
                if interactive {
                    cx.push_hit(
                        SceneRegion::new(self.id.as_str(), area)
                            .role(HitRole::ListItem)
                            .pointer_intent(self.pointer_intent)
                            .pointer_events(self.tree.policy.mouse.enabled)
                            .hoverable(
                                self.tree.policy.mouse.enabled && self.tree.policy.mouse.hover,
                            )
                            .focusable(self.tree.policy.keyboard.enabled)
                            .enabled(!state.interaction.disabled),
                    );
                }
                let painted = projection
                    .visible
                    .get(start..end.min(projection.visible.len()))
                    .unwrap_or_default();
                for (offset, &source) in painted.iter().enumerate() {
                    let visible = start + offset;
                    let item = &self.tree.items[source];
                    let row = Rect::new(0, u16_saturating(offset), area.width, 1);
                    let item_id = format!("{}.{}", self.id.as_str(), item.id);
                    if interactive {
                        cx.push_hit(
                            SceneRegion::new(item_id.clone(), row)
                                .role(HitRole::ListItem)
                                .pointer_intent(self.pointer_intent)
                                .pointer_events(self.tree.policy.mouse.enabled)
                                .hoverable(
                                    self.tree.policy.mouse.enabled && self.tree.policy.mouse.hover,
                                )
                                .enabled(!state.interaction.disabled && !item.disabled),
                        );
                    }
                    cx.push_semantic(SemanticRegion::new(item_id, row, "tree-item"));
                    cx.write_line_with_fallback_style(
                        LocalRect::new(0, i64::from(row.y), row.width, 1),
                        &self.tree.row_line(item, &state, visible, row.width),
                        self.tree.row_style(item, &state, visible),
                    );
                }
                cx.push_semantic(SemanticRegion::new(self.id.as_str(), area, "tree"));
                cx.push_damage(LocalRect::new(0, 0, area.width, area.height));
            },
        );
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        match self.handle_event(event, layout, cx) {
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
            hovered: theme.hover,
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

    use bmux_tui::geometry::Size;
    use bmux_tui::style::{Color, Style};

    use super::{
        TreeViewComponent, TreeViewItem, TreeViewOutcome, TreeViewPolicy, TreeViewSelectionBar,
        TreeViewState, TreeViewStyles,
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
    fn visible_projection_tracks_nested_expansion_and_painted_rows() {
        let items = [
            TreeViewItem::new("root", "Root", 0).expandable(true),
            TreeViewItem::new("nested", "Nested", 1).expandable(true),
            TreeViewItem::new("leaf", "葉🙂", 2),
            TreeViewItem::new("sibling", "Sibling", 0),
        ];
        let state = RefCell::new(TreeViewState::new(None));
        let component = TreeViewComponent::new("tree", &items, &state);
        assert_eq!(component.visible_indices(), vec![0, 3]);
        state.borrow_mut().set_expanded("root", true);
        assert_eq!(component.visible_indices(), vec![0, 1, 3]);
        state.borrow_mut().set_expanded("nested", true);
        assert_eq!(component.visible_indices(), vec![0, 1, 2, 3]);
        let area = Rect::new(0, 0, 30, 4);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        render_component(&component, area, &mut frame);
        assert_eq!(
            frame
                .semantics()
                .regions()
                .iter()
                .filter(|region| region.role == "tree-item")
                .count(),
            component.visible_indices().len()
        );
        state.borrow_mut().set_expanded("root", false);
        assert_eq!(component.visible_indices(), vec![0, 3]);
        let empty = TreeViewComponent::new("empty", &[], &state);
        assert_eq!(empty.visible_indices(), Vec::<usize>::new());
    }

    #[test]
    fn pointer_override_invalidates_paint_without_remeasuring_layout() {
        use bmux_tui::pointer::{PointerIntent, PointerShape};

        let items = [TreeViewItem::new("row", "Row", 0)];
        let state = RefCell::new(TreeViewState::new(None));
        let component = TreeViewComponent::new("tree", &items, &state);
        let original = component.revision();
        let component = component.pointer_intent(PointerIntent::Hidden);
        assert_eq!(component.pointer_intent, PointerIntent::Hidden);
        let hidden = component.revision();
        assert_eq!(original.layout, hidden.layout);
        assert_ne!(original.paint, hidden.paint);
        let component = component.pointer_intent(PointerIntent::Shape(PointerShape::Crosshair));
        assert_eq!(
            component.pointer_intent,
            PointerIntent::Shape(PointerShape::Crosshair)
        );
        assert_ne!(hidden.paint, component.revision().paint);
    }

    #[test]
    fn noninteractive_rows_keep_semantic_identity_without_hits() {
        for disabled in [false, true] {
            let items = [TreeViewItem::new("row", "Row", 0).disabled(disabled)];
            let state = RefCell::new(TreeViewState::new(None));
            let policy = if disabled {
                TreeViewPolicy::interactive()
            } else {
                TreeViewPolicy::bare()
            };
            let component = TreeViewComponent::new("tree", &items, &state).policy(policy);
            let area = Rect::new(0, 0, 20, 1);
            let mut buffer = Buffer::empty(area);
            let mut frame = Frame::new(&mut buffer);
            render_component(&component, area, &mut frame);
            assert_eq!(frame.hits().regions(), []);
            assert!(
                frame
                    .semantics()
                    .regions()
                    .iter()
                    .any(|region| region.id == "tree.row" && region.role == "tree-item")
            );
        }
    }

    #[test]
    fn interaction_state_changes_invalidate_tree_paint() {
        let items = [TreeViewItem::new("row", "Row", 0)];
        let state = RefCell::new(TreeViewState::new(None));
        let component = TreeViewComponent::new("tree", &items, &state);
        let initial = component.revision();
        for field in 0..7 {
            *state.borrow_mut() = TreeViewState::new(None);
            {
                let mut state = state.borrow_mut();
                match field {
                    0 => state.selected_visible = Some(0),
                    1 => state.hovered_visible = Some(0),
                    2 => state.pressed_visible = Some(0),
                    3 => state.interaction.focused = true,
                    4 => state.interaction.hovered = true,
                    5 => state.interaction.pressed = true,
                    _ => state.interaction.disabled = true,
                }
            }
            assert_ne!(component.revision(), initial);
        }
    }

    #[test]
    fn disabled_component_paints_entire_row_surface() {
        let items = [TreeViewItem::new("branch", "Branch", 0).expandable(true)];
        let state = RefCell::new(TreeViewState::new(Some(0)));
        state.borrow_mut().set_disabled(true);
        let disabled = bmux_tui::style::Style::new().bg(bmux_tui::style::Color::Red);
        let styles = super::TreeViewStyles {
            disabled,
            ..super::TreeViewStyles::default()
        };
        let component = TreeViewComponent::new("tree", &items, &state).styles(styles);
        let area = Rect::new(0, 0, 20, 1);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        render_component(&component, area, &mut frame);
        for x in 0..20 {
            assert_eq!(
                frame.buffer().get(Point::new(x, 0)).unwrap().style.bg,
                disabled.bg
            );
        }
    }

    #[test]
    fn disabled_disclosure_marker_uses_disabled_style() {
        for disabled_item in [false, true] {
            for disabled_tree in [false, true] {
                let items = [TreeViewItem::new("branch", "Branch", 0)
                    .expandable(true)
                    .disabled(disabled_item)];
                let mut initial = TreeViewState::new(None);
                initial.set_disabled(disabled_tree);
                let state = RefCell::new(initial);
                let component =
                    TreeViewComponent::new("tree", &items, &state).styles(super::TreeViewStyles {
                        disabled: bmux_tui::style::Style::new().fg(bmux_tui::style::Color::Red),
                        marker: bmux_tui::style::Style::new().fg(bmux_tui::style::Color::Green),
                        ..super::TreeViewStyles::default()
                    });
                for expanded in [false, true] {
                    state.borrow_mut().set_expanded("branch", expanded);
                    let expected = if disabled_item || disabled_tree {
                        component.tree.styles.disabled
                    } else {
                        component.tree.styles.marker
                    };
                    let area = Rect::new(0, 0, 20, 1);
                    let mut buffer = Buffer::empty(area);
                    let mut frame = Frame::new(&mut buffer);
                    render_component(&component, area, &mut frame);
                    assert_eq!(
                        frame.buffer().get(Point::new(0, 0)).unwrap().style,
                        expected
                    );
                }
            }
        }
    }

    #[test]
    fn unsupported_mouse_events_preserve_tree_state() {
        let items = [TreeViewItem::new("row", "Row", 0)];
        let state = RefCell::new(TreeViewState::new(Some(0)));
        state.borrow_mut().pressed_visible = Some(0);
        state.borrow_mut().hovered_visible = Some(0);
        let before = state.borrow().clone();
        let component = TreeViewComponent::new("tree", &items, &state);
        let layout = component.layout(Constraints::for_width(20), &mut LayoutCx::new());
        for kind in [
            MouseEventKind::ScrollDown,
            MouseEventKind::ScrollUp,
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Down(MouseButton::Right),
            MouseEventKind::Up(MouseButton::Right),
        ] {
            let event = Event::Mouse(MouseEvent {
                kind,
                position: Point::new(0, 0),
                modifiers: bmux_tui::event::MouseModifiers::default(),
            });
            assert_eq!(
                component.event(&event, &layout, &mut EventCx::new(&layout)),
                EventOutcome::Ignored
            );
            assert_eq!(*state.borrow(), before);
        }
    }

    #[test]
    fn component_measurement_accounts_for_unicode_and_indentation() {
        for (label, label_width) in [
            ("plain", 5_u16),
            ("界界", 4),
            ("e\u{301}", 1),
            ("👩‍💻", 2),
            ("", 0),
        ] {
            for (depth, indent_width) in [(0, 0_u16), (3, 6), (u16::MAX, u16::MAX)] {
                let items = [TreeViewItem::new("row", label, depth)];
                let expected = indent_width.saturating_add(2).saturating_add(label_width);
                let state = RefCell::new(TreeViewState::new(None));
                let component = TreeViewComponent::new("tree", &items, &state);
                for constraints in [
                    Constraints::new(0, u16::MAX.into(), 0, None),
                    Constraints::for_width(3),
                ] {
                    let layout = component.layout(constraints, &mut LayoutCx::new());
                    assert_eq!(
                        layout.size,
                        constraints
                            .constrain(bmux_tui::component::LogicalSize::new(expected.into(), 1))
                    );
                }
            }
        }
    }

    #[test]
    fn deep_indentation_paints_only_the_allocated_row_width() {
        let items = [TreeViewItem::new("deep", "Hidden label", u16::MAX).expandable(true)];
        let state = RefCell::new(TreeViewState::new(None));
        let component = TreeViewComponent::new("tree", &items, &state);
        let line = component.tree.row_line(&items[0], &state.borrow(), 0, 3);
        assert_eq!(line.width(), 3);
        assert_eq!(line.spans.len(), 1);
        let area = Rect::new(0, 0, 3, 1);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        render_component(&component, area, &mut frame);
        assert_eq!(frame.hits().regions().len(), 2);
    }

    #[test]
    fn hidden_label_is_not_materialized_when_only_the_prefix_fits() {
        let items = [TreeViewItem::new("branch", "X".repeat(100_000), 2).expandable(true)];
        let state = RefCell::new(TreeViewState::new(None));
        let component = TreeViewComponent::new("tree", &items, &state);
        for width in [5, 6] {
            let line = component
                .tree
                .row_line(&items[0], &state.borrow(), 0, width);
            assert_eq!(line.width(), usize::from(width));
            assert_eq!(line.spans.last().unwrap().content, "");
        }
    }

    #[test]
    fn horizontally_clipped_indentation_preserves_logical_columns() {
        let items = [TreeViewItem::new("branch", "X", 2).expandable(true)];
        let state = RefCell::new(TreeViewState::new(None));
        let component = TreeViewComponent::new("tree", &items, &state);
        let layout = component.layout(Constraints::for_width(7), &mut LayoutCx::new());
        for (offset, expected) in [(2, "  ▸"), (3, " ▸ "), (4, "▸ X")] {
            let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 1));
            let mut frame = Frame::new(&mut buffer);
            PaintCx::new(&mut frame).with_child(
                -offset,
                0,
                LocalRect::new(offset, 0, 3, 1),
                |cx| component.paint(&layout, cx),
            );
            assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some(expected));
        }
    }

    #[test]
    fn indentation_boundary_preserves_marker_and_label_columns() {
        let items = [TreeViewItem::new("branch", "X", 2).expandable(true)];
        let state = RefCell::new(TreeViewState::new(None));
        let component = TreeViewComponent::new("tree", &items, &state);
        for expanded in [false, true] {
            state.borrow_mut().set_expanded("branch", expanded);
            for (width, collapsed, opened) in [
                (3, "   ", "   "),
                (4, "    ", "    "),
                (5, "    ▸", "    ▾"),
                (6, "    ▸ ", "    ▾ "),
                (7, "    ▸ X", "    ▾ X"),
            ] {
                let area = Rect::new(0, 0, width, 1);
                let mut buffer = Buffer::empty(area);
                let mut frame = Frame::new(&mut buffer);
                render_component(&component, area, &mut frame);
                assert_eq!(
                    frame.buffer().row_symbols(0).as_deref(),
                    Some(if expanded { opened } else { collapsed })
                );
                assert_eq!(frame.hits().regions().len(), 2);
            }
        }
    }

    #[test]
    fn selection_bar_marks_only_the_selected_row_and_shifts_every_row() {
        let items = [
            TreeViewItem::new("a", "Alpha", 0),
            TreeViewItem::new("b", "Beta", 1),
        ];
        let state = RefCell::new(TreeViewState::new(Some(1)));
        let bar_style = Style::new().fg(Color::Magenta);
        let component = TreeViewComponent::new("tree", &items, &state)
            .styles(TreeViewStyles {
                selected: Style::new().bg(Color::Blue),
                ..TreeViewStyles::default()
            })
            .selection_bar(TreeViewSelectionBar::new("▌", bar_style));
        let layout = component.layout(Constraints::loose(Size::new(40, 4)), &mut LayoutCx::new());
        // The intrinsic width counts the bar column.
        assert_eq!(layout.size.width, 1 + 2 + 2 + 4);
        let area = Rect::new(0, 0, 12, 2);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        render_component(&component, area, &mut frame);
        assert_eq!(
            frame.buffer().row_symbols(0).as_deref(),
            Some("   Alpha    ")
        );
        assert_eq!(
            frame.buffer().row_symbols(1).as_deref(),
            Some("▌    Beta   ")
        );
        let bar = frame
            .buffer()
            .get(Point::new(0, 1))
            .expect("bar cell")
            .style;
        assert_eq!(bar.fg, Some(Color::Magenta));
        assert_eq!(bar.bg, Some(Color::Blue));

        // Narrower than the bar: nothing but blank cells, no partial symbol.
        for width in [0, 1] {
            assert_eq!(
                component
                    .tree
                    .row_line(&items[1], &state.borrow(), 1, width)
                    .width(),
                usize::from(width)
            );
        }
    }

    #[test]
    fn release_without_press_is_ignored() {
        let items = [TreeViewItem::new("row", "Row", 0)];
        let state = RefCell::new(TreeViewState::new(None));
        let component = TreeViewComponent::new("tree", &items, &state);
        let layout = component.layout(Constraints::for_width(20), &mut LayoutCx::new());
        let before = state.borrow().clone();
        for y in [0, 2] {
            let event = Event::Mouse(MouseEvent::new(
                MouseEventKind::Up(MouseButton::Left),
                Point::new(2, y),
            ));
            assert_eq!(
                component.handle_event(&event, &layout, &mut EventCx::new(&layout)),
                TreeViewOutcome::Ignored
            );
            assert_eq!(*state.borrow(), before);
        }
    }

    #[test]
    fn expansion_change_cancels_stale_pointer_activation() {
        let items = [
            TreeViewItem::new("root", "Root", 0).expandable(true),
            TreeViewItem::new("child", "Child", 1),
            TreeViewItem::new("sibling", "Sibling", 0),
        ];
        let mut initial = TreeViewState::new(Some(0));
        initial.set_expanded("root", true);
        let state = RefCell::new(initial);
        let component = TreeViewComponent::new("tree", &items, &state);
        let layout = component.layout(Constraints::for_width(20), &mut LayoutCx::new());
        for kind in [
            MouseEventKind::Move,
            MouseEventKind::Down(MouseButton::Left),
        ] {
            assert_eq!(
                component.handle_event(
                    &Event::Mouse(MouseEvent::new(kind, Point::new(2, 1))),
                    &layout,
                    &mut EventCx::new(&layout),
                ),
                TreeViewOutcome::Redraw
            );
        }
        state.borrow_mut().set_expanded("root", true);
        assert_eq!(state.borrow().pressed_visible, Some(1));
        state.borrow_mut().set_expanded("root", false);
        assert_eq!(state.borrow().hovered_visible, None);
        assert_eq!(state.borrow().pressed_visible, None);
        let layout = component.layout(Constraints::for_width(20), &mut LayoutCx::new());
        assert_eq!(layout.size.height, 2);
        let outcome = component.handle_event(
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Up(MouseButton::Left),
                Point::new(2, 1),
            )),
            &layout,
            &mut EventCx::new(&layout),
        );
        assert_eq!(outcome, TreeViewOutcome::Ignored);
        assert_eq!(state.borrow().selected_visible(), Some(0));
    }

    #[test]
    fn whole_tree_disabled_style_overrides_row_interaction() {
        let items = [TreeViewItem::new("row", "Row", 0)];
        let state = RefCell::new(TreeViewState::new(None));
        let component =
            TreeViewComponent::new("tree", &items, &state).styles(super::TreeViewStyles::default());
        let styles = component.tree.styles;
        for (disabled, selected, hovered, pressed, expected) in [
            (false, false, false, false, styles.normal),
            (false, false, true, false, styles.hovered),
            (false, true, true, false, styles.selected),
            (false, true, true, true, styles.pressed),
            (true, false, false, false, styles.disabled),
            (true, true, false, false, styles.disabled),
            (true, true, true, true, styles.disabled),
        ] {
            {
                let mut state = state.borrow_mut();
                state.interaction.disabled = disabled;
                state.selected_visible = selected.then_some(0);
                state.hovered_visible = hovered.then_some(0);
                state.pressed_visible = pressed.then_some(0);
            }
            let area = Rect::new(0, 0, 20, 1);
            let mut buffer = Buffer::empty(area);
            let mut frame = Frame::new(&mut buffer);
            render_component(&component, area, &mut frame);
            for x in 2..5 {
                assert_eq!(
                    frame.buffer().get(Point::new(x, 0)).unwrap().style,
                    expected
                );
            }
        }
    }

    #[test]
    fn nested_visibility_recovers_at_siblings_and_roots() {
        let items = [
            TreeViewItem::new("root", "Root", 0).expandable(true),
            TreeViewItem::new("branch", "Branch", 1).expandable(true),
            TreeViewItem::new("leaf", "Leaf", 2),
            TreeViewItem::new("sibling", "Sibling", 1),
            TreeViewItem::new("other", "Other", 0),
        ];
        let state = RefCell::new(TreeViewState::new(None));
        let component = TreeViewComponent::new("tree", &items, &state);
        for (change, expected) in [
            (None, vec!["tree.root", "tree.other"]),
            (Some(("branch", true)), vec!["tree.root", "tree.other"]),
            (
                Some(("root", true)),
                vec![
                    "tree.root",
                    "tree.branch",
                    "tree.leaf",
                    "tree.sibling",
                    "tree.other",
                ],
            ),
            (
                Some(("branch", false)),
                vec!["tree.root", "tree.branch", "tree.sibling", "tree.other"],
            ),
            (Some(("root", false)), vec!["tree.root", "tree.other"]),
        ] {
            if let Some((id, expanded)) = change {
                state.borrow_mut().set_expanded(id, expanded);
            }
            let layout = component.layout(Constraints::for_width(20), &mut LayoutCx::new());
            assert_eq!(
                layout.size.height,
                expected.len().try_into().unwrap_or(u64::MAX)
            );
            let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 5));
            let mut frame = Frame::new(&mut buffer);
            component.paint(&layout, &mut PaintCx::new(&mut frame));
            let rows: Vec<_> = frame
                .semantics()
                .regions()
                .iter()
                .filter(|region| region.role == "tree-item")
                .map(|region| region.id.as_str())
                .collect();
            assert_eq!(rows, expected);
        }
    }

    #[test]
    fn outside_press_redraws_when_clearing_pressed_row() {
        let items = [TreeViewItem::new("row", "Row", 0)];
        let state = RefCell::new(TreeViewState::new(None));
        let component = TreeViewComponent::new("tree", &items, &state);
        let layout = component.layout(Constraints::for_width(20), &mut LayoutCx::new());
        let mut cx = EventCx::new(&layout);
        let press = |y| {
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                position: Point::new(0, y),
                modifiers: bmux_tui::event::MouseModifiers::default(),
            })
        };
        assert_eq!(
            component.event(&press(0), &layout, &mut cx),
            EventOutcome::Redraw
        );
        assert_eq!(state.borrow().pressed_visible, Some(0));
        assert_eq!(
            component.event(&press(2), &layout, &mut cx),
            EventOutcome::Redraw
        );
        assert_eq!(state.borrow().pressed_visible, None);
        assert_eq!(
            component.event(&press(2), &layout, &mut cx),
            EventOutcome::Ignored
        );
    }

    #[test]
    fn disabling_click_before_release_cancels_tree_selection() {
        let items = [TreeViewItem::new("one", "One", 0)];
        let mut tree = super::TreeView::new(&items);
        let mut state = TreeViewState::new(None);
        let mouse = |kind| MouseEvent::new(kind, Point::new(0, 0));
        assert_eq!(
            tree.handle_mouse_hit(
                &mut state,
                mouse(MouseEventKind::Down(MouseButton::Left)),
                Some(0)
            ),
            TreeViewOutcome::Redraw
        );
        assert_eq!(state.pressed_visible, Some(0));
        tree.policy.mouse.click = false;
        assert_eq!(
            tree.handle_mouse_hit(
                &mut state,
                mouse(MouseEventKind::Up(MouseButton::Left)),
                Some(0)
            ),
            TreeViewOutcome::Redraw
        );
        assert_eq!(state.pressed_visible, None);
        assert_eq!(state.selected_visible, None);
        tree.policy.mouse.click = true;
        assert_eq!(
            tree.handle_mouse_hit(
                &mut state,
                mouse(MouseEventKind::Up(MouseButton::Left)),
                Some(0)
            ),
            TreeViewOutcome::Ignored
        );
        assert_eq!(state.selected_visible, None);
    }

    #[test]
    fn navigation_initializes_missing_and_stale_selection() {
        for count in [1, 3] {
            let items = (0..count)
                .map(|index| TreeViewItem::new(index.to_string(), "row", 0))
                .collect::<Vec<_>>();
            for selected in [None, Some(count)] {
                for key in [KeyCode::Up, KeyCode::Down] {
                    let state = RefCell::new(TreeViewState::new(selected));
                    let component = TreeViewComponent::new("tree", &items, &state);
                    let layout = component.layout(Constraints::for_width(20), &mut LayoutCx::new());
                    assert_eq!(
                        component.event(
                            &Event::Key(KeyStroke::simple(key)),
                            &layout,
                            &mut EventCx::new(&layout)
                        ),
                        EventOutcome::Redraw
                    );
                    assert_eq!(state.borrow().selected_visible, Some(0));
                }
            }
        }
    }

    #[test]
    fn disabled_branch_ignores_keyboard_expansion() {
        let items = [TreeViewItem::new("branch", "Branch", 0)
            .expandable(true)
            .disabled(true)];
        for expanded in [false, true] {
            let state = RefCell::new(TreeViewState::new(Some(0)));
            state.borrow_mut().set_expanded("branch", expanded);
            let component = TreeViewComponent::new("tree", &items, &state);
            let layout = component.layout(Constraints::for_width(20), &mut LayoutCx::new());
            for key in [KeyCode::Left, KeyCode::Right, KeyCode::Char(' ')] {
                let event = Event::Key(KeyStroke::simple(key));
                assert_eq!(
                    component.event(&event, &layout, &mut EventCx::new(&layout)),
                    EventOutcome::Ignored
                );
                assert_eq!(state.borrow().is_expanded("branch"), expanded);
            }
        }
    }

    #[test]
    fn clicking_disabled_row_preserves_selection() {
        let items = [
            TreeViewItem::new("enabled", "Enabled", 0),
            TreeViewItem::new("disabled", "Disabled", 0).disabled(true),
        ];
        let state = RefCell::new(TreeViewState::new(Some(0)));
        let component = TreeViewComponent::new("tree", &items, &state);
        let layout = component.layout(Constraints::for_width(20), &mut LayoutCx::new());
        let mut cx = EventCx::new(&layout);
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            let event = Event::Mouse(MouseEvent {
                kind,
                position: Point::new(2, 1),
                modifiers: bmux_tui::event::MouseModifiers::default(),
            });
            component.event(&event, &layout, &mut cx);
        }
        assert_eq!(state.borrow().selected_visible, Some(0));
        assert_eq!(state.borrow().pressed_visible, None);
    }

    #[test]
    fn scrolled_pointer_selects_logical_row_beyond_terminal_limit() {
        let items = (0..70_000)
            .map(|index| TreeViewItem::new(index.to_string(), "row", 0))
            .collect::<Vec<_>>();
        let state = RefCell::new(TreeViewState::new(None));
        let component = TreeViewComponent::new("large-tree", &items, &state);
        let layout = component.layout(Constraints::for_width(20), &mut LayoutCx::new());
        let mut cx = EventCx::new(&layout);
        cx.with_transform(0, 0, 0, -69_998, Rect::new(0, 0, 20, 2), |cx| {
            for kind in [
                MouseEventKind::Move,
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
            ] {
                let event = Event::Mouse(MouseEvent {
                    kind,
                    position: Point::new(2, 1),
                    modifiers: bmux_tui::event::MouseModifiers::default(),
                });
                assert_eq!(component.event(&event, &layout, cx), EventOutcome::Redraw);
            }
        });
        assert_eq!(state.borrow().selected_visible, Some(69_999));
        assert_eq!(state.borrow().hovered_visible, Some(69_999));
    }

    #[test]
    fn component_paints_rows_beyond_terminal_coordinate_limit() {
        let items = (0..70_000)
            .map(|index| TreeViewItem::new(index.to_string(), format!("row{index}"), 0))
            .collect::<Vec<_>>();
        let state = RefCell::new(TreeViewState::new(None));
        let component = TreeViewComponent::new("large-tree", &items, &state);
        let layout = component.layout(Constraints::for_width(20), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 2));
        let mut frame = Frame::new(&mut buffer);
        PaintCx::new(&mut frame).with_child(0, -69_998, LocalRect::new(0, 69_998, 20, 2), |cx| {
            component.paint(&layout, cx);
        });
        assert!(frame.buffer().row_symbols(0).unwrap().contains("row69998"));
        assert!(frame.buffer().row_symbols(1).unwrap().contains("row69999"));
    }

    #[test]
    fn component_measurement_preserves_large_logical_tree_height() {
        let items = (0..70_000)
            .map(|index| TreeViewItem::new(index.to_string(), "row", 0))
            .collect::<Vec<_>>();
        let state = RefCell::new(TreeViewState::new(None));
        let component = TreeViewComponent::new("large-tree", &items, &state);
        let layout = component.layout(Constraints::for_width(20), &mut LayoutCx::new());
        assert_eq!(layout.size.height, 70_000);
    }

    #[test]
    fn mouse_disabled_scene_retains_keyboard_target_without_hover() {
        let items = [TreeViewItem::new("row", "Row", 0)];
        let state = RefCell::new(TreeViewState::new(None));
        let mut policy = TreeViewPolicy::interactive();
        policy.mouse.enabled = false;
        let component = TreeViewComponent::new("tree", &items, &state).policy(policy);
        let layout = component.layout(Constraints::for_width(12), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 1));
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
    fn interaction_policy_changes_invalidate_paint_not_layout() {
        let items = sample_items();
        let state = RefCell::new(TreeViewState::new(Some(0)));
        let initial = TreeViewComponent::new("tree", &items, &state).revision();
        let mut keyboard = TreeViewPolicy::interactive();
        keyboard.keyboard.enabled = false;
        let mut mouse = TreeViewPolicy::interactive();
        mouse.mouse.enabled = false;
        for policy in [keyboard, mouse, TreeViewPolicy::bare()] {
            let changed = TreeViewComponent::new("tree", &items, &state)
                .policy(policy)
                .revision();
            assert_eq!(initial.layout, changed.layout);
            assert_ne!(initial.paint, changed.paint);
        }
    }

    #[test]
    fn pointer_rows_respect_translation_expansion_and_viewport_bounds() {
        let items = sample_items();
        let state = RefCell::new(TreeViewState::new(Some(0)));
        let hit_at = |area: Rect, point: Point| {
            let component = TreeViewComponent::new("tree", &items, &state);
            let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
            EventCx::new(&layout).with_transform(
                0,
                0,
                i32::from(area.x),
                i64::from(area.y),
                area,
                |cx| {
                    component.handle_event(
                        &Event::Mouse(MouseEvent::new(MouseEventKind::Move, point)),
                        &layout,
                        cx,
                    )
                },
            );
            state.borrow().hovered_visible
        };
        let area = Rect::new(5, 7, 10, 3);
        assert_eq!(hit_at(area, Point::new(5, 7)), Some(0));
        assert_eq!(hit_at(area, Point::new(14, 8)), Some(1));
        assert_eq!(hit_at(area, Point::new(5, 9)), None);
        state.borrow_mut().set_expanded("src", true);
        assert_eq!(hit_at(area, Point::new(5, 9)), Some(2));
        for point in [
            Point::new(4, 7),
            Point::new(15, 7),
            Point::new(5, 6),
            Point::new(5, 10),
        ] {
            assert_eq!(hit_at(area, point), None);
        }
        assert_eq!(hit_at(Rect::new(5, 7, 0, 3), Point::new(5, 7)), None);
    }

    #[test]
    fn visible_indices_respect_expansion_state() {
        let items = sample_items();
        let state = RefCell::new(TreeViewState::new(Some(0)));
        let component = TreeViewComponent::new("files", &items, &state);
        for expanded in [false, true, false] {
            state.borrow_mut().set_expanded("src", expanded);
            let layout = component.layout(Constraints::for_width(20), &mut LayoutCx::new());
            let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 4));
            let mut frame = Frame::new(&mut buffer);
            component.paint(&layout, &mut PaintCx::new(&mut frame));
            let expected_sources = if expanded {
                vec![0, 1, 2, 3]
            } else {
                vec![0, 3]
            };
            let hits = &frame.hits().regions()[1..];
            assert_eq!(hits.len(), expected_sources.len());
            for (hit, source) in hits.iter().zip(expected_sources) {
                assert_eq!(hit.id, format!("files.{}", items[source].id).into());
            }
        }
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
            component.handle_event(
                &Event::Key(KeyStroke::simple(KeyCode::Enter)),
                &layout,
                &mut EventCx::new(&layout),
            ),
            TreeViewOutcome::Selected {
                visible: 0,
                source: 0
            }
        );
        assert_eq!(
            component.handle_event(
                &Event::Key(KeyStroke::simple(KeyCode::Left)),
                &layout,
                &mut EventCx::new(&layout),
            ),
            TreeViewOutcome::Toggled {
                visible: 0,
                source: 0,
                expanded: false,
            }
        );
        assert!(!state.borrow().is_expanded("src"));
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

        assert_eq!(frame.hits().regions(), []);
    }

    #[test]
    fn keyboard_navigation_moves_selection() {
        let items = sample_items();
        let mut initial = TreeViewState::new(Some(0));
        initial.set_focused(true);
        initial.set_expanded("src", true);
        let state = RefCell::new(initial);
        let component = TreeViewComponent::new("tree", &items, &state);
        let layout = component.layout(Constraints::for_width(20), &mut LayoutCx::new());
        for (key, expected) in [(KeyCode::Down, 1), (KeyCode::Up, 0)] {
            assert_eq!(
                component.handle_event(
                    &Event::Key(KeyStroke::simple(key)),
                    &layout,
                    &mut EventCx::new(&layout),
                ),
                TreeViewOutcome::Focused {
                    visible: expected,
                    source: expected,
                }
            );
            assert_eq!(state.borrow().selected_visible(), Some(expected));
        }
    }

    #[test]
    fn directly_dispatched_tree_key_navigates_without_visual_focus() {
        let items = sample_items();
        let mut initial = TreeViewState::new(Some(0));
        initial.set_expanded("src", true);
        let state = RefCell::new(initial);
        let component = TreeViewComponent::new("tree", &items, &state);
        let layout = component.layout(
            Constraints::tight(Rect::new(0, 0, 20, 4).size()),
            &mut LayoutCx::new(),
        );
        let outcome = component.handle_event(
            &Event::Key(KeyStroke::simple(KeyCode::Down)),
            &layout,
            &mut EventCx::new(&layout),
        );

        assert_eq!(
            outcome,
            TreeViewOutcome::Focused {
                visible: 1,
                source: 1,
            }
        );
        assert_eq!(state.borrow().selected_visible(), Some(1));
    }

    #[test]
    fn right_and_left_expand_and_collapse_selected_item() {
        let items = sample_items();
        let mut initial = TreeViewState::new(Some(0));
        initial.set_focused(true);
        let state = RefCell::new(initial);
        let component = TreeViewComponent::new("tree", &items, &state);
        let constraints = Constraints::for_width(20);
        let collapsed = component.layout(constraints, &mut LayoutCx::new());
        assert_eq!(collapsed.size.height, 2);
        assert_eq!(
            component.handle_event(
                &Event::Key(KeyStroke::simple(KeyCode::Right)),
                &collapsed,
                &mut EventCx::new(&collapsed),
            ),
            TreeViewOutcome::Toggled {
                visible: 0,
                source: 0,
                expanded: true,
            }
        );
        assert!(state.borrow().is_expanded("src"));
        let expanded = component.layout(constraints, &mut LayoutCx::new());
        assert_eq!(expanded.size.height, 4);
        assert_eq!(
            component.handle_event(
                &Event::Key(KeyStroke::simple(KeyCode::Left)),
                &expanded,
                &mut EventCx::new(&expanded),
            ),
            TreeViewOutcome::Toggled {
                visible: 0,
                source: 0,
                expanded: false,
            }
        );
        assert!(!state.borrow().is_expanded("src"));
        assert_eq!(
            component.layout(constraints, &mut LayoutCx::new()).size,
            collapsed.size
        );
    }

    #[test]
    fn enter_selects_enabled_item() {
        let items = sample_items();
        let mut initial = TreeViewState::new(Some(1));
        initial.set_focused(true);
        let state = RefCell::new(initial);
        let component = TreeViewComponent::new("tree", &items, &state);
        let layout = component.layout(Constraints::for_width(20), &mut LayoutCx::new());
        assert_eq!(layout.size.height, 2);
        assert_eq!(
            component.handle_event(
                &Event::Key(KeyStroke::simple(KeyCode::Enter)),
                &layout,
                &mut EventCx::new(&layout),
            ),
            TreeViewOutcome::Selected {
                visible: 1,
                source: 3
            }
        );
        assert_eq!(state.borrow().selected_visible(), Some(1));
    }

    #[test]
    fn mouse_click_selects_visible_row() {
        let items = sample_items();
        let mut initial = TreeViewState::new(Some(0));
        initial.set_expanded("src", true);
        let state = RefCell::new(initial);
        let component = TreeViewComponent::new("tree", &items, &state);
        let layout = component.layout(Constraints::for_width(20), &mut LayoutCx::new());
        let mut cx = EventCx::new(&layout);
        assert_eq!(
            component.handle_event(
                &Event::Mouse(MouseEvent::new(
                    MouseEventKind::Down(MouseButton::Left),
                    Point::new(1, 2),
                )),
                &layout,
                &mut cx,
            ),
            TreeViewOutcome::Redraw
        );
        let outcome = component.handle_event(
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Up(MouseButton::Left),
                Point::new(1, 2),
            )),
            &layout,
            &mut cx,
        );

        assert_eq!(
            outcome,
            TreeViewOutcome::Selected {
                visible: 2,
                source: 2
            }
        );
        assert_eq!(state.borrow().selected_visible(), Some(2));
        assert_eq!(state.borrow().pressed_visible, None);
    }

    #[test]
    fn toggle_on_click_expands_and_collapses_expandable_rows_only() {
        let items = sample_items();
        let state = RefCell::new(TreeViewState::new(None));
        let component = TreeViewComponent::new("tree", &items, &state).toggle_on_click(true);
        let click = |row: u16| {
            let layout = component.layout(Constraints::for_width(20), &mut LayoutCx::new());
            let mut cx = EventCx::new(&layout);
            let mut outcome = TreeViewOutcome::Ignored;
            for kind in [
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
            ] {
                outcome = component.handle_event(
                    &Event::Mouse(MouseEvent::new(kind, Point::new(1, row))),
                    &layout,
                    &mut cx,
                );
            }
            outcome
        };
        let collapsed = component.visible_count();
        let source = component.visible_source(0).unwrap();
        assert!(items[source].expandable);
        assert_eq!(
            click(0),
            TreeViewOutcome::Toggled {
                visible: 0,
                source,
                expanded: true
            }
        );
        assert!(component.visible_count() > collapsed);
        assert_eq!(
            state.borrow().selected_id(),
            Some(items[source].id.as_str())
        );
        assert_eq!(
            click(0),
            TreeViewOutcome::Toggled {
                visible: 0,
                source,
                expanded: false
            }
        );
        assert_eq!(component.visible_count(), collapsed);
        // Leaf rows still only select.
        let leaf = (0..component.visible_count())
            .find(|row| !items[component.visible_source(*row).unwrap()].expandable)
            .unwrap();
        let row = u16::try_from(leaf).unwrap();
        assert_eq!(
            click(row),
            TreeViewOutcome::Selected {
                visible: leaf,
                source: component.visible_source(leaf).unwrap()
            }
        );
        assert_eq!(component.visible_count(), collapsed);
    }

    #[test]
    fn disabled_items_do_not_select() {
        let items = [TreeViewItem::new("disabled", "Disabled", 0).disabled(true)];
        let state = RefCell::new(TreeViewState::new(Some(0)));
        let component = TreeViewComponent::new("tree", &items, &state);
        let layout = component.layout(Constraints::for_width(20), &mut LayoutCx::new());
        assert_eq!(
            component.handle_event(
                &Event::Key(KeyStroke::simple(KeyCode::Enter)),
                &layout,
                &mut EventCx::new(&layout),
            ),
            TreeViewOutcome::Ignored
        );
        assert_eq!(state.borrow().selected_visible(), Some(0));
    }

    #[test]
    fn bare_policy_ignores_events() {
        let items = sample_items();
        let state = RefCell::new(TreeViewState::new(Some(0)));
        let component =
            TreeViewComponent::new("tree", &items, &state).policy(TreeViewPolicy::bare());
        let layout = component.layout(Constraints::for_width(20), &mut LayoutCx::new());
        assert_eq!(
            component.handle_event(
                &Event::Key(KeyStroke::simple(KeyCode::Right)),
                &layout,
                &mut EventCx::new(&layout),
            ),
            TreeViewOutcome::Ignored
        );
        assert!(!state.borrow().is_expanded("src"));
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

#[cfg(test)]
mod focus_visual_tests {
    use std::cell::RefCell;

    use bmux_tui::component::Component;
    use bmux_tui::style::{Color, Modifier, Style};

    use super::*;

    #[test]
    fn selected_row_focus_is_an_optional_overlay() {
        let items = [TreeViewItem::new("one", "One", 0)];
        let state = RefCell::new(TreeViewState::new(Some(0)));
        let styles = TreeViewStyles {
            selected: Style::new().bg(Color::Blue),
            ..TreeViewStyles::default()
        };
        let component = TreeViewComponent::new("focus.tree", &items, &state).styles(styles);
        assert_eq!(
            component.tree.row_style(&items[0], &state.borrow(), 0),
            styles.selected
        );
        state.borrow_mut().set_focused(true);
        let focused = component.tree.row_style(&items[0], &state.borrow(), 0);
        assert_eq!(focused.bg, styles.selected.bg);
        assert!(focused.modifiers.contains(Modifier::UNDERLINE));
        let component = component.focus_style(Style::new());
        assert_eq!(
            component.tree.row_style(&items[0], &state.borrow(), 0),
            styles.selected
        );
        state.borrow_mut().interaction.disabled = true;
        assert_eq!(
            component.tree.row_style(&items[0], &state.borrow(), 0),
            styles.disabled
        );
        assert_eq!(state.borrow().selected_visible, Some(0));
    }

    #[test]
    fn focus_state_and_style_are_paint_only() {
        let items = [TreeViewItem::new("one", "One", 0)];
        let state = RefCell::new(TreeViewState::new(Some(0)));
        let component = TreeViewComponent::new("focus.tree", &items, &state);
        let before = component.revision();
        state.borrow_mut().set_focused(true);
        let focused = component.revision();
        let restyled = component
            .focus_style(Style::new().fg(Color::Red))
            .revision();
        assert_eq!(before.layout, focused.layout);
        assert_eq!(focused.layout, restyled.layout);
        assert_ne!(before.paint, focused.paint);
        assert_ne!(focused.paint, restyled.paint);
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::TreeViewState;

    #[test]
    fn native_snapshot_preserves_private_state_and_rejects_bad_input_atomically() {
        let mut state = TreeViewState::new(Some(1));
        state.expanded.push("root".into());
        state.hovered_visible = Some(2);
        state.pressed_visible = Some(2);
        let snapshot = state.snapshot();
        let mut rebuilt = TreeViewState::new(Some(1));
        rebuilt.restore_snapshot(&snapshot).unwrap();
        assert_eq!(rebuilt, state);
        let mut malformed = snapshot.clone();
        malformed["unknown_field"] = serde_json::json!(true);
        assert!(rebuilt.restore_snapshot(&malformed).is_err());
        assert_eq!(rebuilt, state);
        assert!(rebuilt.restore_snapshot(&serde_json::Value::Null).is_err());
        assert_eq!(rebuilt, state);
    }
}

#[cfg(test)]
mod rich_and_scale_tests {
    use std::cell::RefCell;

    use bmux_keyboard::{KeyCode, KeyStroke};
    use bmux_tui::buffer::Buffer;
    use bmux_tui::component::{Component, Constraints, EventCx, LayoutCx, LayoutNode};
    use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};
    use bmux_tui::paint::{LocalRect, PaintCx};
    use bmux_tui::prelude::{Line, Span};
    use bmux_tui::style::{Color, Style};

    use super::{
        TreeViewComponent, TreeViewItem, TreeViewLabelOverflow, TreeViewOutcome, TreeViewState,
        TreeViewStyles, TreeViewTrailingLayout,
    };

    fn red() -> Style {
        Style::new().fg(Color::Red)
    }

    fn blue() -> Style {
        Style::new().fg(Color::Blue)
    }

    /// Region ids and areas registered by one paint.
    type Regions = Vec<(String, Rect)>;

    /// Lay out at a tight width and paint `height` rows starting at logical
    /// row `top`, as an enclosing scroll view would.
    fn paint_window(
        component: &TreeViewComponent<'_, '_>,
        width: u16,
        top: u64,
        height: u16,
    ) -> (LayoutNode, Buffer, Regions, Regions) {
        let layout = component.layout(Constraints::for_width(width.into()), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
        let mut frame = Frame::new(&mut buffer);
        let offset = i64::try_from(top).unwrap();
        PaintCx::new(&mut frame).with_child(
            0,
            -offset,
            LocalRect::new(0, offset, width, height),
            |cx| {
                component.paint(&layout, cx);
            },
        );
        let hits = frame
            .hits()
            .regions()
            .iter()
            .map(|region| (region.id.as_str().to_owned(), region.area))
            .collect();
        let semantics = frame
            .semantics()
            .regions()
            .iter()
            .filter(|region| region.role == "tree-item")
            .map(|region| (region.id.clone(), region.area))
            .collect();
        drop(frame);
        (layout, buffer, hits, semantics)
    }

    fn row(buffer: &Buffer, y: u16) -> String {
        buffer.row_symbols(y).unwrap()
    }

    fn decorated(label: &str) -> TreeViewItem {
        TreeViewItem::new("file", label, 0)
            .trailing_decoration(Line::from_spans([Span::styled("M", red())]))
            .trailing_decoration(Line::from_spans([Span::styled("+12", blue())]))
    }

    #[test]
    fn rich_label_spans_paint_over_row_state_and_yield_to_disabled() {
        let label = Line::from_spans([Span::styled("ab", red()), Span::styled("cd", blue())]);
        let items = [
            TreeViewItem::rich("one", label.clone(), 0),
            TreeViewItem::rich("two", label, 0).disabled(true),
        ];
        assert_eq!(items[0].label, "abcd");
        let state = RefCell::new(TreeViewState::new(Some(0)));
        let styles = TreeViewStyles {
            selected: Style::new().bg(Color::Yellow),
            disabled: Style::new().fg(Color::BrightBlack),
            ..TreeViewStyles::default()
        };
        let component = TreeViewComponent::new("tree", &items, &state).styles(styles);
        let (_, buffer, _, _) = paint_window(&component, 10, 0, 2);
        assert_eq!(row(&buffer, 0), "  abcd    ");
        let cell = |x, y| buffer.get(Point::new(x, y)).unwrap().style;
        assert_eq!(cell(2, 0).fg, Some(Color::Red));
        assert_eq!(cell(4, 0).fg, Some(Color::Blue));
        for x in 0..10 {
            assert_eq!(cell(x, 0).bg, Some(Color::Yellow), "column {x}");
        }
        for x in 2..6 {
            assert_eq!(cell(x, 1).fg, Some(Color::BrightBlack));
        }
    }

    #[test]
    fn trailing_decorations_align_to_the_row_edge_with_their_styles() {
        let items = [decorated("lib.rs")];
        let state = RefCell::new(TreeViewState::new(None));
        let component = TreeViewComponent::new("tree", &items, &state);
        let (_, buffer, hits, _) = paint_window(&component, 20, 0, 1);
        assert_eq!(row(&buffer, 0), "  lib.rs       M +12");
        assert_eq!(
            buffer.get(Point::new(15, 0)).unwrap().style.fg,
            Some(Color::Red)
        );
        for x in 17..20 {
            assert_eq!(
                buffer.get(Point::new(x, 0)).unwrap().style.fg,
                Some(Color::Blue)
            );
        }
        // The row hit region spans the whole row, decorations included.
        assert!(hits.contains(&("tree.file".to_owned(), Rect::new(0, 0, 20, 1))));
    }

    #[test]
    fn intrinsic_width_includes_gap_and_decorations() {
        let items = [decorated("lib.rs")];
        let state = RefCell::new(TreeViewState::new(None));
        for (layout, expected) in [
            (TreeViewTrailingLayout::new(), 2 + 6 + 1 + 1 + 1 + 3),
            (
                TreeViewTrailingLayout::new().gap(3).spacing(0),
                2 + 6 + 3 + 1 + 3,
            ),
        ] {
            let component = TreeViewComponent::new("tree", &items, &state).trailing_layout(layout);
            let node = component.layout(Constraints::new(0, 200, 0, None), &mut LayoutCx::new());
            assert_eq!(node.size.width, expected);
        }
    }

    #[test]
    fn narrow_rows_hide_whole_leading_decorations_before_the_label_minimum() {
        let items = [decorated("lib.rs")];
        let state = RefCell::new(TreeViewState::new(None));
        let component = TreeViewComponent::new("tree", &items, &state);
        // Room is width - 2; the label keeps at least four cells plus the gap.
        for (width, expected) in [
            (14, "  lib.rs M +12"),
            (13, "  lib.r M +12"),
            (12, "  lib. M +12"),
            (11, "  lib.r +12"),
            (10, "  lib. +12"),
            (9, "  lib.rs"),
            (8, "  lib.rs"),
            (6, "  lib."),
            (3, "  l"),
            (2, "  "),
            (1, " "),
        ] {
            let (_, buffer, _, _) = paint_window(&component, width, 0, 1);
            assert_eq!(
                row(&buffer, 0).trim_end(),
                expected.trim_end(),
                "width {width}"
            );
        }
        for width in 0..=30_u16 {
            let (_, buffer, _, _) = paint_window(&component, width, 0, 1);
            if width == 0 {
                continue;
            }
            let painted = row(&buffer, 0);
            let content = painted.get(2..).unwrap_or_default();
            // Decorations appear whole or not at all, and never overlap the label.
            let label_end = content.find(' ').unwrap_or(content.len());
            assert!(
                "lib.rs".starts_with(&content[..label_end]),
                "width {width}: {painted}"
            );
            let trailing = content[label_end..].trim();
            assert!(
                ["", "+12", "M +12"].contains(&trailing),
                "width {width}: {painted}"
            );
        }
    }

    #[test]
    fn ellipsis_overflow_marks_shortened_labels() {
        let items = [
            decorated("main_module.rs"),
            TreeViewItem::new("plain", "README.markdown", 0),
        ];
        let state = RefCell::new(TreeViewState::new(None));
        let component = TreeViewComponent::new("tree", &items, &state).trailing_layout(
            TreeViewTrailingLayout::new().label_overflow(TreeViewLabelOverflow::Ellipsis),
        );
        let (_, buffer, _, _) = paint_window(&component, 16, 0, 2);
        assert_eq!(row(&buffer, 0), "  main_mo… M +12");
        assert_eq!(row(&buffer, 1), "  README.markdo…");
    }

    #[test]
    fn wide_graphemes_keep_decorations_flush_right_without_overlap() {
        let badge = Line::from_spans([Span::styled("🙂3", red())]);
        let items = [
            TreeViewItem::new("cjk", "界界界界界", 0).trailing_decoration(badge.clone()),
            TreeViewItem::new("combining", "e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}", 0)
                .trailing_decoration(badge),
        ];
        let state = RefCell::new(TreeViewState::new(None));
        let component = TreeViewComponent::new("tree", &items, &state);
        for width in 0..=16_u16 {
            let (_, buffer, _, _) = paint_window(&component, width, 0, 2);
            for y in 0..2 {
                let shown = row(&buffer, y).contains('🙂');
                // Room = width - 2; label minimum 4 (cells) + gap 1 + badge 3.
                assert_eq!(shown, width >= 10, "width {width} row {y}");
                if shown {
                    // The badge always ends at the last column.
                    assert_eq!(buffer.get(Point::new(width - 1, y)).unwrap().symbol, "3");
                    assert_eq!(buffer.get(Point::new(width - 3, y)).unwrap().symbol, "🙂");
                    assert_eq!(buffer.get(Point::new(width - 4, y)).unwrap().symbol, " ");
                }
            }
        }
        // An odd label room omits the straddling wide grapheme instead of
        // overlapping the gap: room 9 leaves 5 label cells for "界界" (4),
        // and the spare cell becomes padding before the badge.
        let (_, buffer, _, _) = paint_window(&component, 11, 0, 2);
        assert_eq!(row(&buffer, 0), "  界界  🙂3");
        assert_eq!(
            row(&buffer, 1),
            "  e\u{301}e\u{301}e\u{301}e\u{301}e\u{301} 🙂3"
        );
    }

    #[test]
    fn oversized_prefix_hides_label_and_decorations_inside_the_row() {
        let items = [decorated("lib.rs")];
        let deep = [TreeViewItem {
            depth: 40,
            ..items[0].clone()
        }];
        let state = RefCell::new(TreeViewState::new(None));
        let component = TreeViewComponent::new("tree", &deep, &state);
        for width in [1_u16, 10, 80, 82, 90] {
            let (_, buffer, hits, _) = paint_window(&component, width, 0, 1);
            let painted = row(&buffer, 0);
            assert_eq!(
                bmux_tui::text_width::display_width(&painted),
                usize::from(width)
            );
            if width <= 82 {
                assert!(painted.trim().is_empty(), "width {width}: {painted}");
            } else {
                assert!(painted.contains('l'));
            }
            assert!(hits.iter().all(|(_, area)| area.width <= width));
        }
    }

    #[test]
    fn zero_width_registers_nothing() {
        let items = [decorated("lib.rs")];
        let state = RefCell::new(TreeViewState::new(None));
        let component = TreeViewComponent::new("tree", &items, &state);
        let (layout, _, hits, semantics) = paint_window(&component, 0, 0, 1);
        assert_eq!(layout.size.width, 0);
        assert_eq!(hits, []);
        assert_eq!(semantics, []);
    }

    #[test]
    fn resize_restores_hidden_decorations_and_hits_follow_painted_rows() {
        let items = [decorated("lib.rs"), decorated("main.rs")];
        let state = RefCell::new(TreeViewState::new(None));
        let component = TreeViewComponent::new("tree", &items, &state);
        for (width, shown) in [(30, true), (8, false), (30, true)] {
            let (_, buffer, hits, semantics) = paint_window(&component, width, 0, 2);
            for y in 0..2 {
                assert_eq!(row(&buffer, y).contains("+12"), shown, "width {width}");
            }
            let rows = hits
                .iter()
                .filter(|(id, _)| id != "tree")
                .map(|(_, area)| *area)
                .collect::<Vec<_>>();
            assert_eq!(
                rows,
                vec![Rect::new(0, 0, width, 1), Rect::new(0, 1, width, 1)]
            );
            assert_eq!(
                semantics.iter().map(|(_, area)| *area).collect::<Vec<_>>(),
                rows
            );
        }
    }

    fn large_tree() -> Vec<TreeViewItem> {
        // 500 directories of 100 files each, plus a root-level file.
        let mut items = Vec::new();
        for directory in 0..500 {
            items.push(
                TreeViewItem::new(format!("d{directory}"), format!("dir{directory}"), 0)
                    .expandable(true),
            );
            for file in 0..100 {
                items.push(
                    TreeViewItem::new(
                        format!("d{directory}/f{file}"),
                        format!("file{directory}-{file}"),
                        1,
                    )
                    .trailing_decoration(Line::raw(format!("+{file}"))),
                );
            }
        }
        items.push(TreeViewItem::new("tail", "tail", 0));
        items
    }

    fn click(
        component: &TreeViewComponent<'_, '_>,
        layout: &LayoutNode,
        top: u64,
        height: u16,
        point: Point,
    ) -> TreeViewOutcome {
        let mut cx = EventCx::new(layout);
        let width = u16::try_from(layout.size.width).unwrap();
        let offset = i64::try_from(top).unwrap();
        cx.with_transform(0, 0, 0, -offset, Rect::new(0, 0, width, height), |cx| {
            let mut outcome = TreeViewOutcome::Ignored;
            for kind in [
                MouseEventKind::Move,
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
            ] {
                outcome =
                    component.handle_event(&Event::Mouse(MouseEvent::new(kind, point)), layout, cx);
            }
            outcome
        })
    }

    #[test]
    fn large_scrolled_tree_paints_and_hits_only_visible_rows_in_agreement() {
        let items = large_tree();
        let state = RefCell::new(TreeViewState::new(None));
        state
            .borrow_mut()
            .set_expanded_ids((0..500).map(|directory| format!("d{directory}")));
        let component = TreeViewComponent::new("files", &items, &state).items_revision(1);
        assert_eq!(component.visible_count(), items.len());
        for top in [0_u64, 1, 25_000, 50_490] {
            let (layout, buffer, hits, semantics) = paint_window(&component, 24, top, 10);
            assert_eq!(layout.size.height, 50_501);
            // One composite plus one region per painted row; nothing offscreen.
            assert_eq!(hits.len(), 11);
            assert_eq!(semantics.len(), 10);
            for y in 0..10_u16 {
                let logical = usize::try_from(top).unwrap() + usize::from(y);
                let source = component.visible_source(logical).unwrap();
                let item = &items[source];
                assert!(row(&buffer, y).contains(&item.label), "row {logical}");
                assert_eq!(
                    semantics[usize::from(y)],
                    (format!("files.{}", item.id), Rect::new(0, y, 24, 1))
                );
                // The pointer selects exactly the painted item.
                let outcome = click(&component, &layout, top, 10, Point::new(3, y));
                assert_eq!(
                    outcome,
                    TreeViewOutcome::Selected {
                        visible: logical,
                        source
                    }
                );
                assert_eq!(state.borrow().selected_id(), Some(item.id.as_str()));
            }
            // Below the viewport clip nothing is hit and the selection stays.
            let selected = state.borrow().selected_id().map(str::to_owned);
            assert_eq!(
                click(&component, &layout, top, 10, Point::new(3, 10)),
                TreeViewOutcome::Ignored
            );
            assert_eq!(state.borrow().selected_id(), selected.as_deref());
        }
    }

    #[test]
    fn keyed_selection_follows_its_item_across_expansion_above_it() {
        let items = large_tree();
        let state = RefCell::new(TreeViewState::new(None));
        let component = TreeViewComponent::new("files", &items, &state).items_revision(7);
        assert_eq!(component.visible_count(), 501);
        state.borrow_mut().set_selected_id("tail");
        assert_eq!(component.selected_visible(), Some(500));
        state.borrow_mut().set_expanded("d3", true);
        assert_eq!(component.selected_visible(), Some(600));
        state.borrow_mut().set_expanded("d3", false);
        assert_eq!(component.selected_visible(), Some(500));
        // Keyboard selection is keyed too.
        let layout = component.layout(Constraints::for_width(20), &mut LayoutCx::new());
        let key = |code| {
            component.handle_event(
                &Event::Key(KeyStroke::simple(code)),
                &layout,
                &mut EventCx::new(&layout),
            )
        };
        state.borrow_mut().set_selected_visible(Some(2));
        assert_eq!(state.borrow().selected_id(), None);
        assert_eq!(
            key(KeyCode::Down),
            TreeViewOutcome::Focused {
                visible: 3,
                source: 303
            }
        );
        assert_eq!(state.borrow().selected_id(), Some("d3"));
        state.borrow_mut().set_expanded("d0", true);
        assert_eq!(component.selected_visible(), Some(103));
        // A hidden selection selects no row, keeps its id, and returns with it.
        state.borrow_mut().set_expanded("d1", true);
        state.borrow_mut().set_selected_id("d1/f5");
        assert_eq!(component.selected_visible(), Some(107));
        state.borrow_mut().set_expanded("d1", false);
        assert_eq!(component.selected_visible(), None);
        assert_eq!(state.borrow().selected_id(), Some("d1/f5"));
        state.borrow_mut().set_expanded("d1", true);
        assert_eq!(component.selected_visible(), Some(107));
        // Painting reflects the resolved keyed row.
        let styles = TreeViewStyles {
            selected: Style::new().bg(Color::Yellow),
            ..TreeViewStyles::default()
        };
        let component = component.styles(styles);
        let (_, buffer, _, _) = paint_window(&component, 20, 100, 10);
        assert_eq!(
            buffer.get(Point::new(0, 7)).unwrap().style.bg,
            Some(Color::Yellow)
        );
        assert_ne!(
            buffer.get(Point::new(0, 6)).unwrap().style.bg,
            Some(Color::Yellow)
        );
    }

    #[test]
    fn items_revision_replaces_item_hashing_and_expansion_invalidates_projection() {
        let items = large_tree();
        let mut relabeled = items.clone();
        relabeled[1].label = "renamed".to_owned();
        let state = RefCell::new(TreeViewState::new(None));
        let original = TreeViewComponent::new("files", &items, &state).items_revision(1);
        let changed = TreeViewComponent::new("files", &relabeled, &state).items_revision(1);
        assert_eq!(original.revision(), changed.revision());
        let bumped = TreeViewComponent::new("files", &relabeled, &state).items_revision(2);
        assert_ne!(original.revision().layout, bumped.revision().layout);
        // Without a revision, item content is hashed.
        assert_ne!(
            TreeViewComponent::new("files", &items, &state).revision(),
            TreeViewComponent::new("files", &relabeled, &state).revision()
        );
        // Decoration changes count as layout changes when hashed.
        let mut decorated = items.clone();
        decorated[1].trailing.push(Line::raw("!"));
        assert_ne!(
            TreeViewComponent::new("files", &items, &state)
                .revision()
                .layout,
            TreeViewComponent::new("files", &decorated, &state)
                .revision()
                .layout
        );
        // The retained projection never outlives an expansion change.
        assert_eq!(original.visible_count(), 501);
        state.borrow_mut().set_expanded("d0", true);
        assert_eq!(original.visible_count(), 601);
        assert_eq!(original.visible_source(1), Some(1));
        state.borrow_mut().set_expanded_ids(Vec::new());
        assert_eq!(original.visible_count(), 501);
        assert_eq!(original.visible_row_of("tail"), Some(500));
        assert_eq!(original.visible_row_of("d0/f0"), None);
    }

    #[test]
    fn items_revision_retains_one_projection_across_frame_phases() {
        let items = large_tree();
        let state = RefCell::new(TreeViewState::new(None));
        state.borrow_mut().set_selected_id("tail");
        let component = TreeViewComponent::new("files", &items, &state).items_revision(3);
        let _ = component.revision();
        let retained = std::sync::Arc::clone(state.borrow().cache.projection.as_ref().unwrap());
        let (layout, _, _, _) = paint_window(&component, 20, 400, 5);
        let _ = component.revision();
        let _ = click(&component, &layout, 400, 5, Point::new(1, 2));
        let current = std::sync::Arc::clone(state.borrow().cache.projection.as_ref().unwrap());
        assert!(std::sync::Arc::ptr_eq(&retained, &current));
        // Without a revision nothing is retained.
        let state = RefCell::new(TreeViewState::new(None));
        let component = TreeViewComponent::new("files", &items, &state);
        let _ = component.visible_count();
        assert!(state.borrow().cache.projection.is_none());
    }

    #[test]
    fn bulk_expansion_deduplicates_and_snapshots_keep_keyed_selection() {
        let mut state = TreeViewState::new(None);
        state.set_expanded_ids(["b", "a", "b"].map(str::to_owned));
        assert_eq!(state.expanded_ids(), ["b", "a"]);
        assert!(state.is_expanded("a") && state.is_expanded("b") && !state.is_expanded("c"));
        state.set_expanded("c", true);
        state.set_expanded("b", false);
        assert_eq!(state.expanded_ids(), ["a", "c"]);
        state.set_selected_id("c/x");
        let snapshot = state.snapshot();
        let mut restored = TreeViewState::new(None);
        restored.restore_snapshot(&snapshot).unwrap();
        assert_eq!(restored, state);
        assert_eq!(restored.selected_id(), Some("c/x"));
        assert!(restored.is_expanded("c"));
        // Snapshots written before keyed selection existed still restore.
        let mut legacy = snapshot;
        legacy.as_object_mut().unwrap().remove("selected_id");
        restored.restore_snapshot(&legacy).unwrap();
        assert_eq!(restored.selected_id(), None);
        restored.clear_selection();
        assert_eq!(restored.selected_visible(), None);
    }
}
