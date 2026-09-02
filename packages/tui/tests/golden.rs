use bmux_text_edit::TextEditBuffer;
use bmux_tui::prelude::*;

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
    let mut state = DialogState { focused_action: 0 };
    let mut buffer = Buffer::empty(Rect::new(0, 0, 24, 6));
    let mut frame = Frame::new(&mut buffer);

    Dialog::new("Run this command?", &actions)
        .panel(Panel::new().border(Border::ascii()).title("Confirm"))
        .paint_in(Rect::new(0, 0, 24, 6), &mut frame, &mut state);

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

#[test]
fn golden_list_picker_rendering() {
    let input = TextEditBuffer::from_text("op");
    let items = vec![
        ListItem::new("open file"),
        ListItem::new("open recent"),
        ListItem::new("open settings"),
    ];
    let mut state = ListState {
        selected: Some(1),
        offset: 0,
    };
    let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 6));
    let mut frame = Frame::new(&mut buffer);

    ListPicker::new(&input, &items)
        .panel(Panel::new().border(Border::rounded()).title("Command"))
        .list(List::new(&items).highlight_symbol("> "))
        .paint_in(Rect::new(0, 0, 20, 6), &mut frame, &mut state);

    assert_eq!(
        buffer_rows(frame.buffer()),
        vec![
            "╭Command───────────╮",
            "│op                │",
            "│                  │",
            "│open file         │",
            "│> open recent     │",
            "╰──────────────────╯",
        ]
    );
}

#[test]
fn golden_dropdown_overlay_rendering() {
    let base = TextBlock::new("base surface").id("base");
    let items = vec![ListItem::new("alpha"), ListItem::new("beta")];
    let dropdown = Dropdown::new(&items)
        .panel(Panel::new().border(Border::ascii()))
        .max_height(4);
    let mut list_state = ListState {
        selected: Some(0),
        offset: 0,
    };
    let mut buffer = Buffer::empty(Rect::new(0, 0, 16, 4));
    let mut frame = Frame::new(&mut buffer);

    let layout = base.layout(
        Constraints::tight(Rect::new(0, 0, 16, 1).size()),
        &mut LayoutCx::new(),
    );
    base.paint(&layout, &mut PaintCx::new(&mut frame));
    dropdown.paint_in(Rect::new(4, 1, 8, 3), &mut frame, &mut list_state);

    assert_eq!(
        buffer_rows(frame.buffer()),
        vec![
            "base surface    ",
            "    +------+    ",
            "    |alpha |    ",
            "    +------+    ",
        ]
    );
}
