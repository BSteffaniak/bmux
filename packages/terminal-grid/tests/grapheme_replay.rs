//! Product observations captured with registry unicode-segmentation 1.13.3.
use bmux_terminal_grid::{GridLimits, TerminalGridStream};
use std::fmt::Write;

const CASES: &[(&str, &str)] = &[
    ("accent", "abce\u{301}"),
    ("flag", "ab🇨🇦"),
    ("indic", "abcक्ष"),
    ("emoji", "ab👩🏽‍💻"),
    ("style between indicators", "ab🇨\x1b[31m🇦"),
    ("cursor between indicators", "ab🇨\x1b[H🇦"),
];

// Deliberately exclude revision counters: the number of process calls is not
// a visible difference. Include continuation cells rather than just row text.
fn observe(stream: &TerminalGridStream) -> String {
    let snap = stream.snapshot(0, 3);
    let mut out = format!(
        "cursor={},{} pending={}\n",
        snap.cursor.row, snap.cursor.col, snap.pending_wrap
    );
    for row in stream.grid().viewport_rows() {
        write!(out, "  wrap={}", row.wrapped()).unwrap();
        for cell in row.cells() {
            write!(
                out,
                " |{}:{}{}",
                cell.text().escape_default(),
                cell.width(),
                if cell.is_wide_continuation() { "+" } else { "" }
            )
            .unwrap();
            let style = snap.styles[cell.style().0 as usize];
            if style != Default::default() {
                write!(out, " fg={:?}", style.fg).unwrap();
            }
        }
        out.push('\n');
    }
    out
}

fn replay<'a>(chunks: impl Iterator<Item = &'a [u8]>) -> String {
    let mut stream = TerminalGridStream::new(4, 3, GridLimits::default()).unwrap();
    for chunk in chunks {
        stream.process(chunk);
    }
    let mut out = observe(&stream);
    stream.process(b"!");
    out += &observe(&stream);
    stream.resize(3, 3).unwrap();
    out += &observe(&stream);
    let mut restored =
        TerminalGridStream::from_snapshot(&stream.snapshot(0, 3), GridLimits::default()).unwrap();
    assert_eq!(observe(&restored), observe(&stream));
    for s in [&mut stream, &mut restored] {
        s.resize(6, 3).unwrap();
        s.process(b"?");
    }
    assert_eq!(observe(&restored), observe(&stream));
    out + &observe(&stream)
}

#[test]
fn right_margin_stream_replay() {
    let mut baseline = String::new();
    for &(name, input) in CASES {
        let bytes = input.as_bytes();
        let whole = replay(std::iter::once(bytes));
        for i in 0..=bytes.len() {
            assert_eq!(
                replay([&bytes[..i], &bytes[i..]].into_iter()),
                whole,
                "{name} split {i}"
            );
        }
        for size in [1, 2, 3, 5, 7] {
            assert_eq!(replay(bytes.chunks(size)), whole, "{name} chunks {size}");
        }
        writeln!(baseline, "{name}").unwrap();
        baseline += &whole;
    }
    assert_eq!(baseline, include_str!("fixtures/grapheme_replay.txt"));
}
