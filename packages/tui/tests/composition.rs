use bmux_tui::component::Component;
use bmux_tui::prelude::*;

#[test]
fn message_card_sketch_uses_only_canonical_composition() {
    let card = Surface::new(
        Column::new()
            .gap(1)
            .child(
                Row::new()
                    .gap(1)
                    .child(TextContent::new("Ada").style(Style::new().add_modifier(Modifier::BOLD)))
                    .flex(Flex::new(
                        1,
                        TextContent::new("10:42").alignment(Alignment::Right),
                    )),
            )
            .child(TextContent::new(
                "A variable-height message wraps without precomputing its child height.",
            )),
    )
    .id("message:42")
    .background(Style::new().bg(Color::Blue))
    .padding(Insets::new(1, 1, 1, 1));

    let layout = card.layout(Constraints::for_width(24), &mut LayoutCx::new());
    assert_eq!(layout.size.height, 8);

    let mut buffer = Buffer::empty(Rect::new(0, 0, 24, 8));
    let mut frame = Frame::new(&mut buffer);
    card.paint(&layout, &mut PaintCx::new(&mut frame));

    assert_eq!(
        frame.buffer().row_symbols(0).as_deref(),
        Some("                        ")
    );
    assert_eq!(
        frame.buffer().row_symbols(1).as_deref(),
        Some(" Ada              10:42 ")
    );
    assert_eq!(
        frame.buffer().row_symbols(3).as_deref(),
        Some(" A variable-height      ")
    );
    assert_eq!(
        frame
            .buffer()
            .get(Point::new(23, 7))
            .and_then(|cell| cell.style.bg),
        Some(Color::Blue)
    );
}
