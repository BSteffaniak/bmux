//! Frame corpora for pane-output benchmarks (see `docs/development/pane-output-performance.md`).
//!
//! A corpus is the terminal output of an application, one record per frame it presented: an
//! 8-byte little-endian length, then the frame's bytes. It starts from a full repaint, so
//! replaying it from a blank screen reproduces what the application showed. Corpora are captured
//! from real applications (Bcode's review benchmark writes them with `BCODE_BENCH_CAPTURE`) or
//! generated here, deterministically, for the shapes that matter: scrolling with `SU`, scrolling
//! with linefeeds inside a region, full repaints, a plain stream without frames, and a
//! synchronized update that is never closed.

use std::fmt::Write as _;
use std::io;
use std::path::Path;

/// One application frame's terminal output.
pub type Frame = Vec<u8>;

/// Begin a synchronized update (DEC mode 2026).
pub const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
/// End a synchronized update.
pub const SYNC_END: &[u8] = b"\x1b[?2026l";

/// Start of a replay marker: a private OSC naming a frame and when it was written, which a
/// terminal ignores. `bmux-bench replay` puts one in each frame it plays; an attach client run with
/// `BMUX_REPLAY_LOG` logs when the host frame showing it is written, for end-to-end latency.
pub const MARKER_PREFIX: &[u8] = b"\x1b]7771;bmux-replay;";

/// The marker for frame `frame` written at `written_ns` (nanoseconds since the Unix epoch).
#[must_use]
pub fn marker(frame: u64, written_ns: u128) -> Vec<u8> {
    let mut bytes = MARKER_PREFIX.to_vec();
    bytes.extend_from_slice(format!("{frame};{written_ns}\x07").as_bytes());
    bytes
}

/// The complete markers in `bytes`: frame and write time of each, in order.
#[must_use]
pub fn markers(bytes: &[u8]) -> Vec<(u64, u128)> {
    let mut found = Vec::new();
    let mut rest = bytes;
    while let Some(start) = rest
        .windows(MARKER_PREFIX.len())
        .position(|window| window == MARKER_PREFIX)
    {
        rest = &rest[start + MARKER_PREFIX.len()..];
        let Some(end) = rest.iter().position(|byte| *byte == 0x07) else {
            break;
        };
        let fields = std::str::from_utf8(&rest[..end]).ok().and_then(|text| {
            let (frame, written) = text.split_once(';')?;
            Some((frame.parse().ok()?, written.parse().ok()?))
        });
        found.extend(fields);
        rest = &rest[end + 1..];
    }
    found
}

/// `frame` with `marker` inside its synchronized update (before its end) when it has one, else
/// after it, so the marker arrives with the frame it names.
#[must_use]
pub fn with_marker(frame: &[u8], marker: &[u8]) -> Frame {
    let split = if frame.ends_with(SYNC_END) {
        frame.len() - SYNC_END.len()
    } else {
        frame.len()
    };
    let mut marked = Vec::with_capacity(frame.len() + marker.len());
    marked.extend_from_slice(&frame[..split]);
    marked.extend_from_slice(marker);
    marked.extend_from_slice(&frame[split..]);
    marked
}

/// Read a corpus file.
///
/// # Errors
/// Returns an error when the file cannot be read or a record is truncated.
pub fn read(path: &Path) -> io::Result<Vec<Frame>> {
    decode(&std::fs::read(path)?)
}

/// Decode corpus records.
///
/// # Errors
/// Returns an error when a record is truncated.
pub fn decode(bytes: &[u8]) -> io::Result<Vec<Frame>> {
    let mut frames = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let (length, tail) = rest
            .split_first_chunk::<8>()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated length"))?;
        let length = usize::try_from(u64::from_le_bytes(*length))
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame too large"))?;
        if tail.len() < length {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated frame",
            ));
        }
        let (frame, tail) = tail.split_at(length);
        frames.push(frame.to_vec());
        rest = tail;
    }
    Ok(frames)
}

/// Encode frames as corpus records.
#[must_use]
pub fn encode(frames: &[Frame]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(frames.iter().map(|frame| frame.len() + 8).sum());
    for frame in frames {
        bytes.extend_from_slice(&(frame.len() as u64).to_le_bytes());
        bytes.extend_from_slice(frame);
    }
    bytes
}

/// A generated corpus: what it exercises and its frames.
#[derive(Debug, Clone)]
pub struct Generated {
    /// Short name, for reports.
    pub name: &'static str,
    /// Columns and rows the frames assume.
    pub size: (u16, u16),
    /// The frames, starting with a full repaint.
    pub frames: Vec<Frame>,
}

/// Every generated corpus at `size`, `frames` frames each (besides the first full repaint).
#[must_use]
pub fn generated(size: (u16, u16), frames: usize) -> Vec<Generated> {
    vec![
        Generated {
            name: "su-scroll",
            size,
            frames: su_scroll(size, frames),
        },
        Generated {
            name: "region-linefeed",
            size,
            frames: region_linefeed(size, frames),
        },
        Generated {
            name: "full-repaint",
            size,
            frames: full_repaint(size, frames),
        },
        Generated {
            name: "plain-stream",
            size,
            frames: plain_stream(size, frames),
        },
        Generated {
            name: "unclosed-sync",
            size,
            frames: unclosed_sync(size, frames),
        },
    ]
}

/// A deterministic line of colored code-like text, `cols` cells wide, for line `line`.
fn text_line(cols: u16, line: usize) -> String {
    let mut text = String::new();
    let _ = write!(text, "\x1b[38;5;{}m{line:>6} ", 30 + line % 200);
    let words = [
        "fn", "let", "match", "pipeline", "engine", "row", "cls", "->", "str", "{",
    ];
    let mut width = 7usize;
    let mut index = line;
    while width < usize::from(cols) {
        let word = words[index % words.len()];
        let _ = write!(text, "\x1b[38;5;{}m{word} ", 100 + index % 120);
        width += word.len() + 1;
        index = index.wrapping_mul(31).wrapping_add(7);
    }
    // Clip to the line's width in cells (the text is ASCII).
    let mut visible = 0usize;
    let mut clipped = String::new();
    let mut escape = false;
    for character in text.chars() {
        if character == '\x1b' {
            escape = true;
        }
        if escape {
            clipped.push(character);
            if character == 'm' {
                escape = false;
            }
            continue;
        }
        if visible == usize::from(cols) {
            break;
        }
        clipped.push(character);
        visible += 1;
    }
    clipped.push_str("\x1b[0m\x1b[K");
    clipped
}

/// A full repaint of rows `0..rows` showing lines from `first`.
fn repaint(size: (u16, u16), first: usize) -> Frame {
    let (cols, rows) = size;
    let mut frame = Vec::new();
    frame.extend_from_slice(SYNC_BEGIN);
    frame.extend_from_slice(b"\x1b[H\x1b[2J");
    for row in 0..rows {
        let _ = write_cup(&mut frame, row, 0);
        frame.extend_from_slice(text_line(cols, first + usize::from(row)).as_bytes());
    }
    frame.extend_from_slice(SYNC_END);
    frame
}

fn write_cup(frame: &mut Frame, row: u16, col: u16) -> io::Result<()> {
    use std::io::Write as _;
    write!(frame, "\x1b[{};{}H", row + 1, col + 1)
}

/// Scrolling by three lines a frame with `SU` over the whole screen, then writing the new lines,
/// each frame in a synchronized update (how full-screen applications such as editors and diff
/// viewers scroll).
#[must_use]
pub fn su_scroll(size: (u16, u16), frames: usize) -> Vec<Frame> {
    let (cols, rows) = size;
    let mut out = vec![repaint(size, 0)];
    for frame_index in 0..frames {
        let mut frame = Vec::new();
        frame.extend_from_slice(SYNC_BEGIN);
        let _ = std::io::Write::write_fmt(&mut frame, format_args!("\x1b[1;{rows}r\x1b[3S\x1b[r"));
        for offset in 0..3u16 {
            let row = rows.saturating_sub(3) + offset;
            let _ = write_cup(&mut frame, row, 0);
            let line = usize::from(rows) + frame_index * 3 + usize::from(offset);
            frame.extend_from_slice(text_line(cols, line).as_bytes());
        }
        frame.extend_from_slice(SYNC_END);
        out.push(frame);
    }
    out
}

/// Scrolling a region (all rows but the first and last) with linefeeds at its bottom, as a
/// pager or log view inside a status frame does.
#[must_use]
pub fn region_linefeed(size: (u16, u16), frames: usize) -> Vec<Frame> {
    let (cols, rows) = size;
    let mut out = vec![repaint(size, 0)];
    for frame_index in 0..frames {
        let mut frame = Vec::new();
        frame.extend_from_slice(SYNC_BEGIN);
        let _ = std::io::Write::write_fmt(
            &mut frame,
            format_args!("\x1b[2;{}r", rows.saturating_sub(1)),
        );
        for offset in 0..3usize {
            let _ = write_cup(&mut frame, rows.saturating_sub(2), 0);
            frame.extend_from_slice(b"\n");
            let line = usize::from(rows) + frame_index * 3 + offset;
            frame.extend_from_slice(text_line(cols, line).as_bytes());
        }
        frame.extend_from_slice(b"\x1b[r");
        frame.extend_from_slice(SYNC_END);
        out.push(frame);
    }
    out
}

/// Every row rewritten every frame (a dashboard redrawing in place).
#[must_use]
pub fn full_repaint(size: (u16, u16), frames: usize) -> Vec<Frame> {
    (0..=frames).map(|frame| repaint(size, frame * 7)).collect()
}

/// Lines written with newlines and no synchronized updates (`cat` of a large file), three lines
/// a frame.
#[must_use]
pub fn plain_stream(size: (u16, u16), frames: usize) -> Vec<Frame> {
    let (cols, _) = size;
    let mut out = vec![b"\x1b[H\x1b[2J".to_vec()];
    for frame_index in 0..frames {
        let mut frame = Vec::new();
        for offset in 0..3 {
            frame.extend_from_slice(text_line(cols, frame_index * 3 + offset).as_bytes());
            frame.extend_from_slice(b"\r\n");
        }
        out.push(frame);
    }
    out
}

/// A synchronized update that opens and never closes, then ordinary output: a viewer must not
/// freeze on it.
#[must_use]
pub fn unclosed_sync(size: (u16, u16), frames: usize) -> Vec<Frame> {
    let mut out = vec![repaint(size, 0)];
    let mut open = SYNC_BEGIN.to_vec();
    open.extend_from_slice(b"\x1b[Hstuck frame");
    out.push(open);
    out.extend(plain_stream(size, frames).into_iter().skip(1));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_ride_inside_their_frame_and_are_found_again() {
        let frame = su_scroll((40, 10), 1).pop().unwrap();
        let marked = with_marker(&frame, &marker(7, 123_456_789));
        assert!(marked.starts_with(SYNC_BEGIN) && marked.ends_with(SYNC_END));
        let plain = with_marker(b"text", &marker(8, 1));
        assert!(plain.starts_with(b"text"));
        let mut both = marked.clone();
        both.extend_from_slice(&plain);
        // A marker cut off at the end is not reported.
        both.extend_from_slice(&marker(9, 2)[..MARKER_PREFIX.len() + 2]);
        assert_eq!(markers(&both), [(7, 123_456_789), (8, 1)]);
    }

    #[test]
    fn records_round_trip_and_truncation_is_an_error() {
        let frames = su_scroll((40, 10), 3);
        assert_eq!(frames.len(), 4);
        let bytes = encode(&frames);
        assert_eq!(decode(&bytes).unwrap(), frames);
        assert!(decode(&bytes[..bytes.len() - 1]).is_err());
        assert!(decode(&bytes[..4]).is_err());
    }

    #[test]
    fn generated_corpora_start_with_a_repaint_and_frame_their_updates() {
        for corpus in generated((80, 24), 5) {
            assert!(!corpus.frames.is_empty(), "{}", corpus.name);
            if corpus.name == "su-scroll" || corpus.name == "full-repaint" {
                for frame in &corpus.frames {
                    assert!(frame.starts_with(SYNC_BEGIN) && frame.ends_with(SYNC_END));
                }
            }
        }
        // Lines fit their width in cells (escape sequences end at their final letter).
        let line = text_line(20, 3);
        let mut visible = 0;
        let mut escape = false;
        for character in line.chars() {
            if character == '\x1b' {
                escape = true;
            } else if escape {
                escape = !character.is_ascii_alphabetic();
            } else {
                visible += 1;
            }
        }
        assert_eq!(visible, 20, "{line:?}");
    }
}
