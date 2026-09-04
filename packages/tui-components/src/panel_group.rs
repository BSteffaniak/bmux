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
}

impl PanelGroupResizePolicy {
    /// Resize behavior disabled.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            live_resize: false,
        }
    }

    /// Live divider resize behavior enabled.
    #[must_use]
    pub const fn live() -> Self {
        Self {
            enabled: true,
            live_resize: true,
        }
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
}

/// Child-owning panel group on the canonical component lifecycle.
///
/// Panel placement, divider painting, hit registration, and event routing all
/// consume the same resolved [`LayoutNode`]. Interactive state remains
/// caller-owned.
pub struct PanelGroupComponent<'a> {
    id: LayoutId,
    group: PanelGroup,
    state: &'a RefCell<PanelGroupState>,
    children: Vec<Element<'a>>,
}

impl<'a> PanelGroupComponent<'a> {
    /// Create a component using `group` policy and caller-owned `state`.
    #[must_use]
    pub fn new(
        id: impl Into<LayoutId>,
        group: PanelGroup,
        state: &'a RefCell<PanelGroupState>,
    ) -> Self {
        Self {
            id: id.into(),
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
            layout.size.width,
            u16::try_from(layout.size.height).unwrap_or(u16::MAX),
        )
    }

    fn panel_rects(layout: &LayoutNode) -> Vec<Rect> {
        layout
            .children
            .iter()
            .map(|child| {
                Rect::new(
                    child.x,
                    u16::try_from(child.y).unwrap_or(u16::MAX),
                    child.node.size.width,
                    u16::try_from(child.node.size.height).unwrap_or(u16::MAX),
                )
            })
            .collect()
    }

    fn divider_rects(&self, layout: &LayoutNode) -> Vec<Rect> {
        layout
            .children
            .windows(2)
            .map(|pair| match self.group.axis {
                PanelGroupAxis::Horizontal => Rect::new(
                    pair[0].x.saturating_add(pair[0].node.size.width),
                    0,
                    1,
                    u16::try_from(layout.size.height).unwrap_or(u16::MAX),
                ),
                PanelGroupAxis::Vertical => Rect::new(
                    0,
                    u16::try_from(pair[0].y.saturating_add(pair[0].node.size.height))
                        .unwrap_or(u16::MAX),
                    layout.size.width,
                    1,
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
                        .focusable(false),
                );
            }
        }
        let divider_interactive = self.group.policy.mouse.enabled
            && (self.group.policy.mouse.hover_dividers
                || (self.group.policy.mouse.drag_dividers && self.group.policy.resize.enabled));
        for (index, divider) in resolved.dividers.iter().copied().enumerate() {
            if divider_interactive {
                cx.push_hit(
                    SceneRegion::new(format!("{}.divider.{index}", self.id.as_str()), divider)
                        .role(HitRole::ResizeHandle)
                        .hoverable(self.group.policy.mouse.hover_dividers)
                        .focusable(false),
                );
            }
            let style = if state.active_divider() == Some(index) {
                self.group.styles.active_divider
            } else if state.hovered_divider == Some(index) {
                self.group.styles.hovered_divider
            } else {
                self.group.styles.divider
            };
            let symbol = match self.group.axis {
                PanelGroupAxis::Horizontal => "│",
                PanelGroupAxis::Vertical => "─",
            };
            cx.fill(
                LocalRect::new(
                    i32::from(divider.x),
                    i64::from(divider.y),
                    divider.width,
                    divider.height,
                ),
                symbol,
                style,
            );
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
        let divider_count = count.saturating_sub(1);
        let primary_max = match self.group.axis {
            PanelGroupAxis::Horizontal => constraints.max_width(),
            PanelGroupAxis::Vertical => u16::try_from(
                constraints
                    .max_height()
                    .unwrap_or_else(|| usize::from(u16::MAX)),
            )
            .unwrap_or(u16::MAX),
        };
        let available = primary_max.saturating_sub(u16_saturating(divider_count));
        let lengths = allocated_lengths_for_count(available, &state, count);
        let mut children = Vec::with_capacity(count);
        let mut cursor = 0usize;
        let mut cross = 0usize;
        for (index, child) in self.children.iter().take(count).enumerate() {
            let primary = lengths[index];
            let child_constraints = match self.group.axis {
                PanelGroupAxis::Horizontal => Constraints::new(
                    primary,
                    primary,
                    constraints.min_height(),
                    constraints.max_height(),
                ),
                PanelGroupAxis::Vertical => Constraints::new(
                    constraints.min_width(),
                    constraints.max_width(),
                    usize::from(primary),
                    Some(usize::from(primary)),
                ),
            };
            let node = child.layout(child_constraints, cx);
            match self.group.axis {
                PanelGroupAxis::Horizontal => {
                    cross = cross.max(node.size.height);
                    children.push(ChildLayout::new(
                        u16::try_from(cursor).unwrap_or(u16::MAX),
                        0,
                        node,
                    ));
                }
                PanelGroupAxis::Vertical => {
                    cross = cross.max(usize::from(node.size.width));
                    children.push(ChildLayout::new(0, cursor, node));
                }
            }
            cursor = cursor
                .saturating_add(usize::from(primary))
                .saturating_add(usize::from(index + 1 < count));
        }
        let proposed = match self.group.axis {
            PanelGroupAxis::Horizontal => {
                LogicalSize::new(u16::try_from(cursor).unwrap_or(u16::MAX), cross)
            }
            PanelGroupAxis::Vertical => {
                LogicalSize::new(u16::try_from(cross).unwrap_or(u16::MAX), cursor)
            }
        };
        LayoutNode::with_children(self.id.clone(), constraints.constrain(proposed), children)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        for (child, component) in layout.children.iter().zip(&self.children) {
            cx.with_child(
                i32::from(child.x),
                i64::try_from(child.y).unwrap_or(i64::MAX),
                LocalRect::new(
                    0,
                    0,
                    child.node.size.width,
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
                area.x.saturating_add(child.x),
                area.y
                    .saturating_add(u16::try_from(child.y).unwrap_or(u16::MAX)),
                child.node.size.width,
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
        let dividers = count.saturating_sub(1);
        let available = primary_len(self.axis, area).saturating_sub(u16_saturating(dividers));
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
                let hovered = divider_at(&layout, mouse.position);
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
                if let Some(divider) = divider_at(&layout, mouse.position) {
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
        if delta == 0 || active.divider + 1 >= state.sizes.len() {
            return PanelGroupOutcome::Ignored;
        }
        let before_index = active.divider;
        let after_index = active.divider + 1;
        let current_lengths = allocated_lengths(
            primary_len(self.axis, area)
                .saturating_sub(u16_saturating(state.sizes.len().saturating_sub(1))),
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
        state.sizes[before_index] = PanelSize::Fixed(new_before);
        state.sizes[after_index] = PanelSize::Fixed(new_after);
        PanelGroupOutcome::Resized {
            divider: active.divider,
            before: new_before,
            after: new_after,
        }
    }

    fn rects_from_lengths(&self, area: Rect, lengths: &[u16]) -> PanelGroupLayout {
        let mut panels = Vec::with_capacity(lengths.len());
        let mut dividers = Vec::with_capacity(lengths.len().saturating_sub(1));
        let mut cursor = match self.axis {
            PanelGroupAxis::Horizontal => area.x,
            PanelGroupAxis::Vertical => area.y,
        };
        for (index, length) in lengths.iter().copied().enumerate() {
            let panel = match self.axis {
                PanelGroupAxis::Horizontal => Rect::new(cursor, area.y, length, area.height),
                PanelGroupAxis::Vertical => Rect::new(area.x, cursor, area.width, length),
            };
            panels.push(panel);
            cursor = cursor.saturating_add(length);
            if index + 1 < lengths.len() {
                let divider = match self.axis {
                    PanelGroupAxis::Horizontal => {
                        Rect::new(cursor, area.y, u16::from(area.width > 0), area.height)
                    }
                    PanelGroupAxis::Vertical => {
                        Rect::new(area.x, cursor, area.width, u16::from(area.height > 0))
                    }
                };
                dividers.push(divider);
                cursor = cursor.saturating_add(1);
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
                let clamped = state.constraint(index).clamp(cells).min(remaining);
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

fn divider_at(layout: &PanelGroupLayout, position: Point) -> Option<usize> {
    layout
        .dividers
        .iter()
        .position(|divider| divider.contains(position))
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
            hovered_divider: theme.info,
            active_divider: theme.focused,
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
        PanelGroup, PanelGroupAxis, PanelGroupComponent, PanelGroupConstraints, PanelGroupOutcome,
        PanelGroupPolicy, PanelGroupState, PanelSize,
    };
    use crate::selection::{ComponentSelectionPolicy, ComponentSelectionState};

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

    fn mouse_event(kind: MouseEventKind, x: u16, y: u16) -> Event {
        Event::Mouse(MouseEvent::new(kind, Point::new(x, y)))
    }
}
