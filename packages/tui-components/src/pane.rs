//! Generic configurable pane/surface component.
//!
//! This pane is a neutral UI surface. It has no BMUX product semantics.

use std::cell::Cell;
use std::hash::{Hash, Hasher};

use bmux_tui::chrome::Border;
use bmux_tui::component::{
    ChildLayout, Component, ComponentRevision, Constraints, Element, EventCx, LayoutCx, LayoutId,
    LayoutNode,
};
use bmux_tui::composition::Surface;
use bmux_tui::event::{Event, EventOutcome, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::geometry::{Insets, Point, Rect, Size};
use bmux_tui::hit::{HitRegion as SceneRegion, HitRole};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Style};
use bmux_tui::style::Modifier;

use crate::common::{DragState, InteractionState};
use crate::scroll_view::{ScrollExtent, ScrollSource, ScrollViewOutcome, TrackScrollbar};
use crate::scrollbar::{ScrollbarPolicy, ScrollbarStyles};
use crate::selection::{
    ComponentSelectionOutcome, ComponentSelectionPolicy, ComponentSelectionState,
    paint_component_scope,
};

/// Visual styles for a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneStyles {
    /// Optional full-area background style.
    pub background: Option<Style>,
    /// Border style when the pane is not focused.
    pub border: Style,
    /// Border style when the pane is focused.
    pub focused_border: Style,
}

impl Default for PaneStyles {
    fn default() -> Self {
        Self {
            background: None,
            border: Style::new(),
            focused_border: Style::new().add_modifier(Modifier::REVERSED),
        }
    }
}

/// Configurable pane mouse behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct PaneMousePolicy {
    /// Whether the pane accepts mouse events.
    pub enabled: bool,
    /// Whether primary clicks inside the pane request focus.
    pub click_to_focus: bool,
    /// Whether the title bar starts pane dragging.
    pub title_bar_drag: bool,
    /// Whether wheel scroll events inside the pane are handled as scroll delegation.
    pub scroll_wheel: bool,
    /// Whether pane edges/corners start resizing.
    pub resize_handles: ResizeHandles,
}

impl PaneMousePolicy {
    /// Mouse handling disabled.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            click_to_focus: false,
            title_bar_drag: false,
            scroll_wheel: false,
            resize_handles: ResizeHandles::NONE,
        }
    }

    /// Common pane mouse behavior: click focus and title-bar dragging.
    #[must_use]
    pub const fn draggable() -> Self {
        Self {
            enabled: true,
            click_to_focus: true,
            title_bar_drag: true,
            scroll_wheel: false,
            resize_handles: ResizeHandles::NONE,
        }
    }
}

impl Default for PaneMousePolicy {
    fn default() -> Self {
        Self::disabled()
    }
}

/// Enabled resize handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct ResizeHandles {
    /// Top edge.
    pub top: bool,
    /// Right edge.
    pub right: bool,
    /// Bottom edge.
    pub bottom: bool,
    /// Left edge.
    pub left: bool,
    /// Top-left corner.
    pub top_left: bool,
    /// Top-right corner.
    pub top_right: bool,
    /// Bottom-left corner.
    pub bottom_left: bool,
    /// Bottom-right corner.
    pub bottom_right: bool,
}

impl ResizeHandles {
    /// No resize handles.
    pub const NONE: Self = Self {
        top: false,
        right: false,
        bottom: false,
        left: false,
        top_left: false,
        top_right: false,
        bottom_left: false,
        bottom_right: false,
    };

    /// All edges and corners.
    pub const ALL: Self = Self {
        top: true,
        right: true,
        bottom: true,
        left: true,
        top_left: true,
        top_right: true,
        bottom_left: true,
        bottom_right: true,
    };

    const fn is_empty(self) -> bool {
        !self.top
            && !self.right
            && !self.bottom
            && !self.left
            && !self.top_left
            && !self.top_right
            && !self.bottom_left
            && !self.bottom_right
    }
}

/// Pane movement and resize bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneBoundsPolicy {
    /// Optional parent bounds to clamp movement/resizing into.
    pub parent: Option<Rect>,
    /// Minimum pane size.
    pub min_size: Size,
    /// Optional maximum pane size.
    pub max_size: Option<Size>,
}

impl Default for PaneBoundsPolicy {
    fn default() -> Self {
        Self {
            parent: None,
            min_size: Size::new(3, 3),
            max_size: None,
        }
    }
}

/// Pane behavior policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PanePolicy {
    /// Mouse behavior.
    pub mouse: PaneMousePolicy,
    /// Movement and resize bounds.
    pub bounds: PaneBoundsPolicy,
}

/// Runtime pane state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneState {
    /// Current pane area.
    pub area: Rect,
    /// Common focus/hover/press state.
    pub interaction: InteractionState,
    drag: Option<PaneDragState>,
}

impl PaneState {
    /// Create pane state at `area`.
    #[must_use]
    pub const fn new(area: Rect) -> Self {
        Self {
            area,
            interaction: InteractionState::new(),
            drag: None,
        }
    }

    /// Return whether a drag or resize interaction is active.
    #[must_use]
    pub const fn is_dragging(self) -> bool {
        self.drag.is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PaneDragState {
    pointer: DragState,
    original_area: Rect,
    mode: PaneDragMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneDragMode {
    Move,
    Resize(ResizeHandle),
}

/// A resize handle hit by pointer input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeHandle {
    /// Top edge.
    Top,
    /// Right edge.
    Right,
    /// Bottom edge.
    Bottom,
    /// Left edge.
    Left,
    /// Top-left corner.
    TopLeft,
    /// Top-right corner.
    TopRight,
    /// Bottom-left corner.
    BottomLeft,
    /// Bottom-right corner.
    BottomRight,
}

/// Outcome from handling pane input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneOutcome {
    /// Event was not handled.
    Ignored,
    /// Event was handled without requiring redraw.
    Handled,
    /// Event was handled and requires redraw.
    Redraw,
    /// Pane focus was requested.
    FocusRequested,
    /// Pane moved to a new area.
    Moved { area: Rect },
    /// Pane resized to a new area.
    Resized { area: Rect },
    /// Scroll wheel was used inside the pane.
    ScrollDelegated { direction: ScrollDirection },
}

/// Direction for delegated pane scroll input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollDirection {
    /// Scroll content upward.
    Up,
    /// Scroll content downward.
    Down,
    /// Scroll content leftward.
    Left,
    /// Scroll content rightward.
    Right,
}

/// A labeled control drawn at the right end of a pane's title row, such as collapse,
/// maximize or close. Its hit region id is `<pane id>.action.<id>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneTitleAction {
    /// Stable action id.
    pub id: String,
    /// Label, including any padding and styling.
    pub label: Line,
}

impl PaneTitleAction {
    /// Create a title action.
    #[must_use]
    pub fn new(id: impl Into<String>, label: impl Into<Line>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
        }
    }
}

/// Title-row geometry shared by painting and hit registration, relative to the pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneTitleLayout {
    /// Where the title is written (clipped), if anything of it fits.
    pub title: Option<Rect>,
    /// Visible actions as `(index into the pane's actions, area)`, left to right.
    pub actions: Vec<(usize, Rect)>,
}

/// Configurable pane renderer and event handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane<'a> {
    title: Option<Line>,
    actions: Vec<PaneTitleAction>,
    title_hit: bool,
    padding: Insets,
    border: bool,
    policy: PanePolicy,
    styles: PaneStyles,
    _marker: std::marker::PhantomData<&'a ()>,
}

impl Pane<'_> {
    /// Create a bordered pane.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            title: None,
            actions: Vec::new(),
            title_hit: false,
            padding: Insets::all(0),
            border: true,
            policy: PanePolicy {
                mouse: PaneMousePolicy::disabled(),
                bounds: PaneBoundsPolicy {
                    parent: None,
                    min_size: Size::new(3, 3),
                    max_size: None,
                },
            },
            styles: PaneStyles {
                background: None,
                border: Style::new(),
                focused_border: Style::new().add_modifier(Modifier::REVERSED),
            },
            _marker: std::marker::PhantomData,
        }
    }

    /// Set pane title.
    #[must_use]
    pub fn title(mut self, title: impl Into<Line>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Append a title-row action. Actions are right-aligned in declaration order; when
    /// the row is too narrow the leftmost actions are dropped first, then the title is
    /// clipped.
    #[must_use]
    pub fn action(mut self, action: PaneTitleAction) -> Self {
        self.actions.push(action);
        self
    }

    /// Register the title row as a clickable region `<pane id>.title` (for focus or
    /// double-click handling by the caller). Panes with title-bar dragging already
    /// register that id as their drag handle.
    #[must_use]
    pub const fn title_hit(mut self, enabled: bool) -> Self {
        self.title_hit = enabled;
        self
    }

    /// Title-row geometry for a pane of `size` (relative to the pane).
    #[must_use]
    pub fn title_layout(&self, size: Rect) -> PaneTitleLayout {
        let width = self
            .title
            .as_ref()
            .map(|title| u16::try_from(title.width()).unwrap_or(u16::MAX));
        self.title_layout_for(size, width)
    }

    /// Title-row geometry for a title `title_width` cells wide (or none). Actions are placed
    /// first; the title gets what remains of the row.
    #[must_use]
    pub fn title_layout_for(&self, size: Rect, title_width: Option<u16>) -> PaneTitleLayout {
        if size.height == 0 || size.width < 3 {
            return PaneTitleLayout {
                title: None,
                actions: Vec::new(),
            };
        }
        // Inside the corners.
        let inner_start = size.x.saturating_add(1);
        let inner_end = size.right().saturating_sub(1);
        let available = inner_end.saturating_sub(inner_start);
        let widths: Vec<u16> = self
            .actions
            .iter()
            .map(|action| u16::try_from(action.label.width()).unwrap_or(u16::MAX))
            .collect();
        // Keep as many trailing actions as fit, with one cell between neighbours.
        let mut first = widths.len();
        let mut used: u16 = 0;
        while first > 0 {
            let width = widths[first - 1];
            let needed = used
                .saturating_add(width)
                .saturating_add(u16::from(used > 0));
            if needed > available {
                break;
            }
            used = needed;
            first -= 1;
        }
        let mut actions = Vec::with_capacity(widths.len() - first);
        let mut x = inner_end.saturating_sub(used);
        let actions_start = x;
        for (index, width) in widths.iter().copied().enumerate().skip(first) {
            actions.push((index, Rect::new(x, size.y, width, 1)));
            x = x.saturating_add(width).saturating_add(1);
        }
        // The title ends one cell before the first action.
        let title_end = if actions.is_empty() {
            inner_end
        } else {
            actions_start.saturating_sub(1)
        };
        let title = title_width.and_then(|width| {
            let width = width.min(title_end.saturating_sub(inner_start));
            (width > 0).then(|| Rect::new(inner_start, size.y, width, 1))
        });
        PaneTitleLayout { title, actions }
    }

    /// Set pane padding.
    #[must_use]
    pub const fn padding(mut self, padding: Insets) -> Self {
        self.padding = padding;
        self
    }

    /// Enable or disable border rendering.
    #[must_use]
    pub const fn border(mut self, border: bool) -> Self {
        self.border = border;
        self
    }

    /// Set behavior policy.
    #[must_use]
    pub const fn policy(mut self, policy: PanePolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set visual styles.
    #[must_use]
    pub const fn styles(mut self, styles: PaneStyles) -> Self {
        self.styles = styles;
        self
    }

    /// Return the content area for the current pane state.
    #[must_use]
    pub const fn inner_area(&self, state: &PaneState) -> Rect {
        let border = if self.border {
            Insets::all(1)
        } else {
            Insets::all(0)
        };
        state.area.inset(border).inset(self.padding)
    }

    /// Track of the border scrollbar of a pane occupying `size`: its right border column
    /// between the corners. `None` without a border or when the pane is too small.
    #[must_use]
    pub const fn border_scrollbar_track(&self, size: Rect) -> Option<Rect> {
        if !self.border || size.width < 2 || size.height < 3 {
            return None;
        }
        Some(Rect::new(
            size.x.saturating_add(size.width - 1),
            size.y.saturating_add(1),
            1,
            size.height - 2,
        ))
    }

    /// Register pane selection geometry using the exact rendered outer and inner areas.
    ///
    /// The content area follows `content_capture`; pane border/title/padding remain
    /// outside initiation and therefore naturally delegate to a parent scope.
    pub fn register_selection(
        &self,
        state: &PaneState,
        selection: &ComponentSelectionState,
        policy: &ComponentSelectionPolicy,
        paint: &mut PaintCx<'_, '_>,
    ) -> ComponentSelectionOutcome {
        paint_component_scope(paint, selection, policy, state.area, self.inner_area(state))
    }

    /// Handle one input event.
    pub fn handle_event(&self, state: &mut PaneState, event: &Event) -> PaneOutcome {
        if state.interaction.disabled {
            return PaneOutcome::Ignored;
        }
        let Event::Mouse(mouse) = event else {
            return PaneOutcome::Ignored;
        };
        self.handle_mouse(state, *mouse)
    }

    fn handle_mouse(&self, state: &mut PaneState, mouse: MouseEvent) -> PaneOutcome {
        if !self.policy.mouse.enabled {
            return PaneOutcome::Ignored;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.handle_mouse_down(state, mouse.position)
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.handle_mouse_drag(state, mouse.position)
            }
            MouseEventKind::Up(MouseButton::Left) => Self::handle_mouse_up(state),
            MouseEventKind::Move => {
                let hovered = state.area.contains(mouse.position);
                if state.interaction.hovered == hovered {
                    PaneOutcome::Handled
                } else {
                    state.interaction.hovered = hovered;
                    PaneOutcome::Redraw
                }
            }
            MouseEventKind::ScrollUp if self.policy.mouse.scroll_wheel => {
                self.handle_scroll(state, mouse.position, ScrollDirection::Up)
            }
            MouseEventKind::ScrollDown if self.policy.mouse.scroll_wheel => {
                self.handle_scroll(state, mouse.position, ScrollDirection::Down)
            }
            MouseEventKind::ScrollLeft if self.policy.mouse.scroll_wheel => {
                self.handle_scroll(state, mouse.position, ScrollDirection::Left)
            }
            MouseEventKind::ScrollRight if self.policy.mouse.scroll_wheel => {
                self.handle_scroll(state, mouse.position, ScrollDirection::Right)
            }
            MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => PaneOutcome::Ignored,
        }
    }

    const fn handle_scroll(
        &self,
        state: &PaneState,
        position: Point,
        direction: ScrollDirection,
    ) -> PaneOutcome {
        if self.inner_area(state).contains(position) {
            PaneOutcome::ScrollDelegated { direction }
        } else {
            PaneOutcome::Ignored
        }
    }

    fn handle_mouse_down(&self, state: &mut PaneState, position: Point) -> PaneOutcome {
        if !state.area.contains(position) {
            return PaneOutcome::Ignored;
        }
        state.interaction.pressed = true;
        state.interaction.hovered = true;
        let resize_handle = self.resize_handle_at(state.area, position);
        if let Some(handle) = resize_handle {
            state.drag = Some(PaneDragState {
                pointer: DragState::new(position),
                original_area: state.area,
                mode: PaneDragMode::Resize(handle),
            });
            state.interaction.focused = true;
            return PaneOutcome::FocusRequested;
        }
        if self.policy.mouse.title_bar_drag && title_bar_area(state.area).contains(position) {
            state.drag = Some(PaneDragState {
                pointer: DragState::new(position),
                original_area: state.area,
                mode: PaneDragMode::Move,
            });
        }
        if self.policy.mouse.click_to_focus {
            state.interaction.focused = true;
            PaneOutcome::FocusRequested
        } else {
            PaneOutcome::Redraw
        }
    }

    fn handle_mouse_drag(&self, state: &mut PaneState, position: Point) -> PaneOutcome {
        let Some(mut drag) = state.drag else {
            return PaneOutcome::Ignored;
        };
        drag.pointer = drag.pointer.moved_to(position);
        state.drag = Some(drag);
        let (dx, dy) = drag.pointer.delta();
        match drag.mode {
            PaneDragMode::Move => {
                let next = clamp_area(move_area(drag.original_area, dx, dy), self.policy.bounds);
                if next == state.area {
                    PaneOutcome::Handled
                } else {
                    state.area = next;
                    PaneOutcome::Moved { area: next }
                }
            }
            PaneDragMode::Resize(handle) => {
                let next = clamp_area(
                    resize_area(drag.original_area, handle, dx, dy, self.policy.bounds),
                    self.policy.bounds,
                );
                if next == state.area {
                    PaneOutcome::Handled
                } else {
                    state.area = next;
                    PaneOutcome::Resized { area: next }
                }
            }
        }
    }

    const fn handle_mouse_up(state: &mut PaneState) -> PaneOutcome {
        if state.interaction.pressed || state.drag.is_some() {
            state.interaction.pressed = false;
            state.drag = None;
            PaneOutcome::Redraw
        } else {
            PaneOutcome::Ignored
        }
    }

    fn resize_regions(&self, area: Rect) -> Vec<(&'static str, Rect)> {
        let handles = self.policy.mouse.resize_handles;
        let right = area.right().saturating_sub(1);
        let bottom = area.bottom().saturating_sub(1);
        let mut regions = Vec::new();
        if handles.top_left {
            regions.push(("top-left", Rect::new(area.x, area.y, 1, 1)));
        }
        if handles.top_right {
            regions.push(("top-right", Rect::new(right, area.y, 1, 1)));
        }
        if handles.bottom_right {
            regions.push(("bottom-right", Rect::new(right, bottom, 1, 1)));
        }
        if handles.bottom_left {
            regions.push(("bottom-left", Rect::new(area.x, bottom, 1, 1)));
        }
        if handles.top {
            regions.push(("top", Rect::new(area.x, area.y, area.width, 1)));
        }
        if handles.right {
            regions.push(("right", Rect::new(right, area.y, 1, area.height)));
        }
        if handles.bottom {
            regions.push(("bottom", Rect::new(area.x, bottom, area.width, 1)));
        }
        if handles.left {
            regions.push(("left", Rect::new(area.x, area.y, 1, area.height)));
        }
        regions
    }

    const fn resize_handle_at(&self, area: Rect, position: Point) -> Option<ResizeHandle> {
        let handles = self.policy.mouse.resize_handles;
        if handles.is_empty() || !area.contains(position) {
            return None;
        }
        let on_left = position.x == area.x;
        let on_right = position.x == area.right().saturating_sub(1);
        let on_top = position.y == area.y;
        let on_bottom = position.y == area.bottom().saturating_sub(1);
        match (on_top, on_right, on_bottom, on_left) {
            (true, false, false, true) if handles.top_left => Some(ResizeHandle::TopLeft),
            (true, true, false, false) if handles.top_right => Some(ResizeHandle::TopRight),
            (false, true, true, false) if handles.bottom_right => Some(ResizeHandle::BottomRight),
            (false, false, true, true) if handles.bottom_left => Some(ResizeHandle::BottomLeft),
            (true, _, _, _) if handles.top => Some(ResizeHandle::Top),
            (_, true, _, _) if handles.right => Some(ResizeHandle::Right),
            (_, _, true, _) if handles.bottom => Some(ResizeHandle::Bottom),
            (_, _, _, true) if handles.left => Some(ResizeHandle::Left),
            _ => None,
        }
    }
}

/// Canonical child-owning pane component.
///
/// Pane interaction state remains caller-owned. Layout, chrome, child placement,
/// hit geometry, and event routing all consume the same resolved tree.
pub struct PaneComponent<'a, 'state> {
    pointer_intent: bmux_tui::pointer::PointerIntent,
    id: LayoutId,
    pane: Pane<'a>,
    state: &'state Cell<PaneState>,
    child: Element<'a>,
    title: Option<Element<'a>>,
    scrollbar: Option<BorderScrollbar<'state>>,
}

/// A scrollbar drawn on a pane's right border for a viewport inside its content.
struct BorderScrollbar<'state> {
    viewport: LayoutId,
    source: &'state dyn ScrollSource,
    thumb: Option<Style>,
}

/// Symbols of a border scrollbar: the border line itself, with a heavy line as the thumb.
const BORDER_SCROLLBAR: ScrollbarPolicy = ScrollbarPolicy::vertical().symbols("│", "│", "┃", "│");

impl<'a, 'state> PaneComponent<'a, 'state> {
    /// Use a component as the title instead of the pane's text title, for example a row
    /// holding a tab bar. It is measured and placed where a text title would be, after the
    /// title-row actions take their room, clipped to the remaining width, and it receives
    /// events, so its own hit regions (such as one per tab) work inside the title row.
    #[must_use]
    pub fn title_content(mut self, title: impl Component + 'a) -> Self {
        self.title = Some(Element::new(title));
        self
    }

    /// Override pointer presentation without changing input routing or mouse policy.
    #[must_use]
    pub const fn pointer_intent(mut self, intent: bmux_tui::pointer::PointerIntent) -> Self {
        self.pointer_intent = intent;
        self
    }

    /// Show a scrollbar on the right border for the scroll viewport laid out as
    /// `viewport` somewhere inside the content (for a selectable list, see
    /// [`crate::selectable_list::list_viewport_id`]), reading and changing the scroll
    /// position held by `source`. It appears only while that content overflows, takes no
    /// content width, and can be dragged or clicked; its hit region is `<id>.scrollbar`.
    /// Its geometry is [`Pane::border_scrollbar_track`].
    #[must_use]
    pub fn border_scrollbar(
        mut self,
        viewport: impl Into<LayoutId>,
        source: &'state dyn ScrollSource,
    ) -> Self {
        self.scrollbar = Some(BorderScrollbar {
            viewport: viewport.into(),
            source,
            thumb: None,
        });
        self
    }

    /// Style of the border scrollbar's thumb. Defaults to the current border style.
    #[must_use]
    pub const fn border_scrollbar_thumb(mut self, style: Style) -> Self {
        if let Some(scrollbar) = &mut self.scrollbar {
            scrollbar.thumb = Some(style);
        }
        self
    }

    /// Create a pane component with stable identity, caller-owned state, and one child.
    #[must_use]
    pub fn new(
        id: impl Into<LayoutId>,
        pane: Pane<'a>,
        state: &'state Cell<PaneState>,
        child: impl Component + 'a,
    ) -> Self {
        Self {
            id: id.into(),
            pointer_intent: bmux_tui::pointer::PointerIntent::Auto,
            pane,
            state,
            child: Element::new(child),
            title: None,
            scrollbar: None,
        }
    }

    const fn border_style(&self) -> Style {
        if self.state.get().interaction.focused {
            self.pane.styles.focused_border
        } else {
            self.pane.styles.border
        }
    }

    /// The border scrollbar with its extent in `layout`, when configured and laid out.
    fn track_scrollbar(
        &self,
        layout: &LayoutNode,
    ) -> Option<(TrackScrollbar, ScrollExtent, &BorderScrollbar<'state>)> {
        let scrollbar = self.scrollbar.as_ref()?;
        let extent = ScrollExtent::of(layout.find(&scrollbar.viewport)?);
        let border = self.border_style();
        let styles = ScrollbarStyles {
            begin: border,
            track: border,
            thumb: scrollbar.thumb.unwrap_or(border),
            end: border,
        };
        Some((
            TrackScrollbar::new()
                .policy(BORDER_SCROLLBAR)
                .styles(styles),
            extent,
            scrollbar,
        ))
    }

    fn surface(&self) -> Surface<'_> {
        let border_style = self.border_style();
        let mut surface = Surface::new(ComponentRef(&self.child))
            .id(format!("{}.surface", self.id.as_str()))
            .padding(self.pane.padding);
        if self.pane.border {
            surface = surface.border(Border::single().style(border_style));
        }
        if let Some(background) = self.pane.styles.background {
            surface = surface.background(background);
        }
        surface
    }
}

impl PaneComponent<'_, '_> {
    /// Title, title-row actions and their hit regions, from one [`Pane::title_layout`].
    fn paint_title_row(
        &self,
        size: Rect,
        state: PaneState,
        title_node: Option<&ChildLayout>,
        cx: &mut PaintCx<'_, '_>,
    ) {
        // The whole-row region goes first so the title's own regions (tabs) sit above it.
        if self.pane.title_hit && !self.pane.policy.mouse.title_bar_drag && !size.is_empty() {
            cx.push_hit(
                SceneRegion::new(format!("{}.title", self.id.as_str()), title_bar_area(size))
                    .role(HitRole::Action)
                    .pointer_intent(self.pointer_intent)
                    .hoverable(true)
                    .focusable(false)
                    .enabled(!state.interaction.disabled),
            );
        }
        let title_row = match (&self.title, title_node) {
            (Some(title), Some(node)) => {
                let width = u16::try_from(node.node.size.width).unwrap_or(u16::MAX);
                cx.with_child(
                    i32::try_from(node.x).unwrap_or(i32::MAX),
                    0,
                    LocalRect::new(0, 0, width, 1),
                    |cx| title.paint(&node.node, cx),
                );
                self.pane.title_layout_for(size, Some(width))
            }
            (Some(_), None) => self.pane.title_layout_for(size, None),
            (None, _) => {
                let title_row = self.pane.title_layout(size);
                if let (Some(title), Some(area)) = (&self.pane.title, title_row.title) {
                    cx.write_line(
                        LocalRect::new(i32::from(area.x), i64::from(area.y), area.width, 1),
                        title,
                    );
                }
                title_row
            }
        };
        for (index, area) in title_row.actions {
            let action = &self.pane.actions[index];
            cx.write_line(
                LocalRect::new(i32::from(area.x), i64::from(area.y), area.width, 1),
                &action.label,
            );
            cx.push_hit(
                SceneRegion::new(format!("{}.action.{}", self.id.as_str(), action.id), area)
                    .role(HitRole::Action)
                    .pointer_intent(self.pointer_intent)
                    .hoverable(true)
                    .focusable(false)
                    .enabled(!state.interaction.disabled),
            );
        }
    }
}

impl Component for PaneComponent<'_, '_> {
    fn revision(&self) -> ComponentRevision {
        let mut child = self.child.revision();
        if let Some(title) = &self.title {
            child = child.combine(title.revision());
        }
        let mut layout = std::collections::hash_map::DefaultHasher::new();
        self.id.as_str().hash(&mut layout);
        format!("{:?}", self.pane.title).hash(&mut layout);
        format!("{:?}", self.pane.padding).hash(&mut layout);
        self.pane.border.hash(&mut layout);
        let state = self.state.get();
        state.area.width.hash(&mut layout);
        state.area.height.hash(&mut layout);

        let mut paint = std::collections::hash_map::DefaultHasher::new();
        format!("{:?}", self.pointer_intent).hash(&mut paint);
        format!("{:?}", self.pane.styles).hash(&mut paint);
        format!("{:?}", self.pane.actions).hash(&mut paint);
        self.pane.title_hit.hash(&mut paint);
        format!("{:?}", self.pane.policy).hash(&mut paint);
        format!("{:?}", state.interaction).hash(&mut paint);
        format!("{:?}", state.drag).hash(&mut paint);
        if let Some(scrollbar) = &self.scrollbar {
            scrollbar.viewport.as_str().hash(&mut paint);
            scrollbar.source.scroll_state().hash(&mut paint);
            format!("{:?}", scrollbar.thumb).hash(&mut paint);
        }
        ComponentRevision::new(layout.finish() ^ child.layout, paint.finish() ^ child.paint)
    }

    /// The pane's node holds the surface (border, padding and content) and, with a title
    /// component, that component placed in the title row.
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        let surface = self.surface().layout(constraints, cx);
        let size = surface.size;
        let mut children = vec![ChildLayout::new(0, 0, surface)];
        if let Some(title) = &self.title {
            let pane = Rect::new(
                0,
                0,
                size.width.try_into().unwrap_or(u16::MAX),
                u16::try_from(size.height).unwrap_or(u16::MAX),
            );
            // The room a title of unbounded width would get beside the actions.
            if let Some(space) = self.pane.title_layout_for(pane, Some(u16::MAX)).title {
                let node = title.layout(Constraints::new(0, space.width.into(), 1, Some(1)), cx);
                children.push(ChildLayout::new(space.x.into(), space.y.into(), node));
            }
        }
        LayoutNode::with_children(self.id.clone(), size, children)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let Some(surface) = layout.children.first() else {
            return;
        };
        self.surface().paint(&surface.node, cx);
        let state = self.state.get();
        let size = Rect::new(
            0,
            0,
            layout.size.width.try_into().unwrap_or(u16::MAX),
            u16::try_from(layout.size.height).unwrap_or(u16::MAX),
        );
        if self.pane.policy.mouse.enabled && !size.is_empty() {
            cx.push_hit(
                SceneRegion::new(self.id.as_str(), size)
                    .role(HitRole::Background)
                    .pointer_intent(self.pointer_intent)
                    .hoverable(true)
                    .focusable(false)
                    .enabled(!state.interaction.disabled),
            );
            if self.pane.policy.mouse.title_bar_drag {
                cx.push_hit(
                    SceneRegion::new(format!("{}.title", self.id.as_str()), title_bar_area(size))
                        .role(HitRole::DragHandle)
                        .pointer_intent(match self.pointer_intent {
                            bmux_tui::pointer::PointerIntent::Auto => {
                                bmux_tui::pointer::PointerIntent::Shape(
                                    if state
                                        .drag
                                        .is_some_and(|drag| matches!(drag.mode, PaneDragMode::Move))
                                    {
                                        bmux_tui::pointer::PointerShape::Grabbing
                                    } else {
                                        bmux_tui::pointer::PointerShape::Grab
                                    },
                                )
                            }
                            intent => intent,
                        })
                        .hoverable(true)
                        .focusable(false)
                        .enabled(!state.interaction.disabled),
                );
            }
            for (suffix, area) in self.pane.resize_regions(size) {
                cx.push_hit(
                    SceneRegion::new(format!("{}.resize.{suffix}", self.id.as_str()), area)
                        .role(HitRole::ResizeHandle)
                        .pointer_intent(match self.pointer_intent {
                            bmux_tui::pointer::PointerIntent::Auto => {
                                bmux_tui::pointer::PointerIntent::Shape(match suffix {
                                    "top" | "bottom" => bmux_tui::pointer::PointerShape::NsResize,
                                    "top-left" | "bottom-right" => {
                                        bmux_tui::pointer::PointerShape::NwseResize
                                    }
                                    "top-right" | "bottom-left" => {
                                        bmux_tui::pointer::PointerShape::NeswResize
                                    }
                                    _ => bmux_tui::pointer::PointerShape::EwResize,
                                })
                            }
                            intent => intent,
                        })
                        .hoverable(true)
                        .focusable(false)
                        .enabled(!state.interaction.disabled),
                );
            }
        }
        if let Some(track) = self.pane.border_scrollbar_track(size)
            && let Some((scrollbar, extent, border)) = self.track_scrollbar(layout)
            && TrackScrollbar::visible(track, extent)
        {
            scrollbar.paint(track, extent, &border.source.scroll_state(), cx);
            cx.push_hit(
                SceneRegion::new(format!("{}.scrollbar", self.id.as_str()), track)
                    .role(HitRole::Scroll)
                    .pointer_intent(self.pointer_intent)
                    .hoverable(true)
                    .focusable(false)
                    .enabled(!state.interaction.disabled),
            );
        }
        self.paint_title_row(size, state, layout.children.get(1), cx);
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        let mut state = self.state.get();
        let surface_id = LayoutId::new(format!("{}.surface", self.id.as_str()));
        state.area = cx.find_visible_rect(&surface_id).unwrap_or_else(|| {
            Rect::new(
                0,
                0,
                layout.size.width.try_into().unwrap_or(u16::MAX),
                u16::try_from(layout.size.height).unwrap_or(u16::MAX),
            )
        });
        self.state.set(state);
        if let Some(track) = self.pane.border_scrollbar_track(state.area)
            && let Some((scrollbar, extent, border)) = self.track_scrollbar(layout)
        {
            let mut scroll = border.source.scroll_state();
            let dragging = scroll.dragging_scrollbar();
            let outcome = scrollbar.handle_event(track, extent, &mut scroll, event);
            border.source.set_scroll_state(scroll);
            if outcome != ScrollViewOutcome::Ignored {
                return EventOutcome::Redraw;
            }
            if dragging || scroll.dragging_scrollbar() {
                return EventOutcome::Handled;
            }
        }
        let outcome = self.pane.handle_event(&mut state, event);
        self.state.set(state);
        if outcome != PaneOutcome::Ignored {
            return EventOutcome::Handled;
        }
        if let (Some(title), Some(node)) = (&self.title, layout.children.get(1)) {
            let x = state
                .area
                .x
                .saturating_add(u16::try_from(node.x).unwrap_or(u16::MAX));
            let clip = Rect::new(
                x,
                state.area.y,
                node.node.size.width.try_into().unwrap_or(u16::MAX),
                u16::try_from(node.node.size.height).unwrap_or(u16::MAX),
            );
            let outcome = cx.with_transform(
                node.x,
                node.y,
                i32::from(x),
                i64::from(state.area.y),
                clip,
                |cx| title.event(event, &node.node, cx),
            );
            if outcome != EventOutcome::Ignored {
                return outcome;
            }
        }
        let Some(child) = layout
            .children
            .first()
            .and_then(|surface| surface.node.children.first())
        else {
            return EventOutcome::Ignored;
        };
        let clip = Rect::new(
            state
                .area
                .x
                .saturating_add(u16::try_from(child.x).unwrap_or(u16::MAX)),
            state
                .area
                .y
                .saturating_add(u16::try_from(child.y).unwrap_or(u16::MAX)),
            child.node.size.width.try_into().unwrap_or(u16::MAX),
            u16::try_from(child.node.size.height).unwrap_or(u16::MAX),
        );
        cx.with_transform(
            child.x,
            child.y,
            i32::from(
                state
                    .area
                    .x
                    .saturating_add(u16::try_from(child.x).unwrap_or(u16::MAX)),
            ),
            i64::from(state.area.y).saturating_add(i64::try_from(child.y).unwrap_or(i64::MAX)),
            clip,
            |cx| self.child.event(event, &child.node, cx),
        )
    }
}

struct ComponentRef<'a>(&'a Element<'a>);

impl Component for ComponentRef<'_> {
    fn revision(&self) -> ComponentRevision {
        self.0.revision()
    }

    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.0.layout(constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.0.paint(layout, cx);
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        self.0.event(event, layout, cx)
    }
}

impl Default for Pane<'_> {
    fn default() -> Self {
        Self::new()
    }
}

fn title_bar_area(area: Rect) -> Rect {
    Rect::new(area.x, area.y, area.width, area.height.min(1))
}

fn move_area(area: Rect, dx: i32, dy: i32) -> Rect {
    Rect::new(
        offset_u16(area.x, dx),
        offset_u16(area.y, dy),
        area.width,
        area.height,
    )
}

fn resize_area(
    area: Rect,
    handle: ResizeHandle,
    dx: i32,
    dy: i32,
    bounds: PaneBoundsPolicy,
) -> Rect {
    let min_width = bounds.min_size.width;
    let min_height = bounds.min_size.height;
    let max_width = bounds.max_size.map_or(u16::MAX, |size| size.width);
    let max_height = bounds.max_size.map_or(u16::MAX, |size| size.height);

    let left_delta = if handle_moves_left(handle) { dx } else { 0 };
    let right_delta = if handle_moves_right(handle) { dx } else { 0 };
    let top_delta = if handle_moves_top(handle) { dy } else { 0 };
    let bottom_delta = if handle_moves_bottom(handle) { dy } else { 0 };

    let left = i32::from(area.x) + left_delta;
    let right = i32::from(area.right()) + right_delta;
    let top = i32::from(area.y) + top_delta;
    let bottom = i32::from(area.bottom()) + bottom_delta;

    let width = clamp_i32(right - left, i32::from(min_width), i32::from(max_width));
    let height = clamp_i32(bottom - top, i32::from(min_height), i32::from(max_height));

    let x = if handle_moves_left(handle) {
        i32::from(area.right()) - width
    } else {
        i32::from(area.x)
    };
    let y = if handle_moves_top(handle) {
        i32::from(area.bottom()) - height
    } else {
        i32::from(area.y)
    };

    Rect::new(
        nonnegative_u16(x),
        nonnegative_u16(y),
        nonnegative_u16(width),
        nonnegative_u16(height),
    )
}

fn clamp_area(area: Rect, bounds: PaneBoundsPolicy) -> Rect {
    let mut next = area;
    if let Some(parent) = bounds.parent {
        let max_x = parent.right().saturating_sub(next.width);
        let max_y = parent.bottom().saturating_sub(next.height);
        next.x = next.x.clamp(parent.x, max_x.max(parent.x));
        next.y = next.y.clamp(parent.y, max_y.max(parent.y));
        if next.right() > parent.right() {
            next.width = parent.right().saturating_sub(next.x);
        }
        if next.bottom() > parent.bottom() {
            next.height = parent.bottom().saturating_sub(next.y);
        }
    }
    next.width = next.width.max(bounds.min_size.width);
    next.height = next.height.max(bounds.min_size.height);
    if let Some(max_size) = bounds.max_size {
        next.width = next.width.min(max_size.width);
        next.height = next.height.min(max_size.height);
    }
    next
}

const fn handle_moves_left(handle: ResizeHandle) -> bool {
    matches!(
        handle,
        ResizeHandle::Left | ResizeHandle::TopLeft | ResizeHandle::BottomLeft
    )
}

const fn handle_moves_right(handle: ResizeHandle) -> bool {
    matches!(
        handle,
        ResizeHandle::Right | ResizeHandle::TopRight | ResizeHandle::BottomRight
    )
}

const fn handle_moves_top(handle: ResizeHandle) -> bool {
    matches!(
        handle,
        ResizeHandle::Top | ResizeHandle::TopLeft | ResizeHandle::TopRight
    )
}

const fn handle_moves_bottom(handle: ResizeHandle) -> bool {
    matches!(
        handle,
        ResizeHandle::Bottom | ResizeHandle::BottomLeft | ResizeHandle::BottomRight
    )
}

fn offset_u16(value: u16, delta: i32) -> u16 {
    nonnegative_u16(i32::from(value) + delta)
}

fn nonnegative_u16(value: i32) -> u16 {
    u16::try_from(value.max(0)).unwrap_or(u16::MAX)
}

const fn clamp_i32(value: i32, min: i32, max: i32) -> i32 {
    if value < min {
        min
    } else if value > max {
        max
    } else {
        value
    }
}

impl crate::theme::ComponentTheme {
    /// Convert this semantic component theme into [`PaneStyles`].
    #[must_use]
    pub fn pane_styles(self) -> PaneStyles {
        PaneStyles::from(self)
    }
}

impl From<crate::theme::ComponentTheme> for PaneStyles {
    fn from(theme: crate::theme::ComponentTheme) -> Self {
        let theme = theme.for_surface(crate::theme::ComponentSurfaceDepth::Raised);
        Self {
            background: Some(theme.surfaces.raised),
            border: theme.border,
            focused_border: theme.focused,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use bmux_tui::buffer::Buffer;
    use bmux_tui::component::{Component, Constraints, EventCx, LayoutCx};
    use bmux_tui::composition::TextBlock;
    use bmux_tui::event::{Event, EventOutcome, MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Insets, Point, Rect, Size};
    use bmux_tui::paint::PaintCx;

    use super::{
        Pane, PaneBoundsPolicy, PaneComponent, PaneMousePolicy, PaneOutcome, PanePolicy, PaneState,
        PaneTitleAction, ResizeHandles,
    };

    /// A 10 by `height` pane over a scroll view of `lines` rows, painted; returns the right
    /// border column's symbols, the scrollbar's hit region and the layout.
    fn scrolled(
        lines: usize,
        height: u16,
        scroll: &Cell<crate::scroll_view::ScrollViewState>,
    ) -> (String, Option<Rect>, bmux_tui::component::LayoutNode) {
        use bmux_tui::component::LogicalSize;
        let pane_state = Cell::new(PaneState::new(Rect::new(0, 0, 10, height)));
        let text = (0..lines)
            .map(|line| format!("row {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let view = crate::scroll_view::ScrollViewComponent::new(
            "view",
            LogicalSize::new(8, u64::from(height - 2)),
            scroll.get(),
            TextBlock::new(text),
        );
        let component = PaneComponent::new("pane", Pane::new(), &pane_state, view)
            .border_scrollbar("view", scroll);
        let layout = component.layout(
            Constraints::new(10, 10, height.into(), Some(height.into())),
            &mut LayoutCx::new(),
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, height));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        let hit = frame
            .hits()
            .regions()
            .iter()
            .find(|region| region.id.as_str() == "pane.scrollbar")
            .map(|region| region.area);
        drop(frame);
        let column = (0..height)
            .filter_map(|y| buffer.get(Point::new(9, y)).map(|cell| cell.symbol.clone()))
            .collect();
        (column, hit, layout)
    }

    #[test]
    fn border_scrollbar_shows_only_when_content_overflows() {
        let scroll = Cell::new(crate::scroll_view::ScrollViewState::new());
        let (column, hit, _) = scrolled(3, 6, &scroll);
        assert_eq!(column, "┐││││┘");
        assert_eq!(hit, None);

        let (column, hit, _) = scrolled(16, 6, &scroll);
        assert_eq!(column, "┐┃│││┘");
        assert_eq!(hit, Some(Rect::new(9, 1, 1, 4)));

        let mut state = scroll.get();
        state.set_vertical_offset(u64::MAX);
        scroll.set(state);
        let (column, _, _) = scrolled(16, 6, &scroll);
        assert_eq!(column, "┐│││┃┘");
    }

    #[test]
    fn border_scrollbar_drags_the_scroll_position() {
        let scroll = Cell::new(crate::scroll_view::ScrollViewState::new());
        let pane_state = Cell::new(PaneState::new(Rect::new(0, 0, 10, 6)));
        let (_, _, layout) = scrolled(16, 6, &scroll);
        let view = TextBlock::new("unused");
        let component = PaneComponent::new("pane", Pane::new(), &pane_state, view)
            .border_scrollbar("view", &scroll);
        let mut cx = EventCx::new(&layout);
        let mut send = |kind, x, y| {
            component.event(
                &Event::Mouse(MouseEvent::new(kind, Point::new(x, y))),
                &layout,
                &mut cx,
            )
        };
        assert_eq!(
            send(MouseEventKind::Down(MouseButton::Left), 9, 4),
            EventOutcome::Redraw
        );
        assert_eq!(scroll.get().vertical_offset(), 12);
        // The drag keeps control outside the track until released.
        assert_eq!(
            send(MouseEventKind::Drag(MouseButton::Left), 2, 1),
            EventOutcome::Redraw
        );
        assert_eq!(scroll.get().vertical_offset(), 0);
        assert_eq!(
            send(MouseEventKind::Up(MouseButton::Left), 2, 1),
            EventOutcome::Handled
        );
        assert!(!scroll.get().dragging_scrollbar());
        // Clicks elsewhere in the pane are not the scrollbar's.
        assert_ne!(
            send(MouseEventKind::Down(MouseButton::Left), 2, 2),
            EventOutcome::Redraw
        );
        assert_eq!(scroll.get().vertical_offset(), 0);
    }

    #[test]
    fn border_scrollbar_track_needs_a_border_and_room() {
        let size = Rect::new(2, 3, 10, 6);
        assert_eq!(
            Pane::new().border_scrollbar_track(size),
            Some(Rect::new(11, 4, 1, 4))
        );
        assert_eq!(Pane::new().border(false).border_scrollbar_track(size), None);
        assert_eq!(
            Pane::new().border_scrollbar_track(Rect::new(0, 0, 10, 2)),
            None
        );
        assert_eq!(
            Pane::new().border_scrollbar_track(Rect::new(0, 0, 1, 6)),
            None
        );
    }

    #[test]
    fn pointer_override_invalidates_paint_without_remeasuring_layout() {
        use bmux_tui::pointer::{PointerIntent, PointerShape};

        let state = Cell::new(PaneState::new(Rect::new(0, 0, 12, 5)));
        let component = PaneComponent::new(
            "pane",
            Pane::new()
                .title("Pane")
                .padding(Insets::all(1))
                .policy(PanePolicy {
                    mouse: PaneMousePolicy {
                        enabled: true,
                        click_to_focus: true,
                        title_bar_drag: true,
                        scroll_wheel: false,
                        resize_handles: ResizeHandles::ALL,
                    },
                    bounds: PaneBoundsPolicy::default(),
                }),
            &state,
            TextBlock::new("body").id("pane.body"),
        );
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
    fn title_preserves_remaining_top_border() {
        for (title, expected) in [
            ("Hi", "┌Hi──────┐"),
            ("界", "┌界──────┐"),
            ("", "┌────────┐"),
            ("1234567890", "┌12345678┐"),
        ] {
            let state = Cell::new(PaneState::new(Rect::new(0, 0, 10, 3)));
            let component = PaneComponent::new(
                "pane",
                Pane::new().border(true).title(title),
                &state,
                TextBlock::new(""),
            );
            let layout =
                component.layout(Constraints::tight(Size::new(10, 3)), &mut LayoutCx::new());
            let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 3));
            component.paint(&layout, &mut PaintCx::new(&mut Frame::new(&mut buffer)));
            let top = buffer.cells()[..10]
                .iter()
                .filter(|cell| !cell.is_wide_continuation())
                .map(|cell| cell.symbol.as_str())
                .collect::<String>();
            assert_eq!(top, expected, "title: {title:?}");
        }
    }

    #[test]
    fn child_text_cannot_overwrite_chrome_at_small_sizes() {
        for width in 0..=8 {
            for height in 0..=5 {
                let state = Cell::new(PaneState::new(Rect::new(0, 0, width, height)));
                let component = PaneComponent::new(
                    "pane",
                    Pane::new().title("界 oversized title"),
                    &state,
                    TextBlock::new("overflowing content\nsecond\nthird\nfourth")
                        .wrap(bmux_tui::text::TextWrap::None),
                );
                let layout = component.layout(
                    Constraints::tight(Size::new(width, height)),
                    &mut LayoutCx::new(),
                );
                let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 7));
                component.paint(&layout, &mut PaintCx::new(&mut Frame::new(&mut buffer)));
                for y in 0..7 {
                    for x in 0..10 {
                        if x >= width || y >= height {
                            assert_eq!(
                                buffer.get(Point::new(x, y)).unwrap().symbol,
                                " ",
                                "width={width} height={height} layout={:?} x={x} y={y}",
                                layout.size
                            );
                        }
                    }
                }
                if width >= 2 && height >= 2 {
                    for y in 1..height - 1 {
                        assert_eq!(buffer.get(Point::new(0, y)).unwrap().symbol, "│");
                        assert_eq!(buffer.get(Point::new(width - 1, y)).unwrap().symbol, "│");
                    }
                    for x in 1..width - 1 {
                        assert_eq!(buffer.get(Point::new(x, height - 1)).unwrap().symbol, "─");
                    }
                }
            }
        }
    }

    #[test]
    fn inner_area_accounts_for_border_and_padding() {
        let pane = Pane::new().padding(Insets::new(1, 2, 1, 2));
        let state = PaneState::new(Rect::new(0, 0, 20, 8));

        assert_eq!(pane.inner_area(&state), Rect::new(3, 2, 14, 4));
    }

    #[test]
    fn title_bar_drag_moves_pane() {
        let pane = Pane::new().policy(PanePolicy {
            mouse: PaneMousePolicy::draggable(),
            bounds: PaneBoundsPolicy::default(),
        });
        let mut state = PaneState::new(Rect::new(2, 2, 10, 4));

        let down = pane.handle_event(
            &mut state,
            &mouse(MouseEventKind::Down(MouseButton::Left), 3, 2),
        );
        let moved = pane.handle_event(
            &mut state,
            &mouse(MouseEventKind::Drag(MouseButton::Left), 6, 4),
        );

        assert_eq!(down, PaneOutcome::FocusRequested);
        assert_eq!(
            moved,
            PaneOutcome::Moved {
                area: Rect::new(5, 4, 10, 4)
            }
        );
    }

    #[test]
    fn drag_clamps_to_parent_bounds() {
        let pane = Pane::new().policy(PanePolicy {
            mouse: PaneMousePolicy::draggable(),
            bounds: PaneBoundsPolicy {
                parent: Some(Rect::new(0, 0, 20, 10)),
                min_size: Size::new(3, 3),
                max_size: None,
            },
        });
        let mut state = PaneState::new(Rect::new(2, 2, 10, 4));

        let _ = pane.handle_event(
            &mut state,
            &mouse(MouseEventKind::Down(MouseButton::Left), 3, 2),
        );
        let moved = pane.handle_event(
            &mut state,
            &mouse(MouseEventKind::Drag(MouseButton::Left), 30, 20),
        );

        assert_eq!(
            moved,
            PaneOutcome::Moved {
                area: Rect::new(10, 6, 10, 4)
            }
        );
    }

    #[test]
    fn bottom_right_resize_grows_pane() {
        let pane = Pane::new().policy(PanePolicy {
            mouse: PaneMousePolicy {
                enabled: true,
                click_to_focus: true,
                title_bar_drag: false,
                scroll_wheel: false,
                resize_handles: ResizeHandles::ALL,
            },
            bounds: PaneBoundsPolicy::default(),
        });
        let mut state = PaneState::new(Rect::new(2, 2, 10, 4));

        let _ = pane.handle_event(
            &mut state,
            &mouse(MouseEventKind::Down(MouseButton::Left), 11, 5),
        );
        let resized = pane.handle_event(
            &mut state,
            &mouse(MouseEventKind::Drag(MouseButton::Left), 14, 7),
        );

        assert_eq!(
            resized,
            PaneOutcome::Resized {
                area: Rect::new(2, 2, 13, 6)
            }
        );
    }

    #[test]
    fn resize_clamps_to_max_size() {
        let pane = Pane::new().policy(PanePolicy {
            mouse: PaneMousePolicy {
                enabled: true,
                click_to_focus: true,
                title_bar_drag: false,
                scroll_wheel: false,
                resize_handles: ResizeHandles::ALL,
            },
            bounds: PaneBoundsPolicy {
                parent: None,
                min_size: Size::new(3, 3),
                max_size: Some(Size::new(12, 5)),
            },
        });
        let mut state = PaneState::new(Rect::new(2, 2, 10, 4));

        let _ = pane.handle_event(
            &mut state,
            &mouse(MouseEventKind::Down(MouseButton::Left), 11, 5),
        );
        let resized = pane.handle_event(
            &mut state,
            &mouse(MouseEventKind::Drag(MouseButton::Left), 30, 20),
        );

        assert_eq!(
            resized,
            PaneOutcome::Resized {
                area: Rect::new(2, 2, 12, 5)
            }
        );
    }

    #[test]
    fn scroll_wheel_inside_inner_area_is_delegated() {
        let pane = Pane::new().padding(Insets::all(1)).policy(PanePolicy {
            mouse: PaneMousePolicy {
                enabled: true,
                click_to_focus: false,
                title_bar_drag: false,
                scroll_wheel: true,
                resize_handles: ResizeHandles::NONE,
            },
            bounds: PaneBoundsPolicy::default(),
        });
        let mut state = PaneState::new(Rect::new(0, 0, 10, 5));

        let outcome = pane.handle_event(&mut state, &mouse(MouseEventKind::ScrollDown, 2, 2));

        assert_eq!(
            outcome,
            PaneOutcome::ScrollDelegated {
                direction: super::ScrollDirection::Down
            }
        );
    }

    #[test]
    fn scroll_wheel_on_chrome_is_ignored() {
        let pane = Pane::new().padding(Insets::all(1)).policy(PanePolicy {
            mouse: PaneMousePolicy {
                enabled: true,
                click_to_focus: false,
                title_bar_drag: false,
                scroll_wheel: true,
                resize_handles: ResizeHandles::NONE,
            },
            bounds: PaneBoundsPolicy::default(),
        });
        let mut state = PaneState::new(Rect::new(0, 0, 10, 5));

        let outcome = pane.handle_event(&mut state, &mouse(MouseEventKind::ScrollDown, 0, 0));

        assert_eq!(outcome, PaneOutcome::Ignored);
    }

    #[test]
    fn component_owns_layout_chrome_child_and_interaction_geometry() {
        let state = Cell::new(PaneState::new(Rect::new(0, 0, 12, 5)));
        let component = PaneComponent::new(
            "pane",
            Pane::new()
                .title("Pane")
                .padding(Insets::all(1))
                .policy(PanePolicy {
                    mouse: PaneMousePolicy {
                        enabled: true,
                        click_to_focus: true,
                        title_bar_drag: true,
                        scroll_wheel: false,
                        resize_handles: ResizeHandles::ALL,
                    },
                    bounds: PaneBoundsPolicy::default(),
                }),
            &state,
            TextBlock::new("body").id("pane.body"),
        );
        let layout = component.layout(Constraints::new(12, 12, 5, Some(5)), &mut LayoutCx::new());
        assert_eq!(layout.size.width, 12);
        assert_eq!(layout.size.height, 5);
        assert!(layout.find(&"pane.surface".into()).is_some());
        assert!(layout.find(&"pane.body".into()).is_some());

        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 5));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(frame.buffer().row_symbols(0).unwrap(), "┌Pane──────┐");
        assert!(frame.buffer().row_symbols(2).unwrap().contains("body"));
        assert!(frame.hits().regions().iter().any(|region| {
            region.id.as_str() == "pane" && region.area == Rect::new(0, 0, 12, 5)
        }));

        assert_eq!(
            component.event(
                &mouse(MouseEventKind::Down(MouseButton::Left), 1, 0),
                &layout,
                &mut EventCx::new(&layout),
            ),
            EventOutcome::Handled
        );
        assert!(state.get().interaction.focused);
        assert!(state.get().is_dragging());
    }

    #[test]
    fn component_clips_nested_paint_and_hit_geometry() {
        let state = Cell::new(PaneState::new(Rect::new(0, 0, 8, 4)));
        let component = PaneComponent::new(
            "pane",
            Pane::new().policy(PanePolicy {
                mouse: PaneMousePolicy {
                    enabled: true,
                    click_to_focus: true,
                    title_bar_drag: false,
                    scroll_wheel: false,
                    resize_handles: ResizeHandles::NONE,
                },
                bounds: PaneBoundsPolicy::default(),
            }),
            &state,
            TextBlock::new("inside").id("pane.body"),
        );
        let layout = component.layout(Constraints::new(8, 8, 4, Some(4)), &mut LayoutCx::new());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 5, 4));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        assert_eq!(frame.hits().regions()[0].area, Rect::new(0, 0, 5, 4));
    }

    fn mouse(kind: MouseEventKind, x: u16, y: u16) -> Event {
        Event::Mouse(MouseEvent::new(kind, Point::new(x, y)))
    }

    fn rendered(pane: Pane<'_>, width: u16) -> (Buffer, Vec<(String, Rect)>) {
        let state = Cell::new(PaneState::new(Rect::new(0, 0, width, 3)));
        let component = PaneComponent::new("pane", pane, &state, TextBlock::new(""));
        let layout = component.layout(
            Constraints::new(width.into(), width.into(), 3, Some(3)),
            &mut LayoutCx::new(),
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, 3));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        let hits = frame
            .hits()
            .regions()
            .iter()
            .map(|region| (region.id.as_str().to_owned(), region.area))
            .collect();
        drop(frame);
        (buffer, hits)
    }

    fn with_actions(title: &str) -> Pane<'static> {
        Pane::new()
            .title(title.to_owned())
            .action(PaneTitleAction::new("collapse", "▾"))
            .action(PaneTitleAction::new("close", "✕"))
    }

    #[test]
    fn title_actions_are_right_aligned_with_matching_hit_regions() {
        let (buffer, hits) = rendered(with_actions("Files").title_hit(true), 14);
        assert_eq!(buffer.row_symbols(0).as_deref(), Some("┌Files────▾─✕┐"));
        assert!(hits.contains(&("pane.action.collapse".to_owned(), Rect::new(10, 0, 1, 1))));
        assert!(hits.contains(&("pane.action.close".to_owned(), Rect::new(12, 0, 1, 1))));
        assert!(hits.contains(&("pane.title".to_owned(), Rect::new(0, 0, 14, 1))));
        // Without the opt-in, the title row registers nothing of its own.
        let (_, hits) = rendered(with_actions("Files"), 14);
        assert!(!hits.iter().any(|(id, _)| id == "pane.title"));
    }

    #[test]
    fn narrow_title_rows_drop_leftmost_actions_then_clip_the_title() {
        // Actions keep their room first; the title is clipped to what remains.
        let (buffer, _) = rendered(with_actions("Files"), 7);
        assert_eq!(buffer.row_symbols(0).as_deref(), Some("┌F─▾─✕┐"));
        // Room for one action only: the leftmost is dropped, with its hit region.
        let (buffer, hits) = rendered(with_actions("Files"), 4);
        assert_eq!(buffer.row_symbols(0).as_deref(), Some("┌─✕┐"));
        assert!(!hits.iter().any(|(id, _)| id == "pane.action.collapse"));
        assert!(hits.contains(&("pane.action.close".to_owned(), Rect::new(2, 0, 1, 1))));
        // Wide graphemes are measured in cells and never split.
        let pane = Pane::new()
            .title("変更点")
            .action(PaneTitleAction::new("close", "✕"));
        let (buffer, _) = rendered(pane, 9);
        let row = buffer.row_symbols(0).unwrap_or_default();
        assert!(row.starts_with("┌変更") && !row.contains('点'), "{row}");
        assert!(row.ends_with("✕┐"), "{row}");
        // Too narrow for even the corners and one cell: nothing is drawn or registered.
        let (_, hits) = rendered(with_actions("Files"), 2);
        assert!(!hits.iter().any(|(id, _)| id.starts_with("pane.action")));
    }

    #[cfg(feature = "tab-bar")]
    fn tabbed(
        width: u16,
        tabs: &std::cell::RefCell<crate::tab_bar::TabBarState>,
    ) -> (Buffer, Vec<(String, Rect)>, bmux_tui::component::LayoutNode) {
        use crate::tab_bar::{TabBarComponent, TabBarPolicy, TabItem};
        use bmux_tui::composition::Row;

        let items = [
            TabItem::new("files", "Files"),
            TabItem::new("trees", "Trees"),
        ];
        let state = Cell::new(PaneState::new(Rect::new(0, 0, width, 3)));
        let title = Row::new().child(TextBlock::new("[2]")).child(
            TabBarComponent::new("tabs", &items, tabs)
                .policy(TabBarPolicy::interactive().padding("", "").separator("·")),
        );
        let component = PaneComponent::new(
            "pane",
            Pane::new()
                .title_hit(true)
                .action(PaneTitleAction::new("close", "✕")),
            &state,
            TextBlock::new("body"),
        )
        .title_content(title);
        let layout = component.layout(
            Constraints::new(width.into(), width.into(), 3, Some(3)),
            &mut LayoutCx::new(),
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, 3));
        let mut frame = Frame::new(&mut buffer);
        component.paint(&layout, &mut PaintCx::new(&mut frame));
        let hits: Vec<(String, Rect)> = frame
            .hits()
            .regions()
            .iter()
            .map(|region| (region.id.as_str().to_owned(), region.area))
            .collect();
        drop(frame);

        // Pointer input on the second tab selects it through the pane.
        let trees = hits_area(&hits, "tabs.trees");
        if let Some(trees) = trees {
            let mut cx = EventCx::new(&layout);
            for kind in [
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
            ] {
                component.event(
                    &Event::Mouse(MouseEvent::new(kind, Point::new(trees.x, trees.y))),
                    &layout,
                    &mut cx,
                );
            }
        }
        (buffer, hits, layout)
    }

    #[cfg(feature = "tab-bar")]
    fn hits_area(hits: &[(String, Rect)], id: &str) -> Option<Rect> {
        hits.iter()
            .find(|(candidate, _)| candidate == id)
            .map(|(_, area)| *area)
    }

    #[cfg(feature = "tab-bar")]
    #[test]
    fn title_components_get_their_own_hit_regions_and_events() {
        let tabs = std::cell::RefCell::new(crate::tab_bar::TabBarState::new(Some(0)));
        let (buffer, hits, _) = tabbed(20, &tabs);
        assert_eq!(
            buffer.row_symbols(0).as_deref(),
            Some("┌[2]Files·Trees───✕┐")
        );
        assert_eq!(hits_area(&hits, "tabs.files"), Some(Rect::new(4, 0, 5, 1)));
        assert_eq!(hits_area(&hits, "tabs.trees"), Some(Rect::new(10, 0, 5, 1)));
        assert_eq!(
            hits_area(&hits, "pane.action.close"),
            Some(Rect::new(18, 0, 1, 1))
        );
        // The tab regions sit above the whole-row title region.
        let title = hits.iter().position(|(id, _)| id == "pane.title").unwrap();
        let files = hits.iter().position(|(id, _)| id == "tabs.files").unwrap();
        assert!(title < files);
        assert_eq!(tabs.borrow().selected(), Some(1));
        // The content still renders under the title row.
        assert_eq!(
            buffer.row_symbols(1).as_deref(),
            Some("│body              │")
        );
    }

    #[cfg(feature = "tab-bar")]
    #[test]
    fn clipped_title_components_drop_hidden_tab_regions() {
        let tabs = std::cell::RefCell::new(crate::tab_bar::TabBarState::new(Some(0)));
        // Room for "[2]" and part of "Files" beside the close action.
        let (buffer, hits, layout) = tabbed(10, &tabs);
        assert_eq!(buffer.row_symbols(0).as_deref(), Some("┌[2]Fil─✕┐"));
        assert_eq!(layout.children[1].node.size.width, 6);
        assert_eq!(hits_area(&hits, "tabs.files"), Some(Rect::new(4, 0, 3, 1)));
        assert_eq!(hits_area(&hits, "tabs.trees"), None);
        assert_eq!(tabs.borrow().selected(), Some(0));
    }
}
