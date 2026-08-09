//! Generic configurable pane/surface component.
//!
//! This pane is a neutral UI surface. It has no BMUX product semantics.

use bmux_tui::chrome::{Border, Panel};
use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Point, Rect, Size};
use bmux_tui::prelude::{Line, Style};
use bmux_tui::style::Modifier;
use bmux_tui::widget::Widget;

use crate::common::{DragState, InteractionState};

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

/// Configurable pane renderer and event handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane<'a> {
    title: Option<Line>,
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
    pub fn inner_area(&self, state: &PaneState) -> Rect {
        self.panel(state).inner_area(state.area)
    }

    /// Render pane chrome and background.
    pub fn render(&self, state: &PaneState, frame: &mut Frame<'_>) {
        self.panel(state).render(state.area, frame);
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

    fn panel(&self, state: &PaneState) -> Panel {
        let border_style = if state.interaction.focused {
            self.styles.focused_border
        } else {
            self.styles.border
        };
        let mut panel = Panel::new().padding(self.padding);
        if self.border {
            panel = panel.border(Border::single().style(border_style));
        }
        if let Some(title) = self.title.clone() {
            panel = panel.title(title);
        }
        if let Some(background) = self.styles.background {
            panel = panel.background(background);
        }
        panel
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

    fn handle_scroll(
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
    use bmux_tui::buffer::Buffer;
    use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Insets, Point, Rect, Size};

    use super::{
        Pane, PaneBoundsPolicy, PaneMousePolicy, PaneOutcome, PanePolicy, PaneState, ResizeHandles,
    };

    #[test]
    fn inner_area_accounts_for_border_and_padding() {
        let pane = Pane::new().padding(Insets::new(1, 2, 1, 2));
        let state = PaneState::new(Rect::new(0, 0, 20, 8));

        assert_eq!(pane.inner_area(&state), Rect::new(3, 2, 14, 4));
    }

    #[test]
    fn renders_bordered_pane() {
        let pane = Pane::new().title("Pane");
        let state = PaneState::new(Rect::new(0, 0, 10, 3));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 3));
        let mut frame = Frame::new(&mut buffer);

        pane.render(&state, &mut frame);

        assert_eq!(frame.buffer().row_symbols(0).as_deref(), Some("┌Pane────┐"));
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

    fn mouse(kind: MouseEventKind, x: u16, y: u16) -> Event {
        Event::Mouse(MouseEvent::new(kind, Point::new(x, y)))
    }
}
