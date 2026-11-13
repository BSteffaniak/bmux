//! A bounded grid collecting the text of history it drops gives a complete plain-text log.

use bmux_terminal_grid::{GridLimits, TerminalGridStream, content_text};

fn stream(cols: u16, rows: u16, scrollback: usize) -> TerminalGridStream {
    TerminalGridStream::new(
        cols,
        rows,
        GridLimits {
            scrollback_rows: scrollback,
        },
    )
    .unwrap()
}

/// Everything `bytes` showed, from a grid keeping `scrollback` rows: what it dropped, then
/// what remains, fed in `chunk`-byte pieces.
fn log(bytes: &[u8], scrollback: usize, chunk: usize) -> String {
    let mut stream = stream(10, 3, scrollback);
    stream.grid_mut().collect_evicted_text(true);
    let mut text = String::new();
    for piece in bytes.chunks(chunk) {
        stream.process(piece);
        text.push_str(&stream.grid_mut().take_evicted_text());
    }
    text.push_str(&content_text(stream.grid()));
    text
}

#[test]
fn dropped_history_and_what_remains_make_the_whole_log() {
    let mut bytes = Vec::new();
    for n in 1..=200 {
        // Some lines soft-wrap at ten columns; some redraw in place.
        bytes.extend(format!("line {n} {}\r\n", "x".repeat(n % 17)).as_bytes());
        if n % 25 == 0 {
            bytes.extend(b"progress 10%\rprogress 100%\r\n");
        }
    }
    let unbounded = log(&bytes, 100_000, bytes.len());
    for scrollback in [0, 1, 5, 40] {
        for chunk in [1, 7, 4096] {
            assert_eq!(
                log(&bytes, scrollback, chunk),
                unbounded,
                "{scrollback} rows of history in {chunk}-byte chunks"
            );
        }
    }
    // Wrapped lines read as the lines the program printed.
    assert!(unbounded.contains("line 16 xxxxxxxxxxxxxxxx\n"));
    assert!(unbounded.contains("progress 100%\n"));
    assert!(!unbounded.contains("progress 10%"));
}

#[test]
fn erasing_scrollback_or_resetting_hands_over_history() {
    for clear in [&b"\x1b[3J"[..], b"\x1bc"] {
        let mut stream = stream(10, 2, 100);
        stream.grid_mut().collect_evicted_text(true);
        stream.process(b"one\r\ntwo\r\nthree\r\nfour");
        stream.process(clear);
        let dropped = stream.grid_mut().take_evicted_text();
        assert_eq!(dropped, "one\ntwo\n", "{clear:?}");
    }
}

#[test]
fn nothing_is_collected_unless_asked() {
    let mut stream = stream(10, 2, 0);
    stream.process(b"one\r\ntwo\r\nthree\r\n");
    assert_eq!(stream.grid_mut().take_evicted_text(), "");
}

#[test]
fn spaces_at_a_soft_wrap_are_kept() {
    for scrollback in [0, 100] {
        let mut stream = stream(4, 3, scrollback);
        stream.grid_mut().collect_evicted_text(true);
        stream.process(b"abc def ghi\r\nnext\r\nmore");
        let mut text = stream.grid_mut().take_evicted_text();
        text.push_str(&content_text(stream.grid()));
        assert_eq!(
            text, "abc def ghi\nnext\nmore",
            "{scrollback} rows of history"
        );
    }
    // On the screen too.
    let mut stream = stream(4, 5, 100);
    stream.process(b"abc def");
    assert_eq!(content_text(stream.grid()), "abc def");
}

#[test]
fn an_unfinished_line_leaving_the_screen_is_handed_over_as_it_goes() {
    let mut stream = stream(10, 3, 0);
    stream.grid_mut().collect_evicted_text(true);
    // Fifty rows of one line: everything above the screen is handed over unfinished.
    stream.process(&[b'x'; 500]);
    let taken = stream.grid_mut().take_evicted_text();
    assert_eq!(taken, "x".repeat(470));
    // The screen holds the rest; together they are the line.
    assert_eq!(
        format!("{taken}{}", content_text(stream.grid())),
        "x".repeat(500)
    );
    // Finishing it hands over the rest as the line it is.
    stream.process(b"\r\nnext\r\n\r\n\r\n");
    assert_eq!(
        stream.grid_mut().take_evicted_text(),
        "x".repeat(30) + "\nnext\n"
    );
}
