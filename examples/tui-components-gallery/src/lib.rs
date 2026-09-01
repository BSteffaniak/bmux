use std::cell::{Cell, RefCell};

use bmux_text_edit::TextEditBuffer;
use bmux_tui::buffer::Buffer;
use bmux_tui::component::{Component, Constraints, LayoutCx, LogicalSize};
use bmux_tui::composition::TextContent;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span, Text, TextWrap};
use bmux_tui::style::{Color, Style};
use bmux_tui_components::action_row::{ActionButton, ActionRowComponent, ActionRowState};
use bmux_tui_components::badge::{BadgeComponent, BadgePolicy, BadgeSeverity};
use bmux_tui_components::bar_chart::{
    BarChartComponent, BarChartItem, BarChartPolicy, BarChartValuePlacement,
};
use bmux_tui_components::button::{ButtonComponent, ButtonState};
use bmux_tui_components::canvas::{
    Canvas, CanvasBounds, CanvasCircle, CanvasLine, CanvasPoint, CanvasRect,
};
use bmux_tui_components::chart::{
    Chart, ChartAxes, ChartAxis, ChartAxisVisibility, ChartBounds, ChartDataset,
    ChartLegendPlacement, ChartPoint, ChartPolicy,
};
use bmux_tui_components::dialog::{Dialog, DialogComponent};
use bmux_tui_components::empty_state::{EmptyStateComponent, EmptyStatePolicy};
use bmux_tui_components::form_field::FormFieldComponent;
use bmux_tui_components::labeled_details::{DetailItem, LabeledDetailsComponent};
use bmux_tui_components::modal_frame::{
    ModalFrame, ModalFrameComponent, ModalPlacement, ModalSizing, ModalTheme,
};
use bmux_tui_components::pane::{Pane, PaneState};
use bmux_tui_components::picker_frame::{PickerFrame, PickerFrameComponent, PickerFramePolicy};
use bmux_tui_components::progress_bar::{
    ProgressBarComponent, ProgressBarPolicy, ProgressBarValue, ProgressLabelPlacement,
};
use bmux_tui_components::scroll_view::{ScrollViewComponent, ScrollViewState};
use bmux_tui_components::sparkline::{SparklineComponent, SparklinePolicy};
use bmux_tui_components::stepper::{StepItem, StepStatus, StepperComponent, StepperPolicy};
use bmux_tui_components::table::{Table, TableColumn, TableRow, TableState};
use bmux_tui_components::text_input::{TextInputPolicy, TextInputState};
use bmux_tui_components::text_input_box::{TextInputBoxComponent, TextInputBoxPolicy};
use bmux_tui_components::toast_stack::{
    ToastItem, ToastSeverity, ToastStackComponent, ToastStackState,
};

pub const WIDTH: u16 = 72;
pub const HEIGHT: u16 = 30;

pub fn render_gallery() -> Buffer {
    let mut buffer = Buffer::empty(Rect::new(0, 0, WIDTH, HEIGHT));
    let mut frame = Frame::new(&mut buffer);
    render_gallery_into(&mut frame);
    buffer
}

pub fn render_gallery_into(frame: &mut Frame<'_>) {
    render_gallery_interactive(frame, None);
}

/// Render the gallery with focus styling driven by a committed semantic target.
pub fn render_gallery_interactive(frame: &mut Frame<'_>, focused: Option<&str>) {
    let theme = ModalTheme::dark(Color::Cyan);

    render_buttons(frame, focused);
    render_badges(frame);
    render_details(frame);
    render_field(frame);
    render_pane(frame);
    render_progress(frame);
    render_empty_state(frame);
    render_picker(frame);
    render_stepper(frame);
    render_bar_chart(frame);
    render_modal(frame, theme);
    render_dialog(frame, theme);
    render_toasts(frame);
    render_chart(frame);
    render_canvas(frame);
    render_recent_text_polish(frame);
}

fn render_buttons(frame: &mut Frame<'_>, focused_id: Option<&str>) {
    let mut focused = ButtonState::new();
    focused.interaction.focused = focused_id == Some("gallery.save");
    let focused = Cell::new(focused);
    render_component(
        &ButtonComponent::new("gallery.save", "Save", &focused),
        Rect::new(1, 1, 10, 1),
        frame,
    );

    let mut disabled = ButtonState::new();
    disabled.interaction.disabled = true;
    let disabled = Cell::new(disabled);
    render_component(
        &ButtonComponent::new("gallery.disabled", "Disabled", &disabled),
        Rect::new(13, 1, 14, 1),
        frame,
    );

    let actions = [
        ActionButton::new("accept", "Accept"),
        ActionButton::new("cancel", "Cancel"),
    ];
    let mut state = ActionRowState::new();
    let action_focus = focused_id.and_then(|id| match id {
        "gallery.actions.accept" => Some(0),
        "gallery.actions.cancel" => Some(1),
        _ => None,
    });
    state.set_focused(action_focus);
    let state = Cell::new(state);
    render_component(
        &ActionRowComponent::new("gallery.actions", &actions, &state),
        Rect::new(1, 3, 30, 1),
        frame,
    );
}

fn render_component(component: &impl Component, area: Rect, frame: &mut Frame<'_>) {
    let mut layout_cx = LayoutCx::new();
    let layout = component.layout(Constraints::tight(area.size()), &mut layout_cx);
    let mut paint_cx = PaintCx::new(frame);
    paint_cx.with_child(
        i32::from(area.x),
        i64::from(area.y),
        LocalRect::new(0, 0, area.width, area.height),
        |cx| component.paint(&layout, cx),
    );
}

fn render_badges(frame: &mut Frame<'_>) {
    render_badge_component(
        BadgeComponent::new("gallery.badge.info", "info")
            .severity(BadgeSeverity::Info)
            .policy(BadgePolicy::pill().uppercase(true)),
        Rect::new(1, 3, 10, 1),
        frame,
    );
    render_badge_component(
        BadgeComponent::new("gallery.badge.ok", "ok").severity(BadgeSeverity::Success),
        Rect::new(12, 3, 8, 1),
        frame,
    );
    render_badge_component(
        BadgeComponent::new("gallery.badge.warn", "warn").severity(BadgeSeverity::Warning),
        Rect::new(21, 3, 10, 1),
        frame,
    );
}

fn render_badge_component(component: BadgeComponent<'_>, area: Rect, frame: &mut Frame<'_>) {
    render_component(&component, area, frame);
}

fn render_details(frame: &mut Frame<'_>) {
    let items = [
        DetailItem::new("Component", "LabeledDetails"),
        DetailItem::new("Purpose", "Wrapped labels and values"),
    ];
    render_component(
        &LabeledDetailsComponent::new("gallery.details", &items),
        Rect::new(1, 5, 34, 5),
        frame,
    );
}

fn render_field(frame: &mut Frame<'_>) {
    render_component(
        &FormFieldComponent::new(
            "gallery.form-field",
            "Project",
            TextContent::new("bmux").id("gallery.form-field.control"),
        )
        .required(true)
        .help("Shown with a required marker")
        .error("Example error text"),
        Rect::new(38, 1, 30, 5),
        frame,
    );
}

fn render_pane(frame: &mut Frame<'_>) {
    let pane = Pane::new().title("Pane").padding(Insets::all(1));
    let state = PaneState::new(Rect::new(1, 11, 28, 8));
    pane.render(&state, frame);
    frame.write_line(pane.inner_area(&state), &Line::from("Pane content area"));
}

fn render_progress(frame: &mut Frame<'_>) {
    render_component(
        &ProgressBarComponent::new(
            "gallery.progress.indexed",
            ProgressBarValue::determinate(7, 10),
        )
        .label("70% indexed")
        .policy(
            ProgressBarPolicy::compact()
                .background(true)
                .label(ProgressLabelPlacement::Right),
        ),
        Rect::new(1, 20, 28, 1),
        frame,
    );
    render_component(
        &ProgressBarComponent::new(
            "gallery.progress.ratio",
            ProgressBarValue::determinate(1, 3),
        )
        .policy(
            ProgressBarPolicy::compact()
                .line_gauge()
                .symbols("━", "─", "╸"),
        ),
        Rect::new(1, 21, 28, 1),
        frame,
    );
    render_component(
        &ProgressBarComponent::new(
            "gallery.progress.indeterminate",
            ProgressBarValue::indeterminate(4),
        )
        .policy(ProgressBarPolicy::bare()),
        Rect::new(1, 22, 28, 1),
        frame,
    );
    let samples = [1, 2, 3, 5, 8, 13, 8, 5, 3, 2, 1];
    render_component(
        &SparklineComponent::new("gallery.sparkline", &samples)
            .policy(SparklinePolicy::bare().max(Some(13))),
        Rect::new(1, 23, 28, 1),
        frame,
    );
}

fn render_empty_state(frame: &mut Frame<'_>) {
    let body = [Line::from("No matching components yet")];
    let actions = [Line::from("Press / to filter")];
    render_component(
        &EmptyStateComponent::new("gallery.empty-state", "Empty State")
            .icon("∅")
            .body(&body)
            .actions(&actions)
            .policy(EmptyStatePolicy::centered()),
        Rect::new(2, 14, 26, 4),
        frame,
    );
}

fn render_stepper(frame: &mut Frame<'_>) {
    let steps = [
        StepItem::new("plan", "Plan").status(StepStatus::Complete),
        StepItem::new("build", "Build").status(StepStatus::Current),
        StepItem::new("ship", "Ship"),
    ];
    render_component(
        &StepperComponent::new("gallery.stepper", &steps).policy(StepperPolicy::horizontal()),
        Rect::new(35, 12, 33, 1),
        frame,
    );
}

fn render_bar_chart(frame: &mut Frame<'_>) {
    let mem_group = [4, 6];
    let items = [
        BarChartItem::new("CPU", 7),
        BarChartItem::new("Mem", 4).group(&mem_group),
    ];
    render_component(
        &BarChartComponent::new("gallery.bar-chart", &items).policy(
            BarChartPolicy::with_values()
                .max(Some(10))
                .bar_width(Some(12))
                .bar_gap(1)
                .value_placement(BarChartValuePlacement::Right),
        ),
        Rect::new(35, 13, 32, 3),
        frame,
    );
}

fn render_chart(frame: &mut Frame<'_>) {
    let trend = [
        ChartPoint::new(0.0, 1.0),
        ChartPoint::new(1.0, 2.0),
        ChartPoint::new(2.0, 1.5),
        ChartPoint::new(3.0, 3.0),
    ];
    let points = [ChartPoint::new(0.5, 2.5), ChartPoint::new(2.5, 2.0)];
    let datasets = [
        ChartDataset::line("trend", &trend).marker("·"),
        ChartDataset::scatter("events", &points).marker("◆"),
    ];
    let x_labels = ["0", "3"];
    let y_labels = ["3", "0"];
    render_component(
        &Chart::new(&datasets, ChartBounds::new(0.0, 3.0, 0.0, 3.0))
            .axes(
                ChartAxes::empty()
                    .x(ChartAxis::empty().title("x").labels(&x_labels))
                    .y(ChartAxis::empty().title("y").labels(&y_labels)),
            )
            .policy(
                ChartPolicy::compact()
                    .axes(ChartAxisVisibility::Visible)
                    .legend(ChartLegendPlacement::TopRight),
            ),
        Rect::new(35, 17, 32, 5),
        frame,
    );
}

fn render_canvas(frame: &mut Frame<'_>) {
    let points = [CanvasPoint::new(1.0, 1.0, "●")];
    let lines = [CanvasLine::new(0.0, 0.0, 3.0, 2.0, "·")];
    let rects = [
        CanvasRect::new(0.0, 0.0, 3.0, 2.0, "□"),
        CanvasRect::new(0.2, 0.2, 0.8, 0.8, "▒").fill(),
    ];
    let circles = [CanvasCircle::new(2.0, 1.0, 0.8, "○")];

    render_component(
        &Canvas::new(&points, CanvasBounds::new(0.0, 3.0, 0.0, 2.0))
            .lines(&lines)
            .rects(&rects)
            .circles(&circles),
        Rect::new(35, 22, 18, 2),
        frame,
    );
}

fn render_recent_text_polish(frame: &mut Frame<'_>) {
    let scroll_lines = [
        Line::from("horizontal scroll area"),
        Line::from("wide content with gutter"),
        Line::from("bottom row visible"),
    ];
    let mut scroll_state = ScrollViewState::new();
    scroll_state.set_vertical_offset(1);
    scroll_state.set_horizontal_offset(5);
    render_component(
        &ScrollViewComponent::new(
            "gallery.scroll",
            LogicalSize::new(21, 3),
            scroll_state,
            TextContent::new(Text::from_lines(scroll_lines)).wrap(TextWrap::None),
        )
        .content_width(24),
        Rect::new(1, 24, 22, 3),
        frame,
    );

    let text_lines = [Line::from_spans([
        Span::styled("Styled ", Style::new().fg(Color::Yellow)),
        Span::styled("wrapping ", Style::new().fg(Color::Cyan)),
        Span::styled("demo", Style::new().fg(Color::Magenta)),
    ])];
    render_component(
        &TextContent::new(Text::from_lines(text_lines))
            .id("gallery.styled-text")
            .wrap(TextWrap::Word),
        Rect::new(25, 24, 14, 3),
        frame,
    );

    let columns = [TableColumn::new("Rich").fixed(8)];
    let rows = [TableRow::rich([Line::from_spans([
        Span::styled("red", Style::new().fg(Color::Red)),
        Span::styled("blue", Style::new().fg(Color::Blue)),
    ])])];
    Table::new(&columns, &rows).render(Rect::new(41, 24, 10, 3), &TableState::new(Some(0)), frame);

    render_badge_component(
        BadgeComponent::new("gallery.badge.truncated", "truncated-style")
            .severity(BadgeSeverity::Info),
        Rect::new(53, 24, 12, 1),
        frame,
    );
}

fn render_toasts(frame: &mut Frame<'_>) {
    let toasts = [
        ToastItem::new("saved", "Saved")
            .body("Changes persisted")
            .severity(ToastSeverity::Success),
        ToastItem::new("sync", "Syncing").severity(ToastSeverity::Info),
    ];
    let state = Cell::new(ToastStackState::default());
    render_component(
        &ToastStackComponent::new("gallery.toasts", &toasts, &state),
        Rect::new(1, 20, 30, 4),
        frame,
    );
}

fn render_picker(frame: &mut Frame<'_>) {
    let picker = PickerFrame::new()
        .title("Command Palette")
        .header("Commands")
        .footer("enter select · esc close")
        .policy(PickerFramePolicy::palette().max_size(Size::new(30, 12)));
    let input_state = RefCell::new(TextInputState::new(TextEditBuffer::from_text("open")));
    let input = TextInputBoxComponent::new(
        "gallery.picker.input",
        TextInputPolicy::chat_composer(),
        &input_state,
    )
    .policy(TextInputBoxPolicy::bare().focused(true));
    let list = TextContent::new("Open file\nClose window").id("gallery.picker.list");
    render_component(
        &PickerFrameComponent::new("gallery.picker", picker, list).input(input),
        Rect::new(34, 6, 34, 12),
        frame,
    );
}

fn render_modal(frame: &mut Frame<'_>, theme: ModalTheme) {
    let modal = ModalFrame::new(
        ModalSizing::new(Size::new(24, 7), Size::new(24, 7), Insets::all(0)),
        theme,
    )
    .title("Anchored modal")
    .placement(ModalPlacement::Anchored(bmux_tui::geometry::Point::new(
        2, 0,
    )));
    render_component(
        &ModalFrameComponent::new(
            "gallery.modal",
            modal,
            TextContent::new("Opaque modal frame").id("gallery.modal.body"),
        ),
        Rect::new(34, 18, 34, 6),
        frame,
    );
}

fn render_dialog(frame: &mut Frame<'_>, theme: ModalTheme) {
    let body = [Line::from("Dialog body with actions")];
    let actions = [ActionButton::new("ok", "OK")];
    let mut action_state = ActionRowState::new();
    action_state.set_focused(Some(0));
    let state = Cell::new(action_state);
    let dialog = Dialog::new(&body, &actions, theme)
        .title("Dialog")
        .sizing(ModalSizing::new(
            Size::new(30, 7),
            Size::new(30, 7),
            Insets::all(0),
        ));
    render_component(
        &DialogComponent::new("gallery.dialog", dialog, &state),
        Rect::new(35, 13, 34, 9),
        frame,
    );
}

pub fn rows(buffer: &Buffer) -> Vec<String> {
    (0..buffer.area().height)
        .filter_map(|row| buffer.row_symbols(row))
        .collect()
}

#[cfg(test)]
mod tests {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::Rect;
    use bmux_tui::interaction::InteractionRouter;

    use super::{HEIGHT, WIDTH, render_gallery, render_gallery_interactive, rows};

    #[test]
    fn gallery_committed_scene_routes_exact_pointer_coordinates() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, WIDTH, HEIGHT));
        let scene = {
            let mut frame = Frame::new(&mut buffer);
            render_gallery_interactive(&mut frame, None);
            frame.hits().clone()
        };
        let mut router = InteractionRouter::new();
        router.commit_scene(scene, None);

        let outside = router.route(bmux_tui::event::Event::Mouse(
            bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Move,
                bmux_tui::geometry::Point::new(0, 1),
            ),
        ));
        assert!(outside.target.is_none());

        let inside = router.route(bmux_tui::event::Event::Mouse(
            bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Move,
                bmux_tui::geometry::Point::new(3, 1),
            ),
        ));
        assert_eq!(
            inside.target.as_ref().map(bmux_tui::hit::HitId::as_str),
            Some("gallery.save")
        );
        assert_eq!(inside.bounds, Some(Rect::new(1, 1, 10, 1)));
        assert_eq!(
            inside.local_position,
            Some(bmux_tui::geometry::Point::new(2, 0))
        );
    }

    #[test]
    fn gallery_committed_scene_supports_forward_and_reverse_traversal() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, WIDTH, HEIGHT));
        let scene = {
            let mut frame = Frame::new(&mut buffer);
            render_gallery_interactive(&mut frame, None);
            frame.hits().clone()
        };
        let targets = scene.focus_targets(None);
        assert!(targets.len() >= 4);
        assert_eq!(targets[0].as_str(), "gallery.save");
        assert_eq!(targets[1].as_str(), "gallery.actions.accept");
        assert_eq!(targets[2].as_str(), "gallery.actions.cancel");
        assert!(!targets.iter().any(|id| id.as_str() == "gallery.disabled"));

        let mut router = InteractionRouter::new();
        router.commit_scene(scene, None);
        assert_eq!(
            router.focused().map(bmux_tui::hit::HitId::as_str),
            Some("gallery.save")
        );
        let forward = router.route(bmux_tui::event::Event::Key(
            bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Tab),
        ));
        assert!(forward.traversal_consumed);
        assert_eq!(
            forward
                .focus_changed
                .as_ref()
                .map(bmux_tui::hit::HitId::as_str),
            Some("gallery.actions.accept")
        );
        let reverse = router.route(bmux_tui::event::Event::Key(bmux_keyboard::KeyStroke {
            key: bmux_keyboard::KeyCode::Tab,
            modifiers: bmux_keyboard::Modifiers {
                shift: true,
                ..bmux_keyboard::Modifiers::NONE
            },
        }));
        assert!(reverse.traversal_consumed);
        assert_eq!(
            reverse
                .focus_changed
                .as_ref()
                .map(bmux_tui::hit::HitId::as_str),
            Some("gallery.save")
        );
    }

    #[test]
    fn gallery_badges_publish_stable_canonical_semantics() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, WIDTH, HEIGHT));
        let semantics = {
            let mut frame = Frame::new(&mut buffer);
            render_gallery_interactive(&mut frame, None);
            frame.semantics().regions().to_vec()
        };

        for (id, area) in [
            ("gallery.badge.info", Rect::new(1, 3, 10, 1)),
            ("gallery.badge.ok", Rect::new(12, 3, 8, 1)),
            ("gallery.badge.warn", Rect::new(21, 3, 10, 1)),
            ("gallery.badge.truncated", Rect::new(53, 24, 12, 1)),
        ] {
            let region = semantics.iter().find(|region| region.id == id).unwrap();
            assert_eq!(region.role, "status");
            assert_eq!(region.area, area);
        }
    }

    #[test]
    fn gallery_renders_representative_components() {
        let rendered = rows(&render_gallery()).join("\n");

        assert!(rendered.contains("[ Save ]"));
        assert!(rendered.contains("‹ INFO ›"));
        assert!(rendered.contains("[ ok ]"));
        assert!(rendered.contains("LabeledDetails"));
        assert!(rendered.contains("Pane content area"));
        assert!(rendered.contains("70% indexed"));
        assert!(rendered.contains("▁▂▂▃▅█"));
        assert!(rendered.contains("Empty State"));
        assert!(rendered.contains("No matching components yet"));
        assert!(rendered.contains("✓ Plan ── ● Build"));
        assert!(rendered.contains("CPU"));
        assert!(rendered.contains("◆"));
        assert!(
            rendered
                .chars()
                .any(|ch| ('\u{2801}'..='\u{28ff}').contains(&ch))
        );
        assert!(rendered.contains("Saved ×"));
        assert!(rendered.contains("Changes persisted"));
        assert!(rendered.contains("Command Palette"));
        assert!(rendered.contains("Commands"));
        assert!(rendered.contains("Styled"));
        assert!(rendered.contains("Rich"));
        assert!(rendered.contains("truncated"));
        assert!(rendered.contains("Dialog body with actions"));
    }
}
