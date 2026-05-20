use bmux_text_edit::TextEditBuffer;
use bmux_tui::prelude::*;

#[test]
fn command_palette_like_surface_renders_and_flushes_to_ansi() {
    let input = TextEditBuffer::from_text("op");
    let items = vec![
        ListItem::new(Line::from_spans(vec![Span::styled(
            "open file",
            Style::new().fg(Color::Green),
        )])),
        ListItem::new("open recent"),
        ListItem::new("open settings"),
    ];
    let mut state = ListState {
        selected: Some(1),
        offset: 0,
    };
    let mut buffer = Buffer::empty(Rect::new(0, 0, 24, 8));
    let mut frame = Frame::new(&mut buffer);
    let picker = ListPicker::new(&input, &items)
        .panel(
            Panel::new()
                .border(Border::rounded())
                .title("Command")
                .background(Style::new().bg(Color::Black)),
        )
        .input(TextInput::new(&input).placeholder("Search"))
        .list(
            List::new(&items)
                .highlight_symbol("> ")
                .selected_style(Style::new().bg(Color::Blue).fg(Color::BrightWhite)),
        );
    let modal: Modal<'_, TextBlock> = Modal::new(Size::new(20, 6))
        .panel(Panel::new().background(Style::new().bg(Color::BrightBlack)));
    let root = frame.area();

    modal.render(root, &mut frame);
    picker.render(modal.content_area(root), &mut frame, &mut state);

    assert_eq!(
        frame.buffer().row_symbols(1).as_deref(),
        Some("  ╭Command───────────╮  ")
    );
    assert_eq!(
        frame.buffer().row_symbols(2).as_deref(),
        Some("  │op                │  ")
    );
    assert_eq!(
        frame.buffer().row_symbols(4).as_deref(),
        Some("  │open file         │  ")
    );
    assert_eq!(
        frame.buffer().row_symbols(5).as_deref(),
        Some("  │> open recent     │  ")
    );

    let mut out = Vec::new();
    write_ansi_frame(&mut out, frame.buffer(), frame.cursor()).unwrap();
    let rendered = String::from_utf8(out).unwrap();

    assert!(rendered.contains("Command"));
    assert!(rendered.contains("> open recent"));
    assert!(rendered.ends_with("\x1b[?25h"));
}
