use bmux_tui::{ansi::AnsiLog, geometry::Size, inline_output::InlineOutput, text::Line};
use std::{
    io,
    time::{Duration, Instant},
};

#[test]
fn every_byte_boundary_preserves_utf8_styles_and_controls() {
    let input = "\x1b[38;2;10;20;30m日本 é👩‍💻\x1b[0m\nlong progress\rOK\x1b[K\n\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x07";
    let mut whole = AnsiLog::new(20, 200);
    whole.push(input.as_bytes());
    whole.finish();
    for split in 0..=input.len() {
        let mut log = AnsiLog::new(20, 200);
        log.push(&input.as_bytes()[..split]);
        log.push(&input.as_bytes()[split..]);
        log.finish();
        assert_eq!(log.lines(), whole.lines(), "split {split}");
    }
    let mut bytewise = AnsiLog::new(20, 200);
    let first = &whole.lines()[0].spans[0];
    assert_eq!(
        first.style.fg,
        Some(bmux_tui::style::Color::Rgb(10, 20, 30))
    );
    for byte in input.as_bytes() {
        bytewise.push(&[*byte]);
    }
    assert_eq!(bytewise.lines(), whole.lines());
    let text: Vec<_> = whole.lines().iter().map(Line::plain_text).collect();
    assert_eq!(text, ["日本 é👩‍💻", "OK", "link"]);
}

#[test]
fn retention_invalid_utf8_and_unterminated_controls_are_bounded() {
    let mut log = AnsiLog::new(2, 4);
    log.push(b"old\n123456789\nnext\n\xff\xe2");
    log.finish();
    assert_eq!(
        log.lines().iter().map(Line::plain_text).collect::<Vec<_>>(),
        ["next", "��"]
    );
    log.push(b"\x1b]");
    log.push(&vec![b'x'; 100_000]);
    log.push(b"\x1b\\Z");
    assert_eq!(log.lines()[1].plain_text(), "��Z");
    log.push(b"\x1b[");
    log.push(&vec![b'1'; 100_000]);
    log.push(b"mQ");
    assert_eq!(log.lines()[1].plain_text(), "��ZQ");
}

#[test]
fn carriage_returns_overwrite_without_erasing_suffix_until_requested() {
    let mut log = AnsiLog::new(4, 20);
    log.push(b"abcdef\rxy");
    assert_eq!(log.lines()[0].plain_text(), "xycdef");
    log.push(b"\x1b[K");
    assert_eq!(log.lines()[0].plain_text(), "xy");
    log.push(b"\x08Z\nnext");
    assert_eq!(log.lines()[0].plain_text(), "xZ");
}

#[test]
fn unicode_tail_is_wrapped_and_bounded() {
    let mut log = AnsiLog::new(5, 50);
    log.push("ancient\n日本語\nlast".as_bytes());
    let tail = log.tail(4, 2);
    assert_eq!(tail.len(), 2);
    assert_eq!(tail[0].plain_text(), "語");
    assert_eq!(tail[1].plain_text(), "last");
    assert!(log.tail(0, 2).is_empty());
}

#[test]
fn presenter_diffs_throttles_clears_and_preserves_commits() {
    let mut bytes = Vec::new();
    let now = Instant::now();
    let size = Size::new(40, 10);
    let mut output = InlineOutput::new(&mut bytes, Duration::from_millis(50));
    assert!(
        output
            .draw(&[Line::raw("working")], size, now, false)
            .unwrap()
    );
    assert!(
        !output
            .draw(&[Line::raw("later")], size, now, false)
            .unwrap()
    );
    output.commit(&[Line::raw("FAILED first")], size).unwrap();
    output
        .draw(&[Line::raw("second")], size, now, true)
        .unwrap();
    output.clear(size).unwrap();
    let terminal = String::from_utf8(bytes).unwrap();
    assert!(terminal.contains("FAILED first"));
    assert!(!terminal.contains("later"));
    assert!(!terminal.contains("?1049"));
    assert!(!terminal.contains("?25"));
}

#[test]
fn unchanged_frames_emit_nothing_and_resize_never_uses_stale_anchor() {
    let now = Instant::now();
    let mut bytes = Vec::new();
    let mut output = InlineOutput::new(&mut bytes, Duration::ZERO);
    output
        .draw(&[Line::raw("x")], Size::new(40, 10), now, true)
        .unwrap();
    output
        .draw(&[Line::raw("x")], Size::new(40, 10), now, true)
        .unwrap();
    output
        .draw(&[Line::raw("y")], Size::new(20, 5), now, true)
        .unwrap();
    output.retain();
    let terminal = String::from_utf8(bytes).unwrap();
    assert!(
        !terminal.contains("A"),
        "no upward cursor move on resize: {terminal:?}"
    );
    assert_eq!(terminal.matches('x').count(), 1);
}

#[test]
fn presenter_rejects_further_cursor_operations_after_partial_write() {
    struct Broken;
    impl io::Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("broken"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut output = InlineOutput::new(Broken, Duration::ZERO);
    let size = Size::new(40, 10);
    assert!(
        output
            .draw(&[Line::raw("x")], size, Instant::now(), true)
            .is_err()
    );
    assert!(output.clear(size).is_err());
}
