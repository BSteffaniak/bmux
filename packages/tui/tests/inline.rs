use bmux_tui::{
    ansi::{write_ansi_inline_frame, write_ansi_inline_frame_diff},
    buffer::Buffer,
    geometry::{Point, Rect},
    style::{Color, Modifier, Style},
};

#[test]
fn unchanged_inline_frame_emits_nothing() {
    let buffer = Buffer::empty(Rect::new(0, 0, 20, 4));
    let mut output = Vec::new();
    write_ansi_inline_frame_diff(&mut output, &buffer, &buffer).unwrap();
    assert!(output.is_empty());
}

#[test]
fn inline_changes_do_not_clear_or_scroll() {
    let previous = Buffer::empty(Rect::new(0, 0, 20, 4));
    let mut current = previous.clone();
    current
        .get_mut(Point::new(3, 2))
        .unwrap()
        .set("x", Style::new().fg(Color::Green));
    let mut output = Vec::new();
    write_ansi_inline_frame_diff(&mut output, &previous, &current).unwrap();
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("\x1b[2A"));
    assert!(text.contains("\x1b[3C"));
    assert!(text.contains("\x1b[2B"));
    assert!(!text.contains('\n'));
    assert!(!text.contains("2K"));
    assert!(!text.contains("?1049"));
    assert_eq!(text.matches('x').count(), 1);
}

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
