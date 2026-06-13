use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::buffer::Buffer;
use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Point, Rect, Size};
use bmux_tui::prelude::Line;
use bmux_tui::style::Color;
use bmux_tui_components::common::{
    ComponentHitRegion, DragState, HitRegionId, ResizeBounds, hit_region_at,
};
use bmux_tui_components::modal_frame::{ModalFrame, ModalPlacement, ModalSizing, ModalTheme};
use bmux_tui_components::pane::{
    Pane, PaneBoundsPolicy, PaneMousePolicy, PaneOutcome, PanePolicy, PaneState, ResizeHandles,
};
use bmux_tui_components::panel_group::{
    PanelGroup, PanelGroupAxis, PanelGroupOutcome, PanelGroupPolicy, PanelGroupState, PanelSize,
};

pub const WIDTH: u16 = 72;
pub const HEIGHT: u16 = 18;

pub struct LayoutPlaygroundDemo {
    terminal_area: Rect,
    pane_state: PaneState,
    panel_group_state: PanelGroupState,
    message: String,
}

impl LayoutPlaygroundDemo {
    #[must_use]
    pub fn new(terminal_area: Rect) -> Self {
        Self {
            terminal_area,
            pane_state: PaneState::new(Rect::new(2, 2, 30, 8)),
            panel_group_state: PanelGroupState::new([
                PanelSize::fixed(14),
                PanelSize::flex(1),
                PanelSize::fixed(12),
            ]),
            message: "Drag pane title/border or panel-group divider; q quits".to_string(),
        }
    }

    pub fn resize_terminal(&mut self, terminal_area: Rect) {
        self.terminal_area = terminal_area;
        self.message = "Terminal resized; pane bounds updated".to_string();
    }

    pub fn render(&self, frame: &mut Frame<'_>) {
        render_playground(
            frame,
            self.terminal_area,
            &self.pane_state,
            &self.panel_group_state,
            &self.message,
        );
    }

    pub fn handle_event(&mut self, event: &Event) -> bool {
        if self.handle_unfocus(event) {
            return false;
        }
        if matches!(event, Event::Key(stroke) if should_quit(*stroke)) {
            return true;
        }
        let panel_group = interactive_panel_group();
        match panel_group.handle_event(panel_group_area(), &mut self.panel_group_state, event) {
            PanelGroupOutcome::Focused { panel } => {
                self.message = format!("Panel group focused panel {panel}");
                return false;
            }
            PanelGroupOutcome::DividerDragStarted { divider } => {
                self.message = format!("Panel group divider {divider} drag started");
                return false;
            }
            PanelGroupOutcome::Resized {
                divider,
                before,
                after,
            } => {
                self.message = format!("Panel group divider {divider}: {before}/{after}");
                return false;
            }
            PanelGroupOutcome::DividerDragEnded { divider } => {
                self.message = format!("Panel group divider {divider} drag ended");
                return false;
            }
            PanelGroupOutcome::Redraw | PanelGroupOutcome::Handled => return false,
            PanelGroupOutcome::Ignored => {}
        }
        let pane = interactive_pane(self.terminal_area);
        match pane.handle_event(&mut self.pane_state, event) {
            PaneOutcome::FocusRequested => self.message = "Pane focused".to_string(),
            PaneOutcome::Moved { area } => self.message = format_area("Moved", area),
            PaneOutcome::Resized { area } => self.message = format_area("Resized", area),
            PaneOutcome::Redraw => self.message = "Pane redraw".to_string(),
            PaneOutcome::Handled => {}
            PaneOutcome::Ignored | PaneOutcome::ScrollDelegated { .. } => {}
        }
        false
    }

    fn handle_unfocus(&mut self, event: &Event) -> bool {
        match event {
            Event::Key(stroke)
                if stroke.key == KeyCode::Escape && self.pane_state.interaction.focused =>
            {
                self.clear_focus();
                true
            }
            Event::Mouse(mouse)
                if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                    && !self.pane_state.area.contains(mouse.position)
                    && !panel_group_area().contains(mouse.position) =>
            {
                self.clear_focus();
                true
            }
            Event::Key(_)
            | Event::Mouse(_)
            | Event::Resize(_)
            | Event::Paste(_)
            | Event::Focus(_)
            | Event::Tick
            | Event::User(_) => false,
        }
    }

    fn clear_focus(&mut self) {
        self.pane_state.interaction.focused = false;
        self.pane_state.interaction.hovered = false;
        self.pane_state.interaction.pressed = false;
        self.message = "Pane unfocused".to_string();
    }

    #[must_use]
    pub const fn pane_area(&self) -> Rect {
        self.pane_state.area
    }

    #[must_use]
    pub fn panel_group_widths(&self) -> Vec<u16> {
        interactive_panel_group()
            .layout(panel_group_area(), &self.panel_group_state)
            .panels
            .iter()
            .map(|panel| panel.width)
            .collect()
    }
}

impl Default for LayoutPlaygroundDemo {
    fn default() -> Self {
        Self::new(Rect::new(0, 0, WIDTH, HEIGHT))
    }
}

pub fn render_layout_playground() -> Buffer {
    let mut buffer = Buffer::empty(Rect::new(0, 0, WIDTH, HEIGHT));
    let mut frame = Frame::new(&mut buffer);
    LayoutPlaygroundDemo::default().render(&mut frame);
    buffer
}

pub fn render_layout_playground_into(frame: &mut Frame<'_>) {
    LayoutPlaygroundDemo::default().render(frame);
}

fn render_playground(
    frame: &mut Frame<'_>,
    terminal_area: Rect,
    pane_state: &PaneState,
    panel_group_state: &PanelGroupState,
    message: &str,
) {
    let pane = interactive_pane(terminal_area);
    pane.render(pane_state, frame);
    frame.write_line(
        pane.inner_area(pane_state),
        &Line::from("Drag title or resize border"),
    );

    let modal = ModalFrame::new(
        ModalSizing::new(Size::new(26, 7), Size::new(26, 7), Insets::all(0)),
        ModalTheme::dark(Color::Magenta),
    )
    .placement(ModalPlacement::UpperThird)
    .title("Upper third");
    modal.render(Rect::new(36, 1, 32, 14), frame);
    modal.render_line(
        modal.content_area(Rect::new(36, 1, 32, 14)),
        &Line::from("Modal placement"),
        frame,
    );

    let group = interactive_panel_group();
    let group_area = panel_group_area();
    let group_layout = group.layout(group_area, panel_group_state);
    for (index, area) in group_layout.panels.iter().copied().enumerate() {
        frame.write_line(
            Rect::new(
                area.x.saturating_add(1),
                area.y,
                area.width.saturating_sub(1),
                1,
            ),
            &Line::from(format!("Panel {index}")),
        );
    }
    group.render_dividers(group_area, panel_group_state, frame);

    frame.write_line(Rect::new(1, 16, 68, 1), &Line::from(message));
}

fn panel_group_area() -> Rect {
    Rect::new(2, 12, 64, 3)
}

fn interactive_panel_group() -> PanelGroup {
    PanelGroup::new(PanelGroupAxis::Horizontal).policy(PanelGroupPolicy::interactive())
}

fn interactive_pane(parent: Rect) -> Pane<'static> {
    Pane::new()
        .title("Drag/resize me")
        .padding(Insets::all(1))
        .policy(PanePolicy {
            mouse: PaneMousePolicy {
                enabled: true,
                click_to_focus: true,
                title_bar_drag: true,
                scroll_wheel: false,
                resize_handles: ResizeHandles {
                    top: false,
                    right: true,
                    bottom: true,
                    left: false,
                    top_left: false,
                    top_right: false,
                    bottom_left: false,
                    bottom_right: true,
                },
            },
            bounds: PaneBoundsPolicy {
                parent: Some(parent),
                min_size: Size::new(12, 5),
                max_size: Some(Size::new(40, 12)),
            },
        })
}

pub fn demonstrate_drag_delta() -> (i32, i32) {
    DragState::new(Point::new(2, 3))
        .moved_to(Point::new(7, 1))
        .delta()
}

pub fn demonstrate_hit_region() -> Option<HitRegionId> {
    let regions = [
        ComponentHitRegion::new(HitRegionId(10), Rect::new(0, 0, 5, 5)),
        ComponentHitRegion::new(HitRegionId(20), Rect::new(10, 0, 5, 5)),
    ];
    hit_region_at(&regions, Point::new(11, 2))
}

pub fn demonstrate_resize_bounds() -> Size {
    ResizeBounds::new(Size::new(4, 4), Some(Size::new(12, 8))).clamp(Size::new(20, 2))
}

pub fn demonstrate_pane_drag() -> Rect {
    let mut demo = LayoutPlaygroundDemo::default();
    let _ = demo.handle_event(&mouse_event(MouseEventKind::Down(MouseButton::Left), 4, 2));
    let _ = demo.handle_event(&mouse_event(MouseEventKind::Drag(MouseButton::Left), 9, 5));
    demo.pane_area()
}

pub fn demonstrate_pane_resize() -> Rect {
    let mut demo = LayoutPlaygroundDemo::default();
    let area = demo.pane_area();
    let x = area.right().saturating_sub(1);
    let y = area.bottom().saturating_sub(1);
    let _ = demo.handle_event(&mouse_event(MouseEventKind::Down(MouseButton::Left), x, y));
    let _ = demo.handle_event(&mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        x.saturating_add(5),
        y.saturating_add(3),
    ));
    demo.pane_area()
}

pub fn demonstrate_pane_resize_clamps_to_max() -> Rect {
    let mut demo = LayoutPlaygroundDemo::default();
    let area = demo.pane_area();
    let x = area.right().saturating_sub(1);
    let y = area.bottom().saturating_sub(1);
    let _ = demo.handle_event(&mouse_event(MouseEventKind::Down(MouseButton::Left), x, y));
    let _ = demo.handle_event(&mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        70,
        17,
    ));
    demo.pane_area()
}

pub fn demonstrate_panel_group_resize() -> Vec<u16> {
    let mut demo = LayoutPlaygroundDemo::default();
    let group_area = panel_group_area();
    let divider_x = group_area.x.saturating_add(14);
    let y = group_area.y;
    let _ = demo.handle_event(&mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        divider_x,
        y,
    ));
    let _ = demo.handle_event(&mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        divider_x.saturating_add(4),
        y,
    ));
    demo.panel_group_widths()
}

pub fn demonstrate_click_outside_unfocuses_pane() -> bool {
    let mut demo = LayoutPlaygroundDemo::default();
    let _ = demo.handle_event(&mouse_event(MouseEventKind::Down(MouseButton::Left), 4, 2));
    let _ = demo.handle_event(&mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        60,
        16,
    ));
    demo.pane_state.interaction.focused
}

pub fn demonstrate_escape_unfocuses_pane() -> bool {
    let mut demo = LayoutPlaygroundDemo::default();
    let _ = demo.handle_event(&mouse_event(MouseEventKind::Down(MouseButton::Left), 4, 2));
    let _ = demo.handle_event(&Event::Key(KeyStroke::simple(KeyCode::Escape)));
    demo.pane_state.interaction.focused
}

pub fn rows(buffer: &Buffer) -> Vec<String> {
    (0..buffer.area().height)
        .filter_map(|row| buffer.row_symbols(row))
        .collect()
}

fn mouse_event(kind: MouseEventKind, x: u16, y: u16) -> Event {
    Event::Mouse(MouseEvent::new(kind, Point::new(x, y)))
}

fn format_area(prefix: &str, area: Rect) -> String {
    format!(
        "{prefix}: x={} y={} w={} h={}",
        area.x, area.y, area.width, area.height
    )
}

fn should_quit(stroke: KeyStroke) -> bool {
    stroke.key == KeyCode::Char('q')
}

#[cfg(test)]
mod tests {
    use bmux_tui::geometry::{Rect, Size};
    use bmux_tui_components::common::HitRegionId;

    use super::{
        demonstrate_click_outside_unfocuses_pane, demonstrate_drag_delta,
        demonstrate_escape_unfocuses_pane, demonstrate_hit_region, demonstrate_pane_drag,
        demonstrate_pane_resize, demonstrate_pane_resize_clamps_to_max,
        demonstrate_panel_group_resize, demonstrate_resize_bounds, render_layout_playground, rows,
    };

    #[test]
    fn layout_playground_renders_panes_and_modals() {
        let rendered = rows(&render_layout_playground()).join("\n");

        assert!(rendered.contains("Drag/resize me"));
        assert!(rendered.contains("Modal placement"));
        assert!(rendered.contains("Panel 0"));
        assert!(rendered.contains("Drag pane title/border or panel-group divider"));
    }

    #[test]
    fn shared_geometry_helpers_are_demonstrated() {
        assert_eq!(demonstrate_drag_delta(), (5, -2));
        assert_eq!(demonstrate_hit_region(), Some(HitRegionId(20)));
        assert_eq!(demonstrate_resize_bounds(), Size::new(12, 4));
    }

    #[test]
    fn panel_group_resizes_from_divider_drag() {
        assert_eq!(demonstrate_panel_group_resize(), vec![18, 32, 12]);
    }

    #[test]
    fn click_outside_or_escape_unfocuses_pane() {
        assert!(!demonstrate_click_outside_unfocuses_pane());
        assert!(!demonstrate_escape_unfocuses_pane());
    }

    #[test]
    fn pane_drag_moves_persistent_state() {
        assert_eq!(demonstrate_pane_drag(), Rect::new(7, 5, 30, 8));
    }

    #[test]
    fn pane_resize_updates_persistent_state() {
        assert_eq!(demonstrate_pane_resize(), Rect::new(2, 2, 35, 11));
    }

    #[test]
    fn pane_resize_clamps_to_max_bounds() {
        assert_eq!(
            demonstrate_pane_resize_clamps_to_max(),
            Rect::new(2, 2, 40, 12)
        );
    }
}
