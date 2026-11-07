//! Resizable panel-group layout and divider interaction.

use std::cell::RefCell;
use std::hash::{Hash, Hasher};

use bmux_tui::component::{
    ChildLayout, Component, ComponentRevision, Constraints, Element, EventCx, LayoutCx, LayoutId,
    LayoutNode, LogicalSize,
};
use bmux_tui::event::{Event, EventOutcome, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::hit::{HitRegion as SceneRegion, HitRole};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::style::{Color, Modifier, Style};

use crate::common::u16_saturating;

use crate::common::DragState;
use crate::selection::{
    ComponentSelectionOutcome, ComponentSelectionPolicy, ComponentSelectionState,
    paint_component_scope,
};

/// Direction panels are laid out in a [`PanelGroup`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PanelGroupAxis {
    /// Panels are laid left-to-right with vertical dividers.
    Horizontal,
    /// Panels are laid top-to-bottom with horizontal dividers.
    Vertical,
}

/// How dividers occupy space between panels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PanelDividerMode {
    /// A one-cell divider line between adjacent panels.
    #[default]
    Gap,
    /// Dividers take no layout space. Each handle overlays the `before` trailing cells of
    /// the earlier panel and the `after` leading cells of the later one, typically their
    /// adjacent borders, and is highlighted in place (symbols are kept) while hovered or
    /// dragged. A zero-width handle is widened to one cell.
    Overlay {
        /// Trailing cells of the earlier panel covered by the handle.
        before: u16,
        /// Leading cells of the later panel covered by the handle.
        after: u16,
    },
}

/// Requested panel size along the group's primary axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PanelSize {
    /// Fixed cell count.
    Fixed(u16),
    /// Weighted share of remaining cells after fixed panels and dividers.
    Flex(u16),
}

impl PanelSize {
    /// Return a fixed panel size.
    #[must_use]
    pub const fn fixed(cells: u16) -> Self {
        Self::Fixed(cells)
    }

    /// Return a flex panel size. A zero weight is treated as one.
    #[must_use]
    pub const fn flex(weight: u16) -> Self {
        Self::Flex(weight)
    }
}

/// Per-panel resize limits along the group's primary axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PanelGroupConstraints {
    /// Minimum panel size in cells.
    pub min: u16,
    /// Optional maximum panel size in cells.
    pub max: Option<u16>,
}

impl PanelGroupConstraints {
    /// Create constraints.
    #[must_use]
    pub const fn new(min: u16, max: Option<u16>) -> Self {
        Self { min, max }
    }

    fn clamp(self, value: u16) -> u16 {
        let value = value.max(self.min);
        self.max.map_or(value, |max| value.min(max))
    }
}

impl Default for PanelGroupConstraints {
    fn default() -> Self {
        Self { min: 1, max: None }
    }
}

/// Mouse behavior policy for [`PanelGroup`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct PanelGroupMousePolicy {
    /// Whether mouse events are accepted.
    pub enabled: bool,
    /// Whether move events update hovered divider state.
    pub hover_dividers: bool,
    /// Whether clicking a panel focuses it.
    pub click_to_focus: bool,
    /// Whether dividers can be dragged to resize adjacent panels.
    pub drag_dividers: bool,
}

impl PanelGroupMousePolicy {
    /// Mouse behavior disabled.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            hover_dividers: false,
            click_to_focus: false,
            drag_dividers: false,
        }
    }

    /// Mouse divider resizing enabled.
    #[must_use]
    pub const fn resize_only() -> Self {
        Self {
            enabled: true,
            hover_dividers: true,
            click_to_focus: false,
            drag_dividers: true,
        }
    }

    /// Mouse focus and divider resizing enabled.
    #[must_use]
    pub const fn interactive() -> Self {
        Self {
            enabled: true,
            hover_dividers: true,
            click_to_focus: true,
            drag_dividers: true,
        }
    }
}

impl Default for PanelGroupMousePolicy {
    fn default() -> Self {
        Self::disabled()
    }
}

/// Keyboard behavior policy for [`PanelGroup`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PanelGroupKeyboardPolicy {
    /// Whether keyboard events are accepted.
    pub enabled: bool,
}

impl PanelGroupKeyboardPolicy {
    /// Keyboard behavior disabled.
    #[must_use]
    pub const fn disabled() -> Self {
        Self { enabled: false }
    }
}

impl Default for PanelGroupKeyboardPolicy {
    fn default() -> Self {
        Self::disabled()
    }
}

/// Focus behavior policy for [`PanelGroup`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PanelGroupFocusPolicy {
    /// Whether panel focus can be tracked.
    pub enabled: bool,
}

impl PanelGroupFocusPolicy {
    /// Focus tracking disabled.
    #[must_use]
    pub const fn disabled() -> Self {
        Self { enabled: false }
    }

    /// Focus tracking enabled.
    #[must_use]
    pub const fn enabled() -> Self {
        Self { enabled: true }
    }
}

impl Default for PanelGroupFocusPolicy {
    fn default() -> Self {
        Self::disabled()
    }
}

/// Resize behavior policy for [`PanelGroup`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PanelGroupResizePolicy {
    /// Whether resizing is allowed.
    pub enabled: bool,
    /// Whether resizing happens continuously during drag events.
    pub live_resize: bool,
    /// Keep flex panels flexible when resized: their weights are rebased to the cell
    /// lengths they had, so the resized proportions survive later area changes. Without
    /// it, resized panels become fixed sizes.
    pub preserve_flex: bool,
}

impl PanelGroupResizePolicy {
    /// Resize behavior disabled.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            live_resize: false,
            preserve_flex: false,
        }
    }

    /// Live divider resize behavior enabled.
    #[must_use]
    pub const fn live() -> Self {
        Self {
            enabled: true,
            live_resize: true,
            preserve_flex: false,
        }
    }

    /// Return this policy with flex preservation set; see [`Self::preserve_flex`].
    #[must_use]
    pub const fn preserving_flex(mut self, preserve: bool) -> Self {
        self.preserve_flex = preserve;
        self
    }
}

impl Default for PanelGroupResizePolicy {
    fn default() -> Self {
        Self::disabled()
    }
}

/// Behavior policy for [`PanelGroup`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PanelGroupPolicy {
    /// Mouse policy.
    pub mouse: PanelGroupMousePolicy,
    /// Keyboard policy.
    pub keyboard: PanelGroupKeyboardPolicy,
    /// Focus policy.
    pub focus: PanelGroupFocusPolicy,
    /// Resize policy.
    pub resize: PanelGroupResizePolicy,
}

impl PanelGroupPolicy {
    /// Layout-only policy.
    #[must_use]
    pub const fn bare() -> Self {
        Self {
            mouse: PanelGroupMousePolicy::disabled(),
            keyboard: PanelGroupKeyboardPolicy::disabled(),
            focus: PanelGroupFocusPolicy::disabled(),
            resize: PanelGroupResizePolicy::disabled(),
        }
    }

    /// Divider mouse-resize policy without panel focus.
    #[must_use]
    pub const fn resize_only() -> Self {
        Self {
            mouse: PanelGroupMousePolicy::resize_only(),
            keyboard: PanelGroupKeyboardPolicy::disabled(),
            focus: PanelGroupFocusPolicy::disabled(),
            resize: PanelGroupResizePolicy::live(),
        }
    }

    /// Mouse focus and divider resize policy.
    #[must_use]
    pub const fn interactive() -> Self {
        Self {
            mouse: PanelGroupMousePolicy::interactive(),
            keyboard: PanelGroupKeyboardPolicy::disabled(),
            focus: PanelGroupFocusPolicy::enabled(),
            resize: PanelGroupResizePolicy::live(),
        }
    }
}

/// Visual styles for [`PanelGroup`] divider rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PanelGroupStyles {
    /// Default divider style.
    pub divider: Style,
    /// Hovered divider style.
    pub hovered_divider: Style,
    /// Active drag divider style.
    pub active_divider: Style,
}

impl Default for PanelGroupStyles {
    fn default() -> Self {
        Self {
            divider: Style::new().fg(Color::BrightBlack),
            hovered_divider: Style::new().fg(Color::Cyan),
            active_divider: Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        }
    }
}

/// Runtime state for [`PanelGroup`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelGroupState {
    sizes: Vec<PanelSize>,
    constraints: Vec<PanelGroupConstraints>,
    /// Size to restore for each collapsed panel.
    collapsed: Vec<Option<PanelSize>>,
    focused_panel: Option<usize>,
    hovered_divider: Option<usize>,
    active_drag: Option<PanelDividerDrag>,
}

impl PanelGroupState {
    /// Create state from per-panel requested sizes.
    #[must_use]
    pub fn new(sizes: impl Into<Vec<PanelSize>>) -> Self {
        let sizes = sizes.into();
        Self {
            constraints: vec![PanelGroupConstraints::default(); sizes.len()],
            collapsed: vec![None; sizes.len()],
            sizes,
            focused_panel: None,
            hovered_divider: None,
            active_drag: None,
        }
    }

    /// Return panel sizes.
    #[must_use]
    pub fn sizes(&self) -> &[PanelSize] {
        &self.sizes
    }

    /// Replace every panel size, keeping constraints and expanding collapsed panels whose
    /// index no longer exists.
    pub fn set_sizes(&mut self, sizes: impl Into<Vec<PanelSize>>) {
        self.sizes = sizes.into();
        self.collapsed.resize(self.sizes.len(), None);
        if self
            .active_drag
            .is_some_and(|drag| drag.divider + 1 >= self.sizes.len())
        {
            self.active_drag = None;
        }
    }

    /// Replace one panel size; a collapsed panel instead updates the size it restores to.
    pub fn set_size(&mut self, index: usize, size: PanelSize) {
        match self.collapsed.get_mut(index) {
            Some(Some(restore)) => *restore = size,
            _ => {
                if let Some(slot) = self.sizes.get_mut(index) {
                    *slot = size;
                }
            }
        }
    }

    /// Collapse a panel to `cells` along the primary axis, ignoring its constraints, and
    /// remember its size for [`Self::expand`]. Dividers next to a collapsed panel do not
    /// resize. Returns whether the panel was expanded before.
    pub fn collapse(&mut self, index: usize, cells: u16) -> bool {
        let Some(size) = self.sizes.get(index).copied() else {
            return false;
        };
        if self.collapsed.len() < self.sizes.len() {
            self.collapsed.resize(self.sizes.len(), None);
        }
        if self.collapsed[index].is_some() {
            self.sizes[index] = PanelSize::Fixed(cells);
            return false;
        }
        self.collapsed[index] = Some(size);
        self.sizes[index] = PanelSize::Fixed(cells);
        if self
            .active_drag
            .is_some_and(|drag| drag.divider == index || drag.divider + 1 == index)
        {
            self.active_drag = None;
        }
        true
    }

    /// Restore a collapsed panel's previous size. Returns whether it was collapsed.
    pub fn expand(&mut self, index: usize) -> bool {
        match self.collapsed.get_mut(index).and_then(Option::take) {
            Some(size) => {
                self.sizes[index] = size;
                true
            }
            None => false,
        }
    }

    /// Whether a panel is collapsed.
    #[must_use]
    pub fn is_collapsed(&self, index: usize) -> bool {
        self.collapsed.get(index).is_some_and(Option::is_some)
    }

    /// The size a panel has when expanded: its current size, or the size it restores to.
    #[must_use]
    pub fn expanded_size(&self, index: usize) -> Option<PanelSize> {
        self.collapsed
            .get(index)
            .copied()
            .flatten()
            .or_else(|| self.sizes.get(index).copied())
    }

    /// Whether the divider between `divider` and `divider + 1` can resize them.
    #[must_use]
    pub fn divider_resizable(&self, divider: usize) -> bool {
        divider + 1 < self.sizes.len()
            && !self.is_collapsed(divider)
            && !self.is_collapsed(divider + 1)
    }

    /// Replace per-panel constraints.
    pub fn set_constraints(&mut self, constraints: impl Into<Vec<PanelGroupConstraints>>) {
        self.constraints = constraints.into();
    }

    /// Return focused panel index.
    #[must_use]
    pub const fn focused_panel(&self) -> Option<usize> {
        self.focused_panel
    }

    /// Return hovered divider index.
    #[must_use]
    pub const fn hovered_divider(&self) -> Option<usize> {
        self.hovered_divider
    }

    /// Return active drag divider index.
    #[must_use]
    pub const fn active_divider(&self) -> Option<usize> {
        match self.active_drag {
            Some(drag) => Some(drag.divider),
            None => None,
        }
    }

    fn constraint(&self, index: usize) -> PanelGroupConstraints {
        self.constraints.get(index).copied().unwrap_or_default()
    }
}

/// Active divider drag state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PanelDividerDrag {
    /// Divider being dragged.
    pub divider: usize,
    /// Pointer drag state.
    pub drag: DragState,
}

/// Computed panel-group layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelGroupLayout {
    /// Child panel rectangles.
    pub panels: Vec<Rect>,
    /// Divider rectangles between panels.
    pub dividers: Vec<Rect>,
}

/// Panel-group event result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelGroupOutcome {
    /// Event was ignored.
    Ignored,
    /// Event was handled without a more specific outcome.
    Handled,
    /// Visual state changed and should be redrawn.
    Redraw,
    /// Panel focus changed.
    Focused { panel: usize },
    /// Divider drag started.
    DividerDragStarted { divider: usize },
    /// Adjacent panel sizes changed.
    Resized {
        divider: usize,
        before: u16,
        after: u16,
    },
    /// Divider drag ended.
    DividerDragEnded { divider: usize },
}

/// Configurable resizable panel-group component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelGroup {
    axis: PanelGroupAxis,
    policy: PanelGroupPolicy,
    styles: PanelGroupStyles,
    divider_mode: PanelDividerMode,
}

/// Child-owning panel group on the canonical component lifecycle.
///
/// Panel placement, divider painting, hit registration, and event routing all
/// consume the same resolved [`LayoutNode`]. Interactive state remains
/// caller-owned.
pub struct PanelGroupComponent<'a> {
    pointer_intent: bmux_tui::pointer::PointerIntent,
    id: LayoutId,
    group: PanelGroup,
    state: &'a RefCell<PanelGroupState>,
    children: Vec<Element<'a>>,
}

impl<'a> PanelGroupComponent<'a> {
    /// Override pointer presentation without changing input routing or mouse policy.
    #[must_use]
    pub const fn pointer_intent(mut self, intent: bmux_tui::pointer::PointerIntent) -> Self {
        self.pointer_intent = intent;
        self
    }

    /// Create a component using `group` policy and caller-owned `state`.
    #[must_use]
    pub fn new(
        id: impl Into<LayoutId>,
        group: PanelGroup,
        state: &'a RefCell<PanelGroupState>,
    ) -> Self {
        Self {
            id: id.into(),
            pointer_intent: bmux_tui::pointer::PointerIntent::Auto,
            group,
            state,
            children: Vec::new(),
        }
    }

    /// Append one panel child in primary-axis order.
    #[must_use]
    pub fn child(mut self, child: impl Component + 'a) -> Self {
        self.children.push(Element::new(child));
        self
    }

    fn local_area(layout: &LayoutNode) -> Rect {
        Rect::new(
            0,
            0,
            layout.size.width.try_into().unwrap_or(u16::MAX),
            u16::try_from(layout.size.height).unwrap_or(u16::MAX),
        )
    }

    fn panel_rects(layout: &LayoutNode) -> Vec<Rect> {
        layout
            .children
            .iter()
            .map(|child| {
                Rect::new(
                    u16::try_from(child.x).unwrap_or(u16::MAX),
                    u16::try_from(child.y).unwrap_or(u16::MAX),
                    child.node.size.width.try_into().unwrap_or(u16::MAX),
                    u16::try_from(child.node.size.height).unwrap_or(u16::MAX),
                )
            })
            .collect()
    }

    fn divider_rects(&self, layout: &LayoutNode) -> Vec<Rect> {
        let width = u16::try_from(layout.size.width).unwrap_or(u16::MAX);
        let height = u16::try_from(layout.size.height).unwrap_or(u16::MAX);
        layout
            .children
            .windows(2)
            .map(|pair| match self.group.axis {
                PanelGroupAxis::Horizontal => self.group.divider_rect(
                    u16::try_from(pair[0].x.saturating_add(pair[0].node.size.width))
                        .unwrap_or(u16::MAX),
                    0,
                    width,
                    (0, height),
                ),
                PanelGroupAxis::Vertical => self.group.divider_rect(
                    u16::try_from(pair[0].y.saturating_add(pair[0].node.size.height))
                        .unwrap_or(u16::MAX),
                    0,
                    height,
                    (0, width),
                ),
            })
            .collect()
    }

    fn resolved_layout(&self, layout: &LayoutNode) -> PanelGroupLayout {
        PanelGroupLayout {
            panels: Self::panel_rects(layout),
            dividers: self.divider_rects(layout),
        }
    }

    fn paint_interaction(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let resolved = self.resolved_layout(layout);
        let state = self.state.borrow();
        if self.group.policy.focus.enabled && self.group.policy.mouse.click_to_focus {
            for (index, panel) in resolved.panels.iter().copied().enumerate() {
                cx.push_hit(
                    SceneRegion::new(format!("{}.panel.{index}", self.id.as_str()), panel)
                        .role(HitRole::Background)
                        .pointer_intent(self.pointer_intent)
                        .focusable(false),
                );
            }
        }
        let divider_interactive = self.group.policy.mouse.enabled
            && (self.group.policy.mouse.hover_dividers
                || (self.group.policy.mouse.drag_dividers && self.group.policy.resize.enabled));
        for (index, divider) in resolved.dividers.iter().copied().enumerate() {
            // Dividers beside a collapsed panel neither resize nor react to the pointer.
            let resizable = state.divider_resizable(index);
            if divider_interactive && resizable {
                cx.push_hit(
                    SceneRegion::new(format!("{}.divider.{index}", self.id.as_str()), divider)
                        .role(HitRole::ResizeHandle)
                        .pointer_intent(match self.pointer_intent {
                            bmux_tui::pointer::PointerIntent::Auto => {
                                bmux_tui::pointer::PointerIntent::Shape(match self.group.axis {
                                    PanelGroupAxis::Horizontal => {
                                        bmux_tui::pointer::PointerShape::EwResize
                                    }
                                    PanelGroupAxis::Vertical => {
                                        bmux_tui::pointer::PointerShape::NsResize
                                    }
                                })
                            }
                            intent => intent,
                        })
                        .hoverable(self.group.policy.mouse.hover_dividers)
                        .focusable(false),
                );
            }
            let highlight = if !resizable {
                None
            } else if state.active_divider() == Some(index) {
                Some(self.group.styles.active_divider)
            } else if state.hovered_divider == Some(index) {
                Some(self.group.styles.hovered_divider)
            } else {
                None
            };
            let local = LocalRect::new(
                i32::from(divider.x),
                i64::from(divider.y),
                divider.width,
                divider.height,
            );
            if matches!(self.group.divider_mode, PanelDividerMode::Overlay { .. }) {
                // The handle sits on the panels' own chrome: highlight it in place.
                if let Some(style) = highlight {
                    cx.restyle(local, style);
                }
                continue;
            }
            let style = highlight.unwrap_or(self.group.styles.divider);
            let symbol = match self.group.axis {
                PanelGroupAxis::Horizontal => "│",
                PanelGroupAxis::Vertical => "─",
            };
            cx.fill(local, symbol, style);
        }
    }
}

impl Component for PanelGroupComponent<'_> {
    fn revision(&self) -> ComponentRevision {
        let state = self.state.borrow();
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        self.group.axis.hash(&mut layout);
        state.sizes.hash(&mut layout);
        state.constraints.hash(&mut layout);
        for child in &self.children {
            child.revision().layout.hash(&mut layout);
        }
        let mut paint = std::collections::hash_map::DefaultHasher::new();
        format!("{:?}", self.pointer_intent).hash(&mut paint);
        format!("{:?}", self.group.policy).hash(&mut paint);
        format!("{:?}", self.group.styles).hash(&mut paint);
        state.focused_panel.hash(&mut paint);
        state.hovered_divider.hash(&mut paint);
        format!("{:?}", state.active_drag).hash(&mut paint);
        for child in &self.children {
            child.revision().paint.hash(&mut paint);
        }
        ComponentRevision::new(layout.finish(), paint.finish())
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        let state = self.state.borrow();
        let count = self.children.len().min(state.sizes.len());
        if count == 0 {
            return LayoutNode::leaf(
                self.id.clone(),
                constraints.constrain(LogicalSize::new(constraints.max_width(), 0)),
            );
        }
        let primary_max = match self.group.axis {
            PanelGroupAxis::Horizontal => constraints.max_width(),
            PanelGroupAxis::Vertical => u16::try_from(
                constraints
                    .max_height()
                    .unwrap_or_else(|| u64::try_from(usize::from(u16::MAX)).unwrap_or(u64::MAX)),
            )
            .unwrap_or(u16::MAX)
            .into(),
        };
        let available =
            primary_max.saturating_sub(u64::from(self.group.reserved_divider_cells(count)));
        let gap = usize::from(matches!(self.group.divider_mode, PanelDividerMode::Gap));
        let lengths = allocated_lengths_for_count(
            u16::try_from(available).unwrap_or(u16::MAX),
            &state,
            count,
        );
        let mut children = Vec::with_capacity(count);
        let mut cursor = 0usize;
        let mut cross = 0usize;
        for (index, child) in self.children.iter().take(count).enumerate() {
            let primary = lengths[index];
            let child_constraints = match self.group.axis {
                PanelGroupAxis::Horizontal => Constraints::new(
                    primary.into(),
                    primary.into(),
                    constraints.min_height(),
                    constraints.max_height(),
                ),
                PanelGroupAxis::Vertical => Constraints::new(
                    constraints.min_width(),
                    constraints.max_width(),
                    u64::try_from(usize::from(primary)).unwrap_or(u64::MAX),
                    Some(u64::try_from(usize::from(primary)).unwrap_or(u64::MAX)),
                ),
            };
            let node = child.layout(child_constraints, cx);
            match self.group.axis {
                PanelGroupAxis::Horizontal => {
                    cross = cross.max(node.size.height.try_into().unwrap_or(usize::MAX));
                    children.push(ChildLayout::new(
                        cursor.try_into().unwrap_or(u64::MAX),
                        0,
                        node,
                    ));
                }
                PanelGroupAxis::Vertical => {
                    cross = cross.max(usize::try_from(node.size.width).unwrap_or(usize::MAX));
                    children.push(ChildLayout::new(
                        0,
                        cursor.try_into().unwrap_or(u64::MAX),
                        node,
                    ));
                }
            }
            cursor = cursor
                .saturating_add(usize::from(primary))
                .saturating_add(if index + 1 < count { gap } else { 0 });
        }
        let proposed = match self.group.axis {
            PanelGroupAxis::Horizontal => LogicalSize::new(
                u16::try_from(cursor).unwrap_or(u16::MAX).into(),
                cross.try_into().unwrap_or(u64::MAX),
            ),
            PanelGroupAxis::Vertical => LogicalSize::new(
                u16::try_from(cross).unwrap_or(u16::MAX).into(),
                cursor.try_into().unwrap_or(u64::MAX),
            ),
        };
        LayoutNode::with_children(self.id.clone(), constraints.constrain(proposed), children)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        for (child, component) in layout.children.iter().zip(&self.children) {
            cx.with_child(
                i32::try_from(child.x).unwrap_or(i32::MAX),
                i64::try_from(child.y).unwrap_or(i64::MAX),
                LocalRect::new(
                    0,
                    0,
                    child.node.size.width.try_into().unwrap_or(u16::MAX),
                    u16::try_from(child.node.size.height).unwrap_or(u16::MAX),
                ),
                |cx| component.paint(&child.node, cx),
            );
        }
        self.paint_interaction(layout, cx);
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        let area = cx
            .find_visible_rect(&self.id)
            .unwrap_or_else(|| Self::local_area(layout));
        let outcome = self
            .group
            .handle_event(area, &mut self.state.borrow_mut(), event);
        if outcome != PanelGroupOutcome::Ignored {
            return EventOutcome::Handled;
        }
        for (child, component) in layout.children.iter().zip(&self.children).rev() {
            let clip = Rect::new(
                area.x
                    .saturating_add(u16::try_from(child.x).unwrap_or(u16::MAX)),
                area.y
                    .saturating_add(u16::try_from(child.y).unwrap_or(u16::MAX)),
                child.node.size.width.try_into().unwrap_or(u16::MAX),
                u16::try_from(child.node.size.height).unwrap_or(u16::MAX),
            );
            let outcome = cx.with_transform(
                child.x,
                child.y,
                i32::from(clip.x),
                i64::from(clip.y),
                clip,
                |cx| component.event(event, &child.node, cx),
            );
            if outcome != EventOutcome::Ignored {
                return outcome;
            }
        }
        EventOutcome::Ignored
    }
}

impl PanelGroup {
    /// Create a panel group with layout-only policy.
    #[must_use]
    pub const fn new(axis: PanelGroupAxis) -> Self {
        Self {
            axis,
            policy: PanelGroupPolicy::bare(),
            styles: PanelGroupStyles {
                divider: Style::new(),
                hovered_divider: Style::new(),
                active_divider: Style::new(),
            },
            divider_mode: PanelDividerMode::Gap,
        }
    }

    /// Set how dividers occupy space; see [`PanelDividerMode`].
    #[must_use]
    pub const fn divider_mode(mut self, mode: PanelDividerMode) -> Self {
        self.divider_mode = mode;
        self
    }

    /// Cells reserved between `count` panels for divider lines.
    fn reserved_divider_cells(&self, count: usize) -> u16 {
        match self.divider_mode {
            PanelDividerMode::Gap => u16_saturating(count.saturating_sub(1)),
            PanelDividerMode::Overlay { .. } => 0,
        }
    }

    /// Handle rectangle for a divider whose earlier panel ends at `boundary` (exclusive),
    /// clipped to `[start, end)` along the primary axis.
    fn divider_rect(&self, boundary: u16, start: u16, end: u16, cross: (u16, u16)) -> Rect {
        let (offset, length) = match self.divider_mode {
            PanelDividerMode::Gap => (boundary, 1),
            PanelDividerMode::Overlay { before, after } => {
                let first = boundary.saturating_sub(before).max(start);
                let last = boundary
                    .saturating_add(after)
                    .min(end)
                    .max(first.saturating_add(1));
                (first, last - first)
            }
        };
        let length = length
            .min(end.saturating_sub(offset))
            .max(u16::from(end > offset));
        match self.axis {
            PanelGroupAxis::Horizontal => Rect::new(offset, cross.0, length, cross.1),
            PanelGroupAxis::Vertical => Rect::new(cross.0, offset, cross.1, length),
        }
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: PanelGroupPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: PanelGroupStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Return computed layout.
    #[must_use]
    pub fn layout(&self, area: Rect, state: &PanelGroupState) -> PanelGroupLayout {
        let count = state.sizes.len();
        if count == 0 || area.is_empty() {
            return PanelGroupLayout {
                panels: Vec::new(),
                dividers: Vec::new(),
            };
        }
        let available =
            primary_len(self.axis, area).saturating_sub(self.reserved_divider_cells(count));
        let lengths = allocated_lengths(available, state);
        self.rects_from_lengths(area, &lengths)
    }

    /// Register a parent selection scope and deterministic child panel scopes.
    ///
    /// Divider cells are excluded from child initiation areas, so resize handles
    /// retain pointer precedence while a parent scope can still order content
    /// across sibling panels.
    pub fn register_selection(
        &self,
        area: Rect,
        state: &PanelGroupState,
        selection: &ComponentSelectionState,
        child_policy: &ComponentSelectionPolicy,
        paint: &mut PaintCx<'_, '_>,
    ) -> Vec<ComponentSelectionOutcome> {
        let layout = self.layout(area, state);
        let parent_policy = ComponentSelectionPolicy {
            pointer_intent: child_policy.pointer_intent,
            enabled: child_policy.enabled,
            content_capture: bmux_tui::selection::SelectionCapture::Capture,
            chrome_capture: bmux_tui::selection::SelectionCapture::Capture,
            auto_scroll: child_policy.auto_scroll,
        };
        let mut outcomes = vec![paint_component_scope(
            paint,
            selection,
            &parent_policy,
            area,
            area,
        )];
        for (index, panel) in layout.panels.iter().copied().enumerate() {
            let child = ComponentSelectionState::new(format!(
                "{}.panel.{index}",
                selection.scope_id.as_str()
            ))
            .parent(selection.scope_id.clone())
            .order(u64::try_from(index).unwrap_or(u64::MAX))
            .revision(selection.revision);
            outcomes.push(paint_component_scope(
                paint,
                &child,
                child_policy,
                panel,
                panel,
            ));
        }
        outcomes
    }

    /// Handle one event.
    pub fn handle_event(
        &self,
        area: Rect,
        state: &mut PanelGroupState,
        event: &Event,
    ) -> PanelGroupOutcome {
        match event {
            Event::Mouse(mouse) if self.policy.mouse.enabled => {
                self.handle_mouse(area, state, *mouse)
            }
            Event::Key(_) if self.policy.keyboard.enabled => PanelGroupOutcome::Ignored,
            Event::Mouse(_)
            | Event::Key(_)
            | Event::Resize(_)
            | Event::Paste(_)
            | Event::Focus(_)
            | Event::Tick
            | Event::User(_) => PanelGroupOutcome::Ignored,
        }
    }

    fn handle_mouse(
        &self,
        area: Rect,
        state: &mut PanelGroupState,
        mouse: MouseEvent,
    ) -> PanelGroupOutcome {
        let layout = self.layout(area, state);
        match mouse.kind {
            MouseEventKind::Move if self.policy.mouse.hover_dividers => {
                let hovered = divider_at(&layout, state, mouse.position);
                if state.hovered_divider == hovered {
                    PanelGroupOutcome::Ignored
                } else {
                    state.hovered_divider = hovered;
                    PanelGroupOutcome::Redraw
                }
            }
            MouseEventKind::Down(MouseButton::Left)
                if self.policy.mouse.drag_dividers && self.policy.resize.enabled =>
            {
                if let Some(divider) = divider_at(&layout, state, mouse.position) {
                    state.hovered_divider = Some(divider);
                    state.active_drag = Some(PanelDividerDrag {
                        divider,
                        drag: DragState::new(mouse.position),
                    });
                    PanelGroupOutcome::DividerDragStarted { divider }
                } else {
                    self.focus_panel_at(state, &layout, mouse.position)
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.focus_panel_at(state, &layout, mouse.position)
            }
            MouseEventKind::Drag(MouseButton::Left)
                if self.policy.mouse.drag_dividers && self.policy.resize.enabled =>
            {
                self.drag_divider(area, state, mouse.position)
            }
            MouseEventKind::Up(MouseButton::Left) => {
                state
                    .active_drag
                    .take()
                    .map_or(PanelGroupOutcome::Ignored, |drag| {
                        PanelGroupOutcome::DividerDragEnded {
                            divider: drag.divider,
                        }
                    })
            }
            MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight
            | MouseEventKind::Move => PanelGroupOutcome::Ignored,
        }
    }

    fn focus_panel_at(
        &self,
        state: &mut PanelGroupState,
        layout: &PanelGroupLayout,
        position: Point,
    ) -> PanelGroupOutcome {
        if !self.policy.mouse.click_to_focus || !self.policy.focus.enabled {
            return PanelGroupOutcome::Ignored;
        }
        let Some(panel) = layout
            .panels
            .iter()
            .position(|panel_area| panel_area.contains(position))
        else {
            return PanelGroupOutcome::Ignored;
        };
        if state.focused_panel == Some(panel) {
            PanelGroupOutcome::Handled
        } else {
            state.focused_panel = Some(panel);
            PanelGroupOutcome::Focused { panel }
        }
    }

    fn drag_divider(
        &self,
        area: Rect,
        state: &mut PanelGroupState,
        position: Point,
    ) -> PanelGroupOutcome {
        let Some(active) = state.active_drag else {
            return PanelGroupOutcome::Ignored;
        };
        if !self.policy.resize.live_resize {
            state.active_drag = Some(PanelDividerDrag {
                divider: active.divider,
                drag: active.drag.moved_to(position),
            });
            return PanelGroupOutcome::Redraw;
        }
        let delta = match self.axis {
            PanelGroupAxis::Horizontal => i32::from(position.x) - i32::from(active.drag.current.x),
            PanelGroupAxis::Vertical => i32::from(position.y) - i32::from(active.drag.current.y),
        };
        state.active_drag = Some(PanelDividerDrag {
            divider: active.divider,
            drag: active.drag.moved_to(position),
        });
        self.apply_divider_delta(area, state, active.divider, delta)
    }

    /// Move a divider by `delta` cells (positive grows the earlier panel), as a drag would,
    /// within both panels' constraints. Ignored for dividers next to a collapsed panel.
    pub fn resize_divider(
        &self,
        area: Rect,
        state: &mut PanelGroupState,
        divider: usize,
        delta: i32,
    ) -> PanelGroupOutcome {
        self.apply_divider_delta(area, state, divider, delta)
    }

    fn apply_divider_delta(
        &self,
        area: Rect,
        state: &mut PanelGroupState,
        divider: usize,
        delta: i32,
    ) -> PanelGroupOutcome {
        if delta == 0 || !state.divider_resizable(divider) {
            return PanelGroupOutcome::Ignored;
        }
        let before_index = divider;
        let after_index = divider + 1;
        let current_lengths = allocated_lengths(
            primary_len(self.axis, area)
                .saturating_sub(self.reserved_divider_cells(state.sizes.len())),
            state,
        );
        let before = current_lengths
            .get(before_index)
            .copied()
            .unwrap_or_else(|| resolved_size(state.sizes[before_index]));
        let after = current_lengths
            .get(after_index)
            .copied()
            .unwrap_or_else(|| resolved_size(state.sizes[after_index]));
        let total = before.saturating_add(after);
        let before_target = add_signed(before, delta);
        let before_min = state.constraint(before_index).min;
        let after_min = state.constraint(after_index).min;
        let before_max = state.constraint(before_index).max.unwrap_or(u16::MAX);
        let after_max = state.constraint(after_index).max.unwrap_or(u16::MAX);
        let min_before_from_after = total.saturating_sub(after_max);
        let max_before_from_after = total.saturating_sub(after_min);
        let min_before = before_min.max(min_before_from_after);
        let max_before = before_max.min(max_before_from_after).min(total);
        let new_before = before_target.clamp(min_before, max_before);
        let new_after = total.saturating_sub(new_before);
        if new_before == before && new_after == after {
            return PanelGroupOutcome::Ignored;
        }
        if self.policy.resize.preserve_flex {
            // Rebase every expanded flex weight to its current length so the panels that
            // did not move keep their share, then size the two neighbours the same way.
            for (index, length) in current_lengths.iter().copied().enumerate() {
                if matches!(state.sizes[index], PanelSize::Flex(_)) && !state.is_collapsed(index) {
                    state.sizes[index] = PanelSize::Flex(length.max(1));
                }
            }
        }
        let keep = |size: PanelSize, cells: u16| match size {
            PanelSize::Flex(_) if self.policy.resize.preserve_flex => PanelSize::Flex(cells.max(1)),
            _ => PanelSize::Fixed(cells),
        };
        state.sizes[before_index] = keep(state.sizes[before_index], new_before);
        state.sizes[after_index] = keep(state.sizes[after_index], new_after);
        PanelGroupOutcome::Resized {
            divider,
            before: new_before,
            after: new_after,
        }
    }

    fn rects_from_lengths(&self, area: Rect, lengths: &[u16]) -> PanelGroupLayout {
        let mut panels = Vec::with_capacity(lengths.len());
        let mut dividers = Vec::with_capacity(lengths.len().saturating_sub(1));
        let (start, end, cross) = match self.axis {
            PanelGroupAxis::Horizontal => (area.x, area.right(), (area.y, area.height)),
            PanelGroupAxis::Vertical => (area.y, area.bottom(), (area.x, area.width)),
        };
        let gap = u16::from(matches!(self.divider_mode, PanelDividerMode::Gap));
        let mut cursor = start;
        for (index, length) in lengths.iter().copied().enumerate() {
            let panel = match self.axis {
                PanelGroupAxis::Horizontal => Rect::new(cursor, area.y, length, area.height),
                PanelGroupAxis::Vertical => Rect::new(area.x, cursor, area.width, length),
            };
            panels.push(panel);
            cursor = cursor.saturating_add(length);
            if index + 1 < lengths.len() {
                dividers.push(self.divider_rect(cursor, start, end, cross));
                cursor = cursor.saturating_add(gap);
            }
        }
        PanelGroupLayout { panels, dividers }
    }
}

const fn primary_len(axis: PanelGroupAxis, area: Rect) -> u16 {
    match axis {
        PanelGroupAxis::Horizontal => area.width,
        PanelGroupAxis::Vertical => area.height,
    }
}

fn allocated_lengths(available: u16, state: &PanelGroupState) -> Vec<u16> {
    allocated_lengths_for_count(available, state, state.sizes.len())
}

fn allocated_lengths_for_count(available: u16, state: &PanelGroupState, count: usize) -> Vec<u16> {
    let count = count.min(state.sizes.len());
    let mut lengths = vec![0; count];
    let mut remaining = available;
    let mut total_weight: u16 = 0;
    for (index, size) in state.sizes.iter().copied().take(count).enumerate() {
        match size {
            PanelSize::Fixed(cells) => {
                // A collapsed panel keeps its collapsed size regardless of its constraints.
                let wanted = if state.is_collapsed(index) {
                    cells
                } else {
                    state.constraint(index).clamp(cells)
                };
                let clamped = wanted.min(remaining);
                lengths[index] = clamped;
                remaining = remaining.saturating_sub(clamped);
            }
            PanelSize::Flex(weight) => total_weight = total_weight.saturating_add(weight.max(1)),
        }
    }
    if total_weight == 0 {
        return lengths;
    }
    let mut flex_remaining = remaining;
    let mut seen_weight: u16 = 0;
    let flex_indices = state
        .sizes
        .iter()
        .take(count)
        .enumerate()
        .filter_map(|(index, size)| matches!(size, PanelSize::Flex(_)).then_some(index))
        .collect::<Vec<_>>();
    for (slot, index) in flex_indices.iter().copied().enumerate() {
        let weight = match state.sizes[index] {
            PanelSize::Flex(weight) => weight.max(1),
            PanelSize::Fixed(_) => 0,
        };
        seen_weight = seen_weight.saturating_add(weight);
        let raw = if slot + 1 == flex_indices.len() {
            flex_remaining
        } else {
            let allocated_through_here =
                u32::from(remaining) * u32::from(seen_weight) / u32::from(total_weight);
            let allocated_before = u32::from(remaining.saturating_sub(flex_remaining));
            u16_saturating(allocated_through_here.saturating_sub(allocated_before) as usize)
        };
        let clamped = state.constraint(index).clamp(raw).min(flex_remaining);
        lengths[index] = clamped;
        flex_remaining = flex_remaining.saturating_sub(clamped);
    }
    lengths
}

fn divider_at(
    layout: &PanelGroupLayout,
    state: &PanelGroupState,
    position: Point,
) -> Option<usize> {
    layout
        .dividers
        .iter()
        .enumerate()
        .find(|(index, divider)| divider.contains(position) && state.divider_resizable(*index))
        .map(|(index, _)| index)
}

const fn resolved_size(size: PanelSize) -> u16 {
    match size {
        PanelSize::Fixed(cells) | PanelSize::Flex(cells) => cells,
    }
}

fn add_signed(value: u16, delta: i32) -> u16 {
    let magnitude =
        u16::try_from(delta.unsigned_abs().min(u32::from(u16::MAX))).unwrap_or(u16::MAX);
    if delta.is_negative() {
        value.saturating_sub(magnitude)
    } else {
        value.saturating_add(magnitude)
    }
}

impl crate::theme::ComponentTheme {
    /// Convert this semantic component theme into [`PanelGroupStyles`].
    #[must_use]
    pub fn panel_group_styles(self) -> PanelGroupStyles {
        PanelGroupStyles::from(self)
    }
}

impl From<crate::theme::ComponentTheme> for PanelGroupStyles {
    fn from(theme: crate::theme::ComponentTheme) -> Self {
        let theme = theme.for_surface(crate::theme::ComponentSurfaceDepth::Normal);
        Self {
            divider: theme.border,
            hovered_divider: theme.hover,
            active_divider: theme.border_focused,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use bmux_tui::buffer::Buffer;
    use bmux_tui::component::{Component, Constraints, EventCx, LayoutCx};
    use bmux_tui::composition::TextBlock;
    use bmux_tui::event::{Event, EventOutcome, MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};
    use bmux_tui::paint::PaintCx;
    use bmux_tui::selection::{
        SelectionCapture, SelectionController, SelectionFragment, SelectionGesturePhase,
        SelectionOutcome,
    };

    use super::{
        PanelDividerMode, PanelGroup, PanelGroupAxis, PanelGroupComponent, PanelGroupConstraints,
        PanelGroupOutcome, PanelGroupPolicy, PanelGroupState, PanelGroupStyles, PanelSize,
    };
    use crate::selection::{ComponentSelectionPolicy, ComponentSelectionState};
    use bmux_tui::style::{Color, Style};

    #[test]
    fn pointer_override_invalidates_paint_without_remeasuring_layout() {
        use bmux_tui::pointer::{PointerIntent, PointerShape};

        let state = RefCell::new(PanelGroupState::new([
            PanelSize::fixed(4),
            PanelSize::flex(1),
        ]));
        let component = PanelGroupComponent::new(
            "workspace",
            PanelGroup::new(PanelGroupAxis::Horizontal).policy(PanelGroupPolicy::interactive()),
            &state,
        )
        .child(TextBlock::new("left").id("left"))
        .child(TextBlock::new("right").id("right"));
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
    fn horizontal_layout_allocates_fixed_and_flex_panels() {
        let group = PanelGroup::new(PanelGroupAxis::Horizontal);
        let state = PanelGroupState::new([PanelSize::fixed(10), PanelSize::flex(1)]);

        let layout = group.layout(Rect::new(0, 0, 30, 5), &state);

        assert_eq!(
            layout.panels,
            vec![Rect::new(0, 0, 10, 5), Rect::new(11, 0, 19, 5)]
        );
        assert_eq!(layout.dividers, vec![Rect::new(10, 0, 1, 5)]);
    }

    #[test]
    fn vertical_layout_allocates_fixed_and_flex_panels() {
        let group = PanelGroup::new(PanelGroupAxis::Vertical);
        let state = PanelGroupState::new([PanelSize::fixed(3), PanelSize::flex(1)]);

        let layout = group.layout(Rect::new(2, 1, 10, 10), &state);

        assert_eq!(
            layout.panels,
            vec![Rect::new(2, 1, 10, 3), Rect::new(2, 5, 10, 6)]
        );
        assert_eq!(layout.dividers, vec![Rect::new(2, 4, 10, 1)]);
    }

    #[test]
    fn bare_policy_ignores_mouse_events() {
        let group = PanelGroup::new(PanelGroupAxis::Horizontal).policy(PanelGroupPolicy::bare());
        let mut state = PanelGroupState::new([PanelSize::fixed(10), PanelSize::fixed(10)]);

        let outcome = group.handle_event(
            Rect::new(0, 0, 21, 5),
            &mut state,
            &mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 0),
        );

        assert_eq!(outcome, PanelGroupOutcome::Ignored);
        assert_eq!(state.active_divider(), None);
    }

    #[test]
    fn click_to_focus_focuses_panel_when_enabled() {
        let group =
            PanelGroup::new(PanelGroupAxis::Horizontal).policy(PanelGroupPolicy::interactive());
        let mut state = PanelGroupState::new([PanelSize::fixed(10), PanelSize::fixed(10)]);

        let outcome = group.handle_event(
            Rect::new(0, 0, 21, 5),
            &mut state,
            &mouse_event(MouseEventKind::Down(MouseButton::Left), 15, 0),
        );

        assert_eq!(outcome, PanelGroupOutcome::Focused { panel: 1 });
        assert_eq!(state.focused_panel(), Some(1));
    }

    #[test]
    fn divider_drag_resizes_adjacent_panels() {
        let group =
            PanelGroup::new(PanelGroupAxis::Horizontal).policy(PanelGroupPolicy::resize_only());
        let mut state = PanelGroupState::new([PanelSize::fixed(10), PanelSize::fixed(10)]);
        let area = Rect::new(0, 0, 21, 5);

        assert_eq!(
            group.handle_event(
                area,
                &mut state,
                &mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 0),
            ),
            PanelGroupOutcome::DividerDragStarted { divider: 0 }
        );
        assert_eq!(
            group.handle_event(
                area,
                &mut state,
                &mouse_event(MouseEventKind::Drag(MouseButton::Left), 13, 0),
            ),
            PanelGroupOutcome::Resized {
                divider: 0,
                before: 13,
                after: 7,
            }
        );
        assert_eq!(state.sizes(), &[PanelSize::fixed(13), PanelSize::fixed(7)]);
    }

    #[test]
    fn divider_drag_clamps_to_constraints() {
        let group =
            PanelGroup::new(PanelGroupAxis::Horizontal).policy(PanelGroupPolicy::resize_only());
        let mut state = PanelGroupState::new([PanelSize::fixed(10), PanelSize::fixed(10)]);
        state.set_constraints([
            PanelGroupConstraints::new(5, Some(12)),
            PanelGroupConstraints::new(6, None),
        ]);
        let area = Rect::new(0, 0, 21, 5);

        let _ = group.handle_event(
            area,
            &mut state,
            &mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 0),
        );
        let outcome = group.handle_event(
            area,
            &mut state,
            &mouse_event(MouseEventKind::Drag(MouseButton::Left), 20, 0),
        );

        assert_eq!(
            outcome,
            PanelGroupOutcome::Resized {
                divider: 0,
                before: 12,
                after: 8,
            }
        );
        assert_eq!(state.sizes(), &[PanelSize::fixed(12), PanelSize::fixed(8)]);
    }

    #[test]
    fn nested_panel_scopes_exclude_dividers_and_keep_resize_event_precedence() {
        let group =
            PanelGroup::new(PanelGroupAxis::Horizontal).policy(PanelGroupPolicy::resize_only());
        let mut state = PanelGroupState::new([PanelSize::fixed(4), PanelSize::fixed(4)]);
        let area = Rect::new(0, 0, 9, 3);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);

        group.register_selection(
            area,
            &state,
            &ComponentSelectionState::new("group"),
            &ComponentSelectionPolicy::content(),
            &mut PaintCx::new(&mut frame),
        );
        let scopes = frame.selection().scopes();
        assert_eq!(scopes.len(), 3);
        assert_eq!(scopes[0].capture, SelectionCapture::Capture);
        assert_eq!(
            scopes[1]
                .parent
                .as_ref()
                .map(bmux_tui::selection::SelectionScopeId::as_str),
            Some("group")
        );
        assert_eq!(scopes[1].initiation_area, Rect::new(0, 0, 4, 3));
        assert_eq!(scopes[2].initiation_area, Rect::new(5, 0, 4, 3));
        assert!(
            scopes[1..]
                .iter()
                .all(|scope| !scope.initiation_area.contains(Point::new(4, 1)))
        );
        assert!(matches!(
            group.handle_event(
                area,
                &mut state,
                &mouse_event(MouseEventKind::Down(MouseButton::Left), 4, 1),
            ),
            PanelGroupOutcome::DividerDragStarted { divider: 0 }
        ));
    }

    #[test]
    fn nested_sibling_panels_lock_local_or_delegate_to_parent_by_initiation_scope() {
        let group = PanelGroup::new(PanelGroupAxis::Horizontal);
        let state = PanelGroupState::new([PanelSize::fixed(4), PanelSize::fixed(4)]);
        let area = Rect::new(0, 0, 9, 2);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);

        group.register_selection(
            area,
            &state,
            &ComponentSelectionState::new("workspace"),
            &ComponentSelectionPolicy::content(),
            &mut PaintCx::new(&mut frame),
        );
        for (index, panel) in group.layout(area, &state).panels.into_iter().enumerate() {
            let scope = format!("workspace.panel.{index}");
            PaintCx::new(&mut frame).push_selection_fragment(SelectionFragment::new(
                scope,
                format!("panel-{index}"),
                panel,
                u64::try_from(index).expect("panel index"),
                0..4,
            ));
        }
        let scene = frame.selection().clone();

        let mut local = SelectionController::new();
        assert_eq!(
            local.handle_mouse(
                &scene,
                MouseEvent::new(MouseEventKind::Down(MouseButton::Left), Point::new(1, 0))
            ),
            SelectionOutcome::Armed
        );
        assert_eq!(
            local
                .scope_id()
                .map(bmux_tui::selection::SelectionScopeId::as_str),
            Some("workspace.panel.0")
        );
        assert!(matches!(
            local.handle_mouse(
                &scene,
                MouseEvent::new(MouseEventKind::Drag(MouseButton::Left), Point::new(7, 0))
            ),
            SelectionOutcome::Changed { .. }
        ));
        assert_eq!(
            local
                .scope_id()
                .map(bmux_tui::selection::SelectionScopeId::as_str),
            Some("workspace.panel.0")
        );
        assert_eq!(local.phase(), SelectionGesturePhase::Dragging);

        let mut parent = SelectionController::new();
        assert_eq!(
            parent.handle_mouse(
                &scene,
                MouseEvent::new(MouseEventKind::Down(MouseButton::Left), Point::new(4, 0))
            ),
            SelectionOutcome::Armed
        );
        assert_eq!(
            parent
                .scope_id()
                .map(bmux_tui::selection::SelectionScopeId::as_str),
            Some("workspace")
        );
        assert!(matches!(
            parent.handle_mouse(
                &scene,
                MouseEvent::new(MouseEventKind::Drag(MouseButton::Left), Point::new(7, 0))
            ),
            SelectionOutcome::Changed { .. }
        ));
        let snapshot = parent.snapshot(&scene).expect("cross-panel selection");
        assert_eq!(snapshot.slices.len(), 2);
        assert_eq!(snapshot.slices[0].content_id.as_str(), "panel-0");
        assert_eq!(snapshot.slices[1].content_id.as_str(), "panel-1");
    }

    #[test]
    fn component_layout_paint_and_events_share_resolved_panel_geometry() {
        let state = RefCell::new(PanelGroupState::new([
            PanelSize::fixed(4),
            PanelSize::flex(1),
        ]));
        let component = PanelGroupComponent::new(
            "workspace",
            PanelGroup::new(PanelGroupAxis::Horizontal).policy(PanelGroupPolicy::interactive()),
            &state,
        )
        .child(TextBlock::new("left").id("left"))
        .child(TextBlock::new("right").id("right"));
        let layout = component.layout(Constraints::new(10, 10, 2, Some(2)), &mut LayoutCx::new());

        assert_eq!(layout.children.len(), 2);
        assert_eq!(layout.children[0].x, 0);
        assert_eq!(layout.children[0].node.size.width, 4);
        assert_eq!(layout.children[1].x, 5);
        assert_eq!(layout.children[1].node.size.width, 5);

        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 2));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(
            frame
                .buffer()
                .get(Point::new(4, 0))
                .map(|cell| cell.symbol.as_str()),
            Some("│")
        );
        assert!(frame.hits().regions().iter().any(|region| {
            region.id.as_str() == "workspace.divider.0" && region.area == Rect::new(4, 0, 1, 2)
        }));

        let mut event_cx = EventCx::new(&layout);
        assert_eq!(
            component.event(
                &mouse_event(MouseEventKind::Move, 4, 0),
                &layout,
                &mut event_cx,
            ),
            EventOutcome::Handled
        );
        assert_eq!(state.borrow().hovered_divider(), Some(0));
    }

    #[test]
    fn component_vertical_layout_respects_exact_requested_panel_sizes() {
        let state = RefCell::new(PanelGroupState::new([
            PanelSize::fixed(2),
            PanelSize::flex(1),
        ]));
        let component = PanelGroupComponent::new(
            "vertical",
            PanelGroup::new(PanelGroupAxis::Vertical),
            &state,
        )
        .child(TextBlock::new("top"))
        .child(TextBlock::new("bottom"));
        let layout = component.layout(Constraints::new(6, 6, 6, Some(6)), &mut LayoutCx::new());

        assert_eq!(layout.children[0].y, 0);
        assert_eq!(layout.children[0].node.size.height, 2);
        assert_eq!(layout.children[1].y, 3);
        assert_eq!(layout.children[1].node.size.height, 3);
    }

    const BORDERS: PanelDividerMode = PanelDividerMode::Overlay {
        before: 1,
        after: 1,
    };

    #[test]
    fn overlay_dividers_take_no_space_and_straddle_panel_edges() {
        let group = PanelGroup::new(PanelGroupAxis::Horizontal).divider_mode(BORDERS);
        let state = PanelGroupState::new([PanelSize::fixed(8), PanelSize::flex(1)]);
        let layout = group.layout(Rect::new(0, 0, 20, 4), &state);
        assert_eq!(
            layout.panels,
            vec![Rect::new(0, 0, 8, 4), Rect::new(8, 0, 12, 4)]
        );
        // The earlier panel's last column and the later panel's first column.
        assert_eq!(layout.dividers, vec![Rect::new(7, 0, 2, 4)]);

        let vertical =
            PanelGroup::new(PanelGroupAxis::Vertical).divider_mode(PanelDividerMode::Overlay {
                before: 0,
                after: 0,
            });
        let layout = vertical.layout(Rect::new(0, 0, 5, 10), &state);
        assert_eq!(layout.panels[1], Rect::new(0, 8, 5, 2));
        // A zero-width handle still gets one cell, at the start of the later panel.
        assert_eq!(layout.dividers, vec![Rect::new(0, 8, 5, 1)]);
    }

    #[test]
    fn overlay_handles_drag_and_highlight_in_place() {
        let state = RefCell::new(PanelGroupState::new([
            PanelSize::fixed(4),
            PanelSize::flex(1),
        ]));
        let hovered = Style::new().fg(Color::Cyan);
        let component = PanelGroupComponent::new(
            "workspace",
            PanelGroup::new(PanelGroupAxis::Horizontal)
                .policy(PanelGroupPolicy::resize_only())
                .divider_mode(BORDERS)
                .styles(PanelGroupStyles {
                    divider: Style::new().fg(Color::Red),
                    hovered_divider: hovered,
                    active_divider: hovered,
                }),
            &state,
        )
        .child(TextBlock::new("abcd").id("left"))
        .child(TextBlock::new("efghij").id("right"));
        let layout = component.layout(Constraints::new(10, 10, 1, Some(1)), &mut LayoutCx::new());
        assert_eq!(layout.children[1].x, 4);

        let paint = |component: &PanelGroupComponent<'_>| {
            let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 1));
            let mut frame = Frame::new(&mut buffer);
            component.paint(&layout, &mut PaintCx::new(&mut frame));
            let hits = frame.hits().clone();
            drop(frame);
            (buffer, hits)
        };
        let (buffer, hits) = paint(&component);
        assert_eq!(buffer.row_symbols(0).as_deref(), Some("abcdefghij"));
        // Idle overlay handles paint nothing, not even the divider style.
        assert_eq!(buffer.get(Point::new(3, 0)).unwrap().style.fg, None);
        assert!(hits.regions().iter().any(|region| {
            region.id.as_str() == "workspace.divider.0" && region.area == Rect::new(3, 0, 2, 1)
        }));

        let mut event_cx = EventCx::new(&layout);
        component.event(
            &mouse_event(MouseEventKind::Move, 4, 0),
            &layout,
            &mut event_cx,
        );
        let (buffer, _) = paint(&component);
        assert_eq!(buffer.row_symbols(0).as_deref(), Some("abcdefghij"));
        assert_eq!(
            buffer.get(Point::new(3, 0)).unwrap().style.fg,
            Some(Color::Cyan)
        );
        assert_eq!(
            buffer.get(Point::new(4, 0)).unwrap().style.fg,
            Some(Color::Cyan)
        );
        assert_eq!(buffer.get(Point::new(5, 0)).unwrap().style.fg, None);

        // Dragging the earlier panel's border cell resizes like a gap divider.
        let group = PanelGroup::new(PanelGroupAxis::Horizontal)
            .policy(PanelGroupPolicy::resize_only())
            .divider_mode(BORDERS);
        let mut state = PanelGroupState::new([PanelSize::fixed(4), PanelSize::flex(1)]);
        let area = Rect::new(0, 0, 10, 1);
        assert_eq!(
            group.handle_event(
                area,
                &mut state,
                &mouse_event(MouseEventKind::Down(MouseButton::Left), 3, 0)
            ),
            PanelGroupOutcome::DividerDragStarted { divider: 0 }
        );
        assert_eq!(
            group.handle_event(
                area,
                &mut state,
                &mouse_event(MouseEventKind::Drag(MouseButton::Left), 5, 0)
            ),
            PanelGroupOutcome::Resized {
                divider: 0,
                before: 6,
                after: 4
            }
        );
    }

    #[test]
    fn preserved_flex_keeps_resized_proportions_when_the_area_changes() {
        let group = PanelGroup::new(PanelGroupAxis::Horizontal).policy(PanelGroupPolicy {
            resize: super::PanelGroupResizePolicy::live().preserving_flex(true),
            ..PanelGroupPolicy::resize_only()
        });
        let mut state =
            PanelGroupState::new([PanelSize::flex(1), PanelSize::flex(1), PanelSize::fixed(3)]);
        let area = Rect::new(0, 0, 25, 1);
        assert_eq!(
            group.resize_divider(area, &mut state, 0, 4),
            PanelGroupOutcome::Resized {
                divider: 0,
                before: 14,
                after: 6
            }
        );
        assert_eq!(
            state.sizes(),
            [PanelSize::Flex(14), PanelSize::Flex(6), PanelSize::Fixed(3)]
        );
        // About twice the flexible space (42 cells): the same 7:3 ratio; fixed stays fixed.
        let wider = group.layout(Rect::new(0, 0, 47, 1), &state);
        assert_eq!(
            wider
                .panels
                .iter()
                .map(|panel| panel.width)
                .collect::<Vec<_>>(),
            [29, 13, 3]
        );

        // Without preservation the resized panels become fixed.
        let fixed =
            PanelGroup::new(PanelGroupAxis::Horizontal).policy(PanelGroupPolicy::resize_only());
        let mut state = PanelGroupState::new([PanelSize::flex(1), PanelSize::flex(1)]);
        fixed.resize_divider(Rect::new(0, 0, 21, 1), &mut state, 0, 2);
        assert_eq!(state.sizes(), [PanelSize::Fixed(12), PanelSize::Fixed(8)]);
    }

    #[test]
    fn programmatic_resize_respects_constraints() {
        let group =
            PanelGroup::new(PanelGroupAxis::Vertical).policy(PanelGroupPolicy::resize_only());
        let mut state = PanelGroupState::new([PanelSize::fixed(5), PanelSize::fixed(5)]);
        state.set_constraints([
            PanelGroupConstraints::new(2, Some(7)),
            PanelGroupConstraints::new(3, None),
        ]);
        let area = Rect::new(0, 0, 4, 11);
        assert_eq!(
            group.resize_divider(area, &mut state, 0, 10),
            PanelGroupOutcome::Resized {
                divider: 0,
                before: 7,
                after: 3
            }
        );
        assert_eq!(
            group.resize_divider(area, &mut state, 0, -10),
            PanelGroupOutcome::Resized {
                divider: 0,
                before: 2,
                after: 8
            }
        );
        assert_eq!(
            group.resize_divider(area, &mut state, 1, 1),
            PanelGroupOutcome::Ignored
        );
    }

    #[test]
    fn collapsed_panels_ignore_constraints_and_freeze_their_dividers() {
        let group = PanelGroup::new(PanelGroupAxis::Vertical)
            .policy(PanelGroupPolicy::resize_only())
            .divider_mode(BORDERS);
        let mut state =
            PanelGroupState::new([PanelSize::flex(1), PanelSize::flex(2), PanelSize::flex(1)]);
        state.set_constraints([PanelGroupConstraints::new(3, None); 3]);
        assert!(state.collapse(1, 1));
        assert!(!state.collapse(1, 1));
        assert!(state.is_collapsed(1));
        let area = Rect::new(0, 0, 10, 21);
        let layout = group.layout(area, &state);
        assert_eq!(
            layout
                .panels
                .iter()
                .map(|panel| panel.height)
                .collect::<Vec<_>>(),
            [10, 1, 10]
        );
        assert!(!state.divider_resizable(0) && !state.divider_resizable(1));
        assert_eq!(
            group.resize_divider(area, &mut state, 0, 2),
            PanelGroupOutcome::Ignored
        );
        // Pressing on the frozen handle does not start a drag.
        let on_handle = layout.dividers[0];
        assert_ne!(
            group.handle_event(
                area,
                &mut state,
                &mouse_event(
                    MouseEventKind::Down(MouseButton::Left),
                    on_handle.x,
                    on_handle.y
                )
            ),
            PanelGroupOutcome::DividerDragStarted { divider: 0 }
        );
        // Sizing a collapsed panel changes what it restores to.
        state.set_size(1, PanelSize::flex(4));
        assert_eq!(state.expanded_size(1), Some(PanelSize::flex(4)));
        assert!(state.expand(1));
        assert!(!state.expand(1));
        assert_eq!(state.sizes()[1], PanelSize::flex(4));
        assert!(state.divider_resizable(0));
    }

    fn mouse_event(kind: MouseEventKind, x: u16, y: u16) -> Event {
        Event::Mouse(MouseEvent::new(kind, Point::new(x, y)))
    }
}
