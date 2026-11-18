//! Editor replay captured with registry unicode-segmentation 1.13.3.
use bmux_text_edit::{SelectionMode, TextEditBuffer, TextMotion};
use std::fmt::Write;

#[test]
fn edit_at_wrapped_margin() {
    let mut out = String::new();
    for (prefix, cluster) in [
        ("abc", "e\u{301}"),
        ("ab", "🇨🇦"),
        ("abc", "क्ष"),
        ("ab", "👩🏽‍💻"),
    ] {
        let mut edit = TextEditBuffer::from_text(format!("{prefix}{cluster}!"));
        let layout = edit.wrapped_layout(4);
        writeln!(
            out,
            "lines={:?} ranges={:?}",
            layout.lines, layout.line_ranges
        )
        .unwrap();
        for row in 0..layout.lines.len() {
            let hits: Vec<_> = (0..=4)
                .map(|col| edit.byte_index_for_wrapped_position(4, row, col))
                .collect();
            writeln!(out, "hits={hits:?}").unwrap();
        }
        edit.move_cursor_to_wrapped_position(4, 0, prefix.len());
        let mut forward = edit.clone();
        forward.delete_forward();
        writeln!(
            out,
            "forward={:?}@{}",
            forward.text(),
            forward.cursor_byte_index()
        )
        .unwrap();
        edit.move_cursor_with_selection(TextMotion::Right, SelectionMode::Extend);
        let pos = edit.wrapped_layout(4).cursor;
        writeln!(
            out,
            "cursor={} grapheme={} visual={},{} selection={:?} copy={:?}",
            edit.cursor_byte_index(),
            edit.cursor_grapheme_index(),
            pos.row,
            pos.col,
            edit.selection(),
            edit.selected_text()
        )
        .unwrap();
        let mut backward = edit.clone();
        backward.clear_selection();
        backward.delete_backward();
        writeln!(
            out,
            "backward={:?}@{}",
            backward.text(),
            backward.cursor_byte_index()
        )
        .unwrap();
        assert!(edit.delete_selection());
        writeln!(
            out,
            "delete selection={:?}@{}",
            edit.text(),
            edit.cursor_byte_index()
        )
        .unwrap();
    }
    assert_eq!(out, include_str!("fixtures/grapheme_replay.txt"));
}
