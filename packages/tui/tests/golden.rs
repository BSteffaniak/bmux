use bmux_tui::prelude::*;
use std::cell::Cell;

fn buffer_rows(buffer: &Buffer) -> Vec<String> {
    (buffer.area().y..buffer.area().bottom())
        .filter_map(|row| buffer.row_symbols(row))
        .collect()
}

#[test]
fn golden_panel_text_and_dialog_rendering() {
    let actions = vec![
        DialogAction::new("yes", "Yes"),
        DialogAction::new("no", "No"),
    ];
    let state = Cell::new(DialogState { focused_action: 0 });
    let mut buffer = Buffer::empty(Rect::new(0, 0, 24, 6));
    let mut frame = Frame::new(&mut buffer);
    let dialog = DialogComponent::new(
        "confirm-dialog",
        Dialog::new("Run this command?", &actions)
            .panel(Panel::new().border(Border::ascii()).title("Confirm")),
        &state,
    );
    let layout = dialog.layout(
        Constraints::tight(frame.area().size()),
        &mut LayoutCx::new(),
    );
    dialog.paint(&layout, &mut PaintCx::new(&mut frame));

    assert_eq!(
        buffer_rows(frame.buffer()),
        vec![
            "+Confirm---------------+",
            "|Run this command?     |",
            "|                      |",
            "|                      |",
            "|[ Yes ] [ No ]        |",
            "+----------------------+",
        ]
    );
}
