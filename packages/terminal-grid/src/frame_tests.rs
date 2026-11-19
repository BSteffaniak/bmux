//! Frames keep a replica exactly level with its source grid: every frame applied leaves the
//! replica's whole snapshot (screens, history, cursor, modes, palette) equal to the source's,
//! however output is split, however often the replica syncs, and across resizes, resets,
//! screen switches and history erasure.

use crate::{GridFrame, GridLimits, GridSnapshot, ReplicaCursor, TerminalGrid, TerminalGridStream};

/// A small deterministic generator (xorshift), so every failure replays from its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound.max(1)
    }

    /// An index below `len`.
    fn pick(&mut self, len: usize) -> usize {
        usize::try_from(self.below(len as u64)).unwrap_or(0)
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

/// One random burst of terminal output, mixing what programs write.
fn burst(rng: &mut Rng, out: &mut Vec<u8>) {
    let words: [&str; 8] = [
        "hello",
        "界面",
        "e\u{301}x",
        "👨‍👩‍👧",
        "tab\tbed",
        "a",
        "~$ ",
        "─┼─",
    ];
    match rng.below(20) {
        0..=5 => {
            for _ in 0..rng.below(12) {
                out.extend_from_slice(words[rng.pick(8)].as_bytes());
                out.push(b' ');
            }
        }
        6 => out.extend_from_slice(b"\r\n"),
        7 => {
            for _ in 0..rng.below(40) {
                out.extend_from_slice(b"line of history\r\n");
            }
        }
        8 => out.extend_from_slice(format!("\x1b[{}m", rng.below(108)).as_bytes()),
        9 => out.extend_from_slice(
            format!(
                "\x1b[38;5;{}m\x1b[48;2;{};9;9m",
                rng.below(256),
                rng.below(256)
            )
            .as_bytes(),
        ),
        10 => out.extend_from_slice(
            format!("\x1b[{};{}H", rng.below(30) + 1, rng.below(90) + 1).as_bytes(),
        ),
        11 => out.extend_from_slice(
            [
                &b"\x1b[J"[..],
                b"\x1b[1J",
                b"\x1b[2J",
                b"\x1b[K",
                b"\x1b[2K",
                b"\x1b[3J",
            ][rng.pick(6)],
        ),
        12 => out.extend_from_slice(
            format!("\x1b[{};{}r", rng.below(10) + 1, rng.below(20) + 10).as_bytes(),
        ),
        13 => out.extend_from_slice(
            format!(
                "\x1b[{}{}",
                rng.below(5) + 1,
                ["L", "M", "S", "T", "@", "P"][rng.pick(6)]
            )
            .as_bytes(),
        ),
        14 => out.extend_from_slice(
            [
                &b"\x1b[?1049h"[..],
                b"\x1b[?1049l",
                b"\x1b[?47h",
                b"\x1b[?47l",
            ][rng.pick(4)],
        ),
        15 => out.extend_from_slice(&b"wrapping ".repeat(rng.pick(30))),
        16 => out.extend_from_slice(b"\x1b[r\x1bM\x1bD\x1bE"),
        17 => out.extend_from_slice(if rng.chance(20) { b"\x1bc" } else { b"\x1b[!p" }),
        18 => out.extend_from_slice(b"\x1b7\x1b[5;5H\x1b[4hins\x1b[4l\x1b8"),
        _ => out.extend_from_slice(b"\x1b[?25l\x1b[?2004h\x1b[?1000h\x1b[?25h"),
    }
}

/// A grid's whole snapshot, with what replication does not carry (revision counters, the
/// parser's unfinished prefix) left out.
fn replicated(grid: &TerminalGrid) -> GridSnapshot {
    let mut snapshot = grid.snapshot(0, 100_000);
    snapshot.revision = 0;
    snapshot.content_revision = 0;
    snapshot.pending_bytes.clear();
    snapshot
}

/// The paths where `left` and `right` differ, at most 12.
fn differ(path: &str, left: &serde_json::Value, right: &serde_json::Value, out: &mut Vec<String>) {
    use serde_json::Value;
    if out.len() >= 12 || left == right {
        return;
    }
    match (left, right) {
        (Value::Object(l), Value::Object(r)) => {
            for (key, value) in l {
                differ(
                    &format!("{path}.{key}"),
                    value,
                    r.get(key).unwrap_or(&Value::Null),
                    out,
                );
            }
        }
        (Value::Array(l), Value::Array(r)) if l.len() == r.len() => {
            for (index, (a, b)) in l.iter().zip(r).enumerate() {
                differ(&format!("{path}[{index}]"), a, b, out);
            }
        }
        (Value::Array(l), Value::Array(r)) => {
            out.push(format!("{path}: {} vs {} entries", l.len(), r.len()));
        }
        _ => {
            let show = |value: &Value| value.to_string().chars().take(300).collect::<String>();
            out.push(format!("{path}: {} vs {}", show(left), show(right)));
        }
    }
}

/// A replica: a grid kept only by frames, and where it is.
struct Replica {
    grid: TerminalGrid,
    cursor: ReplicaCursor,
}

impl Replica {
    fn new() -> Self {
        Self {
            grid: TerminalGrid::new(1, 1, GridLimits::default()).unwrap(),
            cursor: ReplicaCursor::default(),
        }
    }

    /// Bring the replica level with `source` through a frame (round-tripped through
    /// serialization, as a wire carries it) and check it is.
    fn sync(&mut self, source: &mut TerminalGridStream, context: &str) {
        let frame = source.grid_mut().frame_since(self.cursor, usize::MAX);
        let wire: GridFrame = serde_json::from_slice(&serde_json::to_vec(&frame).unwrap()).unwrap();
        assert_eq!(wire, frame, "{context}: frame survives serialization");
        self.grid
            .apply_frame(&wire, &mut self.cursor)
            .unwrap_or_else(|error| panic!("{context}: {error}"));
        let (replica, source) = (replicated(&self.grid), replicated(source.grid()));
        if replica != source {
            let mut differences = Vec::new();
            differ(
                "",
                &serde_json::to_value(&replica).unwrap(),
                &serde_json::to_value(&source).unwrap(),
                &mut differences,
            );
            panic!(
                "{context}: replica differs from its source (replica, source):\n{}",
                differences.join("\n")
            );
        }
    }
}

#[test]
fn replicas_stay_level_with_their_source_through_random_output() {
    for seed in 1..=24_u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let (mut cols, mut rows) = (90_u16, 30_u16);
        let limits = GridLimits {
            scrollback_rows: 500,
        };
        let mut source = TerminalGridStream::new(cols, rows, limits).unwrap();
        let (mut eager, mut lazy) = (Replica::new(), Replica::new());
        let mut out = Vec::new();
        for step in 0..400 {
            out.clear();
            for _ in 0..=rng.below(4) {
                burst(&mut rng, &mut out);
            }
            // Split the burst anywhere, as reads split output.
            let split = rng.pick(out.len() + 1);
            source.process(&out[..split]);
            source.process(&out[split..]);
            if rng.chance(3) {
                cols = 20 + u16::try_from(rng.pick(100)).unwrap_or(0);
                rows = 5 + u16::try_from(rng.pick(40)).unwrap_or(0);
                source.resize(cols, rows).unwrap();
            }
            let context = format!("seed {seed} step {step}");
            if rng.chance(60) {
                eager.sync(&mut source, &format!("{context} eager"));
            }
            if rng.chance(4) {
                lazy.sync(&mut source, &format!("{context} lazy"));
            }
        }
        eager.sync(&mut source, &format!("seed {seed} final eager"));
        lazy.sync(&mut source, &format!("seed {seed} final lazy"));
    }
}

#[test]
fn a_frame_for_another_base_is_refused_and_a_reset_frame_recovers() {
    let mut source = TerminalGridStream::new(20, 4, GridLimits::default()).unwrap();
    let mut replica = Replica::new();
    replica.sync(&mut source, "first");
    source.process(b"one\r\n");
    let stale = source
        .grid_mut()
        .frame_since(ReplicaCursor::default(), usize::MAX);
    assert!(stale.reset, "a replica at the start gets a reset frame");
    source.process(b"two\r\n");
    let mut skipped = source.grid_mut().frame_since(replica.cursor, usize::MAX);
    skipped.base.epoch += 1;
    assert_eq!(
        replica.grid.apply_frame(&skipped, &mut replica.cursor),
        Err(crate::FrameApplyError::BaseMismatch)
    );
    replica
        .grid
        .apply_frame(&stale, &mut replica.cursor)
        .expect("a reset frame applies from any state");
    replica.sync(&mut source, "after recovery");
}

#[test]
fn quiet_frames_carry_only_state_and_scrolls_move_rows_instead_of_resending_them() {
    let mut source = TerminalGridStream::new(40, 10, GridLimits::default()).unwrap();
    let mut replica = Replica::new();
    for line in 0..10 {
        source.process(format!("line {line}\r\n").as_bytes());
    }
    replica.sync(&mut source, "filled");
    let quiet = source.grid_mut().frame_since(replica.cursor, usize::MAX);
    assert!(quiet.is_state_only());
    replica
        .grid
        .apply_frame(&quiet, &mut replica.cursor)
        .unwrap();
    source.process(b"next\r\n");
    let scrolled = source.grid_mut().frame_since(replica.cursor, usize::MAX);
    assert_eq!(scrolled.moves.len(), 1, "{:?}", scrolled.moves);
    // The line written and the row exposed, not the screen.
    assert!(scrolled.rows.len() <= 2, "{} rows", scrolled.rows.len());
    assert_eq!(scrolled.history.len(), 1);
    replica
        .grid
        .apply_frame(&scrolled, &mut replica.cursor)
        .unwrap();
    assert_eq!(replicated(&replica.grid), replicated(source.grid()));
}

#[test]
fn a_capped_first_frame_sends_recent_history_and_marks_the_rest_truncated() {
    let mut source = TerminalGridStream::new(30, 5, GridLimits::default()).unwrap();
    for line in 0..50 {
        source.process(format!("line {line}\r\n").as_bytes());
    }
    let mut replica = Replica::new();
    let frame = source.grid_mut().frame_since(replica.cursor, 10);
    assert!(frame.reset);
    assert_eq!(frame.history.len(), 10);
    replica
        .grid
        .apply_frame(&frame, &mut replica.cursor)
        .unwrap();
    assert_eq!(replica.grid.history_line_count(), 10);
    assert!(replica.grid.snapshot(0, 100).history_truncated == Some(true));
    // Later frames carry what is new; the replica keeps the recent lines it was sent.
    source.process(b"new\r\n");
    let frame = source.grid_mut().frame_since(replica.cursor, 10);
    assert_eq!(frame.history.len(), 1);
    replica
        .grid
        .apply_frame(&frame, &mut replica.cursor)
        .unwrap();
    assert_eq!(replica.grid.history_line_count(), 11);
    let tail = |grid: &TerminalGrid| grid.snapshot(0, 16).rows;
    assert_eq!(tail(&replica.grid), tail(source.grid()));
}
