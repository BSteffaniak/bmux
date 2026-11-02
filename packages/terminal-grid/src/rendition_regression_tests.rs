use crate::{Color, GridLimits, Style, TerminalGridStream};

fn style_at(stream: &TerminalGridStream, col: usize) -> Style {
    let rows = stream.grid().viewport_rows();
    stream.grid().palette().get(rows[0].cells()[col].style())
}

#[test]
fn rendition_operations_are_atomic_across_every_chunk_boundary() {
    let cases: &[(&[u8], Style)] = &[
        (
            b"\x1b[1;31m\x1b[4:0mX",
            Style {
                bold: true,
                fg: Some(Color::Indexed(1)),
                ..Style::default()
            },
        ),
        (
            b"\x1b[4:3mX",
            Style {
                underline: true,
                ..Style::default()
            },
        ),
        (b"\x1b[58;2;1;4;9mX", Style::default()),
        (b"\x1b[58:2::1:4:9mX", Style::default()),
        (
            b"\x1b[38:2::255:0:0mX",
            Style {
                fg: Some(Color::Rgb { r: 255, g: 0, b: 0 }),
                ..Style::default()
            },
        ),
        (
            b"\x1b[48:2:1:4:9mX",
            Style {
                bg: Some(Color::Rgb { r: 1, g: 4, b: 9 }),
                ..Style::default()
            },
        ),
        (b"\x1b[38;2;999;4;9mX", Style::default()),
        (
            b"\x1b[31m\x1b[38:2:3:4mX",
            Style {
                fg: Some(Color::Indexed(1)),
                ..Style::default()
            },
        ),
        (b"\x1b[38;2;1;4mX", Style::default()),
        (b"\x1b[>4;2mX", Style::default()),
        (b"\x1b[?4mX", Style::default()),
        (b"\x1b[1 qX", Style::default()),
        (
            b"\x1b[8mX",
            Style {
                hidden: true,
                ..Style::default()
            },
        ),
        (b"\x1b[8m\x1b[28mX", Style::default()),
        (b"\x1b[1m\x1b[mX", Style::default()),
        (
            b"\x1b[58;5;4;31mX",
            Style {
                fg: Some(Color::Indexed(1)),
                ..Style::default()
            },
        ),
    ];
    for &(bytes, expected) in cases {
        for split in 0..=bytes.len() {
            let mut stream = TerminalGridStream::new(20, 2, GridLimits::default()).unwrap();
            stream.process(&bytes[..split]);
            let snapshot = stream.snapshot(0, 2);
            let mut hydrated =
                TerminalGridStream::from_snapshot(&snapshot, GridLimits::default()).unwrap();
            stream.process(&bytes[split..]);
            hydrated.process(&bytes[split..]);
            assert_eq!(
                style_at(&stream, 0),
                expected,
                "bytes={bytes:?}, split={split}"
            );
            assert_eq!(
                style_at(&hydrated, 0),
                expected,
                "hydrated bytes={bytes:?}, split={split}"
            );
        }
    }
}

#[test]
fn omitted_scroll_region_bottom_defaults_to_last_row() {
    for reset in [b"\x1b[r".as_slice(), b"\x1b[0;0r", b"\x1b[1r", b"\x1b[1;0r"] {
        let mut stream = TerminalGridStream::new(12, 4, GridLimits::default()).unwrap();
        stream.process(b"\x1b[2;3r");
        stream.process(reset);
        assert_eq!(stream.grid().scroll_region(), Some((0, 3)), "{reset:?}");
    }
}

#[test]
fn private_and_subparameter_cursor_commands_do_not_mutate_the_screen() {
    let mut stream = TerminalGridStream::new(20, 2, GridLimits::default()).unwrap();
    stream.process(b"abc\x1b[?2J\x1b[>2J\x1b[1:2HX");
    assert!(crate::visible_text_trimmed(stream.grid(), 0, 2).starts_with("abcX"));
}
