use bmux_tui::{
    ansi::write_ansi_inline_frame,
    buffer::Buffer,
    geometry::{Point, Rect},
    style::{Color, Modifier, Style},
};

#[test]
fn inline_encoding_preserves_styles_without_absolute_cursor_or_alternate_screen() {
    let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 1));
    buffer.get_mut(Point::new(0, 0)).unwrap().set(
        "x",
        Style::new()
            .fg(Color::Rgb(12, 34, 56))
            .add_modifier(Modifier::ITALIC),
    );
    let mut output = Vec::new();
    write_ansi_inline_frame(&mut output, &buffer).unwrap();
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("38;2;12;34;56"));
    assert!(text.contains('x'));
    assert!(text.contains("\r\n"));
    assert!(!text.contains("?1049"));
    assert!(!text.contains("1;1H"));
}
