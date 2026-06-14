use bmux_text_edit::TextEditBuffer;
use bmux_tui::buffer::Buffer;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::prelude::{Clear, Line};
use bmux_tui::style::{Color, Style};
use bmux_tui::widget::Widget;
use bmux_tui_components::action_row::{ActionButton, ActionRow, ActionRowState};
use bmux_tui_components::badge::{Badge, BadgePolicy, BadgeSeverity};
use bmux_tui_components::bar_chart::{BarChart, BarChartItem, BarChartPolicy};
use bmux_tui_components::button::{Button, ButtonState};
use bmux_tui_components::dialog::{Dialog, DialogState};
use bmux_tui_components::empty_state::{EmptyState, EmptyStatePolicy};
use bmux_tui_components::filtered_list::FilteredListState;
use bmux_tui_components::form_field::FormField;
use bmux_tui_components::labeled_details::{DetailItem, LabeledDetails};
use bmux_tui_components::modal_frame::{ModalFrame, ModalPlacement, ModalSizing, ModalTheme};
use bmux_tui_components::pane::{Pane, PaneState};
use bmux_tui_components::picker_frame::{PickerFrame, PickerFramePolicy};
use bmux_tui_components::progress_bar::{
    ProgressBar, ProgressBarPolicy, ProgressBarValue, ProgressLabelPlacement,
};
use bmux_tui_components::selectable_list::{
    SelectableList, SelectableListItem, SelectableListState,
};
use bmux_tui_components::sparkline::{Sparkline, SparklinePolicy};
use bmux_tui_components::stepper::{StepItem, StepStatus, Stepper, StepperPolicy};
use bmux_tui_components::text_input::{TextInputPolicy, TextInputState};
use bmux_tui_components::text_input_box::{TextInputBox, TextInputBoxPolicy};
use bmux_tui_components::toast_stack::{ToastItem, ToastSeverity, ToastStack, ToastStackState};

pub const WIDTH: u16 = 72;
pub const HEIGHT: u16 = 24;

pub fn render_gallery() -> Buffer {
    let mut buffer = Buffer::empty(Rect::new(0, 0, WIDTH, HEIGHT));
    let mut frame = Frame::new(&mut buffer);
    render_gallery_into(&mut frame);
    buffer
}

pub fn render_gallery_into(frame: &mut Frame<'_>) {
    let theme = ModalTheme::dark(Color::Cyan);

    render_buttons(frame);
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
}

fn render_buttons(frame: &mut Frame<'_>) {
    let mut focused = ButtonState::new();
    focused.interaction.focused = true;
    Button::new("Save").render(Rect::new(1, 1, 10, 1), &focused, frame);

    let mut disabled = ButtonState::new();
    disabled.interaction.disabled = true;
    Button::new("Disabled").render(Rect::new(13, 1, 14, 1), &disabled, frame);

    let actions = [
        ActionButton::new("accept", "Accept"),
        ActionButton::new("cancel", "Cancel"),
    ];
    let mut state = ActionRowState::new();
    state.set_focused(Some(0));
    ActionRow::new(&actions).render_state(Rect::new(1, 3, 30, 1), &state, frame);
}

fn render_badges(frame: &mut Frame<'_>) {
    Badge::new("info")
        .severity(BadgeSeverity::Info)
        .policy(BadgePolicy::pill().uppercase(true))
        .render(Rect::new(1, 3, 10, 1), frame);
    Badge::new("ok")
        .severity(BadgeSeverity::Success)
        .render(Rect::new(12, 3, 8, 1), frame);
    Badge::new("warn")
        .severity(BadgeSeverity::Warning)
        .render(Rect::new(21, 3, 10, 1), frame);
}

fn render_details(frame: &mut Frame<'_>) {
    let items = [
        DetailItem::new("Component", "LabeledDetails"),
        DetailItem::new("Purpose", "Wrapped labels and values"),
    ];
    LabeledDetails::new(&items).render(Rect::new(1, 5, 34, 5), frame);
}

fn render_field(frame: &mut Frame<'_>) {
    let field = FormField::new("Project")
        .required(true)
        .help("Shown with a required marker")
        .error("Example error text");
    let control_area = field.render(Rect::new(38, 1, 30, 5), frame);
    frame.write_line(control_area, &Line::from("bmux"));
}

fn render_pane(frame: &mut Frame<'_>) {
    let pane = Pane::new().title("Pane").padding(Insets::all(1));
    let state = PaneState::new(Rect::new(1, 11, 28, 8));
    pane.render(&state, frame);
    frame.write_line(pane.inner_area(&state), &Line::from("Pane content area"));
}

fn render_progress(frame: &mut Frame<'_>) {
    ProgressBar::new(ProgressBarValue::determinate(7, 10))
        .label("70% indexed")
        .policy(
            ProgressBarPolicy::compact()
                .background(true)
                .label(ProgressLabelPlacement::Right),
        )
        .render(Rect::new(1, 20, 28, 1), frame);
    ProgressBar::new(ProgressBarValue::indeterminate(4))
        .policy(ProgressBarPolicy::bare())
        .render(Rect::new(1, 22, 28, 1), frame);
    let samples = [1, 2, 3, 5, 8, 13, 8, 5, 3, 2, 1];
    Sparkline::new(&samples)
        .policy(SparklinePolicy::bare().max(Some(13)))
        .render(Rect::new(1, 23, 28, 1), frame);
}

fn render_empty_state(frame: &mut Frame<'_>) {
    let body = [Line::from("No matching components yet")];
    let actions = [Line::from("Press / to filter")];
    EmptyState::new("Empty State")
        .icon("∅")
        .body(&body)
        .actions(&actions)
        .policy(EmptyStatePolicy::centered())
        .render(Rect::new(2, 14, 26, 4), frame);
}

fn render_stepper(frame: &mut Frame<'_>) {
    let steps = [
        StepItem::new("plan", "Plan").status(StepStatus::Complete),
        StepItem::new("build", "Build").status(StepStatus::Current),
        StepItem::new("ship", "Ship"),
    ];
    Stepper::new(&steps)
        .policy(StepperPolicy::horizontal())
        .render(Rect::new(35, 12, 33, 1), frame);
}

fn render_bar_chart(frame: &mut Frame<'_>) {
    let items = [BarChartItem::new("CPU", 7), BarChartItem::new("Mem", 4)];
    BarChart::new(&items)
        .policy(BarChartPolicy::compact().max(Some(10)))
        .render(Rect::new(35, 13, 28, 2), frame);
}

fn render_toasts(frame: &mut Frame<'_>) {
    let toasts = [
        ToastItem::new("saved", "Saved")
            .body("Changes persisted")
            .severity(ToastSeverity::Success),
        ToastItem::new("sync", "Syncing").severity(ToastSeverity::Info),
    ];
    ToastStack::new(&toasts).render(Rect::new(1, 20, 30, 4), &ToastStackState::default(), frame);
}

fn render_picker(frame: &mut Frame<'_>) {
    let picker = PickerFrame::new()
        .title("Command Palette")
        .header("Commands")
        .footer("enter select · esc close")
        .policy(PickerFramePolicy::palette().max_size(Size::new(30, 12)));
    let layout = picker.render(Rect::new(34, 6, 34, 12), frame);

    if let Some(input_area) = layout.input {
        let mut input_state = TextInputState::new(TextEditBuffer::from_text("open"));
        TextInputBox::new(TextInputPolicy::chat_composer())
            .policy(TextInputBoxPolicy::bare().focused(true))
            .render(input_area, &mut input_state, frame);
    }

    let items = [
        SelectableListItem::new("open", "Open file"),
        SelectableListItem::new("switch", "Switch tab"),
        SelectableListItem::new("close", "Close window"),
    ];
    let mut filtered = FilteredListState::from_indices([0, 2]);
    let visible_items = filtered
        .indices()
        .iter()
        .map(|index| items[*index].clone())
        .collect::<Vec<_>>();
    filtered.select_visible(0);
    let list_state = SelectableListState::new(filtered.selected_visible());
    SelectableList::new(&visible_items).render(layout.list, &list_state, frame);
}

fn render_modal(frame: &mut Frame<'_>, theme: ModalTheme) {
    let modal = ModalFrame::new(
        ModalSizing::new(Size::new(24, 7), Size::new(24, 7), Insets::all(0)),
        theme,
    )
    .title("Anchored modal")
    .placement(ModalPlacement::Anchored(bmux_tui::geometry::Point::new(
        36, 18,
    )));
    let modal_area = Rect::new(34, 18, 34, 6);
    Clear::new()
        .style(Style::new().bg(Color::Black))
        .render(modal_area, frame);
    modal.render(modal_area, frame);
    modal.render_line(
        modal.content_area(modal_area),
        &Line::from("Opaque modal frame"),
        frame,
    );
}

fn render_dialog(frame: &mut Frame<'_>, theme: ModalTheme) {
    let body = [Line::from("Dialog body with actions")];
    let actions = [ActionButton::new("ok", "OK")];
    let mut state = DialogState::new();
    state.actions.set_focused(Some(0));
    Dialog::new(&body, &actions, theme)
        .title("Dialog")
        .sizing(ModalSizing::new(
            Size::new(30, 7),
            Size::new(30, 7),
            Insets::all(0),
        ))
        .render(Rect::new(35, 13, 34, 9), &state, frame);
}

pub fn rows(buffer: &Buffer) -> Vec<String> {
    (0..buffer.area().height)
        .filter_map(|row| buffer.row_symbols(row))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{render_gallery, rows};

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
        assert!(rendered.contains("██████"));
        assert!(rendered.contains("Saved ×"));
        assert!(rendered.contains("Changes persisted"));
        assert!(rendered.contains("Command Palette"));
        assert!(rendered.contains("Commands"));
        assert!(rendered.contains("Dialog body with actions"));
    }
}
