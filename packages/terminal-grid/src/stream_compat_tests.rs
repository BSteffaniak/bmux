//! Bounded streaming integration regressions. Registered by the coordinator.
//!
//! Exact revisions are compared only between identical processing schedules.
//! Cross-schedule comparisons retain all snapshot fields except the two revision
//! counters. Restoration now uses bounded canonical decoder replay prefixes
//! and a no-op performer: already-applied effects are not repeated, including
//! when hydrating legacy raw prefixes. Resource and continuation regressions
//! are active; the coordinator verified the repaired CSI/LF baseline.

use crate::{GridLimits, GridSnapshot, TerminalGridStream, TerminalProtocolTracker};

const WIDTH: u16 = 24;
const HEIGHT: u16 = 4;
// Loose representation-independent ceiling: 1024 OSC payload bytes plus
// at most 32 bounded numeric parameters, two intermediates, and syntax/state.
// This asserts canonical replay storage, not silent raw-prefix truncation.
const REPLAY_PREFIX_BOUND: usize = 4096;

fn stream() -> TerminalGridStream {
    TerminalGridStream::new(WIDTH, HEIGHT, GridLimits::default()).unwrap()
}

fn capture(stream: &TerminalGridStream) -> GridSnapshot {
    stream.snapshot(0, stream.grid().main_row_count())
}

fn wire_snapshot(snapshot: &GridSnapshot) -> GridSnapshot {
    serde_json::from_slice(&serde_json::to_vec(snapshot).unwrap()).unwrap()
}

fn semantic(mut snapshot: GridSnapshot) -> GridSnapshot {
    snapshot.revision = 0;
    snapshot.content_revision = 0;
    snapshot
}

fn restored_protocol(snapshot: &GridSnapshot) -> TerminalProtocolTracker {
    let mut tracker = TerminalProtocolTracker::new();
    tracker.restore(
        snapshot.protocol,
        snapshot.mode == "alternate",
        &snapshot.pending_bytes,
    );
    tracker
}

fn assert_protocol(stream: &TerminalGridStream, tracker: &TerminalProtocolTracker) {
    assert_eq!(stream.grid().protocol_state(), tracker.protocol_state());
    assert_eq!(
        capture(stream).mode == "alternate",
        tracker.alternate_screen()
    );
}

/// Each split exercises live parsing, JSON hydration, wire delta application,
/// and protocol restoration at the same watermark, then continues all paths.
fn every_split(bytes: &[u8]) {
    let mut whole = stream();
    whole.process(bytes);
    let expected = semantic(capture(&whole));
    let mut whole_protocol = TerminalProtocolTracker::new();
    let expected_toggle = whole_protocol.process(bytes).toggled_alternate;
    assert_protocol(&whole, &whole_protocol);

    for split in 0..=bytes.len() {
        let mut source = stream();
        let mut replica = stream();
        let mut protocol = TerminalProtocolTracker::new();
        let first_toggle = protocol.process(&bytes[..split]).toggled_alternate;
        if let Some(delta) = source.process_delta(&bytes[..split]) {
            let wire = serde_json::to_vec(&delta).unwrap();
            replica
                .apply_delta(
                    &serde_json::from_slice(&wire).unwrap(),
                    GridLimits::default(),
                )
                .unwrap();
        }
        let prefix = wire_snapshot(&capture(&source));
        assert_eq!(capture(&replica), prefix, "delta prefix {split}: {bytes:?}");
        assert_protocol(&source, &protocol);
        let mut hydrated =
            TerminalGridStream::from_snapshot(&prefix, GridLimits::default()).unwrap();
        assert_eq!(
            capture(&hydrated),
            prefix,
            "hydrate prefix {split}: {bytes:?}"
        );
        let mut restored = restored_protocol(&prefix);
        assert_protocol(&source, &restored);

        let second_toggle = protocol.process(&bytes[split..]).toggled_alternate;
        let restored_toggle = restored.process(&bytes[split..]).toggled_alternate;
        assert_eq!(
            second_toggle, restored_toggle,
            "protocol restore {split}: {bytes:?}"
        );
        assert_eq!(first_toggle || second_toggle, expected_toggle);
        if let Some(delta) = source.process_delta(&bytes[split..]) {
            replica.apply_delta(&delta, GridLimits::default()).unwrap();
        }
        hydrated.process(&bytes[split..]);
        let actual = capture(&source);
        assert_eq!(capture(&replica), actual, "delta suffix {split}: {bytes:?}");
        assert_eq!(
            capture(&hydrated),
            actual,
            "hydrate suffix {split}: {bytes:?}"
        );
        assert_protocol(&source, &protocol);
        assert_protocol(&hydrated, &restored);
        assert_eq!(semantic(actual), expected, "whole/split {split}: {bytes:?}");
    }
}

#[test]
fn every_split_utf8_csi_osc_dcs_sos_pm_apc() {
    let cases: &[&[u8]] = &[
        "Aé界🙂e\u{301}Z".as_bytes(),
        b"A\x1b[38;2;12;34;56mB\x1b[0m\x1b[2;3HC",
        b"A\x1b]0;title\x07B",
        "A\x1b]2;títle界\x1b\\B".as_bytes(),
        b"A\x1bP1;2$qignored\x1b\\B",
        b"A\x1bXignored\x1b\\B",
        b"A\x1b^ignored\x1b\\B",
        b"A\x1b_ignored\x1b\\B",
        b"A\x1b[?1;1000;1006;2004h\x1b=\x1b[?1;1000;1006;2004l\x1b>B",
        b"main\x1b[?1049h\x1b[31malt\x1b[?1049l!",
        b"\x1b)0\x0eqx\x0fA\x1b(0lqk\x1b(BZ",
        b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix",
    ];
    for bytes in cases {
        every_split(bytes);
    }
}

#[test]
fn representative_grid_and_protocol_outcomes_are_not_only_differential() {
    let mut source = stream();
    source.process(b"main\x1b[?1049h\x1b[?1000h\x1b[?1006h\x1b[?2004h\x1b=\x1b)0\x0eqx\x0f");
    let alternate = capture(&source);
    assert_eq!(alternate.mode, "alternate");
    assert_eq!(
        crate::row_text(&source.grid().viewport_rows()[0], usize::from(WIDTH))
            .trim_end_matches(' '),
        "─│"
    );
    assert!(alternate.protocol.application_keypad);
    assert!(alternate.protocol.bracketed_paste);
    assert_eq!(
        alternate.protocol.mouse_mode(),
        crate::MouseProtocolMode::PressRelease
    );
    assert_eq!(
        alternate.protocol.mouse_encoding(),
        crate::MouseProtocolEncoding::Sgr
    );
    let mut hydrated =
        TerminalGridStream::from_snapshot(&wire_snapshot(&alternate), GridLimits::default())
            .unwrap();
    let mut protocol = restored_protocol(&alternate);
    let suffix = b"\x1b[?1049l\x1b[?1000l\x1b[?1006l\x1b[?2004l\x1b>!";
    source.process(suffix);
    hydrated.process(suffix);
    assert!(protocol.process(suffix).toggled_alternate);
    assert_eq!(capture(&hydrated), capture(&source));
    assert_protocol(&source, &protocol);
    assert_eq!(
        crate::row_text(&source.grid().viewport_rows()[0], usize::from(WIDTH))
            .trim_end_matches(' '),
        "main!"
    );
    assert!(!source.grid().protocol_state().bracketed_paste);
    assert!(!source.grid().protocol_state().application_keypad);
}

#[test]
fn every_split_interrupted_and_malformed_sequences() {
    let cases: &[&[u8]] = &[
        b"A\x1b[31\x18B",     // CAN cancels CSI.
        b"A\x1b[31\x1aB",     // SUB cancels CSI.
        b"A\x1b[31\x1b[32mB", // New ESC replaces a CSI.
        b"A\x1b[1:2:3mB",     // Unsupported subparameters must stay bounded.
        b"A\x1b[999999999999999999999999999999CB",
        b"A\x1b[1;2;3;4;5;6;7;8;9;10;11;12;13;14;15;16;17;18;19;20mB",
        b"A\x1b[?1$hB", // An intermediate prevents mode dispatch.
        b"A\xff\xfeB",
        b"A\xc3\x1b[31mB", // ESC interrupts an incomplete UTF-8 scalar.
        b"A\xf0\x9f\x18B",
    ];
    for bytes in cases {
        every_split(bytes);
    }
}

#[test]
fn bytewise_and_seeded_chunks_restore_after_each_prefix() {
    let bytes = "aé界\x1b[32mb\x1b[0m\x1b]0;title\x07\x1bPqdata\x1b\\\x1b[?1049halt\x1b[?1049l\x1b)0\x0eqx\x0fZ".as_bytes();
    let mut whole = stream();
    whole.process(bytes);
    let expected = semantic(capture(&whole));
    // Fixed seed cases, fixed corpus, no randomized or unbounded generation.
    for seed in [0_u32, 1, 7, 0x5eed] {
        let mut source = stream();
        let mut hydrated = stream();
        let mut replica = stream();
        let mut tracker = TerminalProtocolTracker::new();
        let mut restored = TerminalProtocolTracker::new();
        let mut state = seed;
        let mut offset = 0;
        while offset < bytes.len() {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let length = if seed == 0 {
                1
            } else {
                1 + usize::try_from(state % 7).unwrap()
            };
            let end = (offset + length).min(bytes.len());
            let chunk = &bytes[offset..end];
            let live_outcome = tracker.process(chunk);
            assert_eq!(restored.process(chunk), live_outcome);
            if let Some(delta) = source.process_delta(chunk) {
                replica.apply_delta(&delta, GridLimits::default()).unwrap();
            }
            hydrated.process(chunk);
            let snapshot = wire_snapshot(&capture(&source));
            assert_eq!(capture(&hydrated), snapshot, "seed {seed}, prefix {end}");
            assert_eq!(capture(&replica), snapshot, "seed {seed}, delta {end}");
            assert_protocol(&source, &tracker);
            assert_protocol(&source, &restored);
            hydrated = TerminalGridStream::from_snapshot(&snapshot, GridLimits::default()).unwrap();
            restored = restored_protocol(&snapshot);
            assert_eq!(capture(&hydrated), snapshot);
            offset = end;
        }
        assert_eq!(semantic(capture(&source)), expected, "seed {seed}");
    }
}

#[test]
fn prefix_only_delta_advances_revision_without_dirty_rows() {
    let mut source = stream();
    let mut replica = stream();
    let before = capture(&source);
    assert!(source.process_delta(b"").is_none());
    assert_eq!(capture(&source), before);
    for prefix in [b"\x1b".as_slice(), b"[", b"3", b"1"] {
        let before = capture(&source);
        let delta = source
            .process_delta(prefix)
            .expect("changed prefix is replicated");
        assert_eq!(delta.base_revision, before.revision);
        assert!(delta.revision > before.revision);
        assert_eq!(delta.content_revision, before.content_revision);
        assert!(!delta.reset_rows);
        assert!(
            delta.row_updates.is_empty(),
            "prefix does not dirty visible rows"
        );
        replica.apply_delta(&delta, GridLimits::default()).unwrap();
        assert_eq!(capture(&replica), capture(&source));
        let hydrated = TerminalGridStream::from_snapshot(
            &wire_snapshot(&capture(&source)),
            GridLimits::default(),
        )
        .unwrap();
        assert_eq!(capture(&hydrated), capture(&source));
    }
    let delta = source.process_delta(b"mR").unwrap();
    assert!(!delta.row_updates.is_empty());
    assert!(delta.content_revision > before.content_revision);
    replica.apply_delta(&delta, GridLimits::default()).unwrap();
    assert_eq!(capture(&replica), capture(&source));
}

#[test]
fn rejected_snapshot_budget_does_not_consume_pending_parser_state() {
    for prefix in [b"\x1b[38;2;".as_slice(), b"\xf0\x9f\x99", b"\x1b]0;title"] {
        let suffix: &[u8] = match prefix {
            b"\x1b[38;2;" => b"1;2;3mR",
            b"\xf0\x9f\x99" => b"\x82Z",
            _ => b"\x07Z",
        };
        let mut source = stream();
        source.process(prefix);
        let before = capture(&source);
        for budget in [0, 1, prefix.len().saturating_sub(1)] {
            assert!(
                source
                    .try_snapshot(0, usize::from(HEIGHT), budget)
                    .is_none()
            );
            assert_eq!(capture(&source), before);
        }
        let admitted = source
            .try_snapshot(0, usize::from(HEIGHT), 1 << 20)
            .unwrap();
        assert_eq!(admitted, before);
        let mut hydrated =
            TerminalGridStream::from_snapshot(&wire_snapshot(&admitted), GridLimits::default())
                .unwrap();
        source.process(suffix);
        hydrated.process(suffix);
        assert_eq!(capture(&hydrated), capture(&source));
    }
}

#[test]
fn cancelled_strings_must_not_replay_printed_suffix_on_restore() {
    // Minimal prefixes: ESC ] CAN B and ESC P q CAN B. CAN has already
    // returned the live vte parser to ground, so B is visible. The heuristic
    // only recognized BEL/ST string endings and retained the entire prefix.
    // Canonical replay and no-op hydration must not print B again; protocol
    // restoration ignores printable characters and retains the same mode state.
    for bytes in [
        b"\x1b]\x18B".as_slice(),
        b"\x1bPq\x18B",
        b"\x1bX\x18B",
        b"\x1b^\x18B",
        b"\x1b_\x1aB",
    ] {
        every_split(bytes);
    }
}

#[test]
fn rejected_delta_keeps_existing_prefix_for_retry() {
    let mut source = stream();
    let mut replica = stream();
    let first = source.process_delta(b"\x1b[3").unwrap();
    replica.apply_delta(&first, GridLimits::default()).unwrap();
    let before = capture(&replica);
    let valid = source.process_delta(b"1mR").unwrap();
    let mut invalid = valid.clone();
    invalid.base_revision = invalid.base_revision.saturating_add(1);
    assert!(
        replica
            .apply_delta(&invalid, GridLimits::default())
            .is_err()
    );
    assert_eq!(capture(&replica), before);
    // Continue the retained parser independently, not merely retry hydration.
    let mut continued = TerminalGridStream::from_snapshot(&before, GridLimits::default()).unwrap();
    continued.process(b"1mR");
    replica.process(b"1mR");
    assert_eq!(capture(&replica), capture(&continued));
    assert_eq!(capture(&replica), capture(&source));
}

#[test]
fn embedded_csi_controls_restore_grid_and_protocol_without_reexecution() {
    // LF/VT/FF advance rows; CR/BS/HT alter the cursor; SO/SI mutate charset
    // state. Every prefix is JSON-hydrated and delta-applied by every_split,
    // including the watermark immediately after the embedded control.
    for bytes in [
        b"abc\x1b[\n31mR".as_slice(),
        b"abc\x1b[\r31mR",
        b"abc\x1b[\x0831mR",
        b"abc\x1b[\t31mR",
        b"abc\x1b[\x0b31mR",
        b"abc\x1b[\x0c31mR",
        b"\x1b)0\x1b[\x0e31mqx",
        b"\x1b)0\x0eq\x1b[\x0f31mAx",
        b"\x1b)0\x1b[\x0e\x0f31mA",
        b"abc\x1b[\x0731mR", // BEL is a safe no-op for grid and tracker.
    ] {
        every_split(bytes);
    }
}

#[test]
fn escape_controls_preserve_every_split_continuation() {
    // ESC remains active across executed C0 controls. The old heuristic lost
    // the introducer at ESC LF [?1 | h, affecting both cells and protocol.
    for bytes in [
        b"\x1b\n[?1hZ".as_slice(),
        b"abc\x1b\r[?1hZ",
        b"abc\x1b\x08[?1000hZ",
        b"abc\x1b\t[?1006hZ",
        b"\x1b\x07[?2004hZ",
        b"\x1b)0\x1b\x0e[31mqx",
        b"\x1b)0\x0eq\x1b\x0f[31mAx",
        b"\x1b\n\r\x07[?1;2004hZ",
        b"\x1b\n\x1b[?1hZ", // Replacement ESC.
        b"\x1b\n\x18[?1hZ", // CAN returns to ground.
        b"\x1b\n\x1a[?1hZ", // SUB returns to ground.
        b"\x1b \n(BZ",      // Executed LF inside an intermediate sequence.
    ] {
        every_split(bytes);
    }
}

fn bounded_long_prefix_case(introducer: &[u8], fill: u8, suffix: &[u8]) {
    let mut source = stream();
    let mut hydrated = stream();
    let mut replica = stream();
    let mut tracker = TerminalProtocolTracker::new();
    let mut restored = TerminalProtocolTracker::new();
    let chunk = [fill; 1024];
    // Feed exactly 64 KiB, in bounded chunks, with all replicas restored at
    // every watermark. Equality after continuation prevents 'bounded' storage
    // from passing by discarding significant decoder state.
    for (index, bytes) in std::iter::once(introducer)
        .chain(std::iter::repeat_n(chunk.as_slice(), 64))
        .enumerate()
    {
        let outcome = tracker.process(bytes);
        assert_eq!(restored.process(bytes), outcome);
        if let Some(delta) = source.process_delta(bytes) {
            assert!(delta.pending_bytes.len() <= REPLAY_PREFIX_BOUND);
            replica.apply_delta(&delta, GridLimits::default()).unwrap();
        }
        hydrated.process(bytes);
        let prefix = wire_snapshot(&capture(&source));
        assert!(
            prefix.pending_bytes.len() <= REPLAY_PREFIX_BOUND,
            "introducer {introducer:02x?}, chunk {index}, prefix bytes {}",
            prefix.pending_bytes.len()
        );
        assert_eq!(capture(&hydrated), prefix);
        assert_eq!(capture(&replica), prefix);
        assert_protocol(&source, &tracker);
        assert_protocol(&source, &restored);
        for budget in [0, 1] {
            assert!(
                source
                    .try_snapshot(0, usize::from(HEIGHT), budget)
                    .is_none()
            );
            assert_eq!(
                capture(&source),
                prefix,
                "rejection must preserve continuity"
            );
        }
        let admitted = source
            .try_snapshot(0, usize::from(HEIGHT), 1 << 20)
            .unwrap();
        assert_eq!(admitted, prefix);
        hydrated = TerminalGridStream::from_snapshot(&prefix, GridLimits::default()).unwrap();
        restored = restored_protocol(&prefix);
        assert_eq!(capture(&hydrated), prefix);
    }
    let outcome = tracker.process(suffix);
    assert_eq!(restored.process(suffix), outcome);
    if let Some(delta) = source.process_delta(suffix) {
        replica.apply_delta(&delta, GridLimits::default()).unwrap();
    }
    hydrated.process(suffix);
    assert_eq!(capture(&hydrated), capture(&source));
    assert_eq!(capture(&replica), capture(&source));
    assert_protocol(&source, &tracker);
    assert_protocol(&source, &restored);
    assert!(source.grid().protocol_state().bracketed_paste);
    assert!(capture(&source).pending_bytes.is_empty());
    assert_eq!(
        crate::row_text(&source.grid().viewport_rows()[0], usize::from(WIDTH))
            .trim_end_matches(' '),
        "Z"
    );
    // Also compare complete-input semantics, not only identical schedules.
    let mut bytes = introducer.to_vec();
    bytes.extend(std::iter::repeat_n(fill, 64 * 1024));
    bytes.extend_from_slice(suffix);
    let mut whole = stream();
    whole.process(&bytes);
    assert_eq!(semantic(capture(&source)), semantic(capture(&whole)));
}

#[test]
fn long_unterminated_families_bound_replay_prefix_and_recover() {
    let cases: &[(&[u8], u8, &[u8])] = &[
        (b"\x1b]2;", b'a', b"\x07\x1b[?2004hZ"),
        (b"\x1b]2;", b'a', b"\x1b\\\x1b[?2004hZ"),
        (b"\x1b[", b'1', b"m\x1b[?2004hZ"),
        (b"\x1b[", b';', b"m\x1b[?2004hZ"),
        (b"\x1b[", b':', b"m\x1b[?2004hZ"),
        (b"\x1b[1?", b'1', b"m\x1b[?2004hZ"), // CSI discard.
        (b"\x1bP", b'1', b"qdata\x1b\\\x1b[?2004hZ"),
        (b"\x1bPq", b'a', b"\x1b\\\x1b[?2004hZ"),
        (b"\x1bP1?", b'a', b"\x1b\\\x1b[?2004hZ"), // DCS discard.
        (b"\x1bX", b'a', b"\x1b\\\x1b[?2004hZ"),
        (b"\x1b^", b'a', b"\x1b\\\x1b[?2004hZ"),
        (b"\x1b_", b'a', b"\x1b\\\x1b[?2004hZ"),
        (b"\x1b ", b' ', b"B\x1b[?2004hZ"),
    ];
    for &(introducer, fill, suffix) in cases {
        bounded_long_prefix_case(introducer, fill, suffix);
    }
}

#[test]
fn legacy_raw_escape_control_prefix_hydrates_without_reexecuting_effects() {
    let raw = b"\x1b\n[?1";
    let mut live = stream();
    live.process(raw);
    let canonical = capture(&live);
    let mut legacy = canonical.clone();
    legacy.pending_bytes = raw.to_vec();
    let legacy = wire_snapshot(&legacy);
    let mut hydrated = TerminalGridStream::from_snapshot(&legacy, GridLimits::default()).unwrap();
    // Raw prefixes remain valid input without replaying LF or modifying any
    // grid metadata. Their representation need not match canonical captures.
    assert_eq!(
        semantic(hydrated.grid().snapshot(0, usize::from(HEIGHT))),
        semantic(live.grid().snapshot(0, usize::from(HEIGHT)))
    );
    assert_eq!(hydrated.grid().revision(), live.grid().revision());
    let mut tracker = restored_protocol(&legacy);
    live.process(b"hZ");
    hydrated.process(b"hZ");
    let _ = tracker.process(b"hZ");
    assert_eq!(capture(&hydrated), capture(&live));
    assert_protocol(&hydrated, &tracker);
    assert!(hydrated.grid().protocol_state().application_cursor);
    assert_eq!(hydrated.grid().cursor().row, 1);
}

#[test]
fn replay_state_prefix_next_byte_matrix_preserves_continuation() {
    // The grid intentionally ignores OSC/DCS payload callbacks; this checks
    // public grid/protocol semantics and decoder continuation, not callbacks.
    let prefixes: &[&[u8]] = &[
        b"",
        b"\x1b",
        b"\x1b\n",
        b"\x1b ",
        b"\x1b   ",
        b"\x1b[",
        b"\x1b[?",
        b"\x1b[1;",
        b"\x1b[1:",
        b"\x1b[1 ",
        b"\x1b[1?",
        b"\x1b[   ",
        b"\x1bP",
        b"\x1bP?",
        b"\x1bP1;",
        b"\x1bP1:",
        b"\x1bP1 ",
        b"\x1bP1?",
        b"\x1bPqdata",
        b"\x1bP   ",
        b"\x1b]",
        b"\x1b]2;abc",
        b"\x1b]2;abc\x1b",
        b"\x1bX",
        b"\x1b^",
        b"\x1b_",
        b"\xc2",
        b"\xe2",
        b"\xe2\x82",
        b"\xf0",
        b"\xf0\x9f",
        b"\xf0\x9f\x92",
    ];
    for &prefix in prefixes {
        for byte in 0..=u8::MAX {
            let mut live = stream();
            live.process(b"\x1b)0seed");
            live.process(prefix);
            live.process(&[byte]);
            let snapshot = wire_snapshot(&capture(&live));
            assert!(snapshot.pending_bytes.len() <= REPLAY_PREFIX_BOUND);
            let mut hydrated =
                TerminalGridStream::from_snapshot(&snapshot, GridLimits::default()).unwrap();
            assert_eq!(
                capture(&hydrated),
                snapshot,
                "prefix {prefix:02x?}, byte {byte:02x}"
            );
            let mut tracker = TerminalProtocolTracker::new();
            let _ = tracker.process(b"\x1b)0seed");
            let _ = tracker.process(prefix);
            let _ = tracker.process(&[byte]);
            let mut restored = restored_protocol(&snapshot);
            // Exercise continuation BEFORE cancellation, then deterministic
            // recovery. Cancelling first would conceal incorrect restored state.
            for suffix in [b"1;31mR".as_slice(), b"\x1b\\\x18\x1b[?1;2004h\x0eqx\x0fZ"] {
                live.process(suffix);
                hydrated.process(suffix);
                assert_eq!(restored.process(suffix), tracker.process(suffix));
                assert_eq!(
                    semantic(capture(&hydrated)),
                    semantic(capture(&live)),
                    "prefix {prefix:02x?}, byte {byte:02x}, suffix {suffix:02x?}"
                );
                assert_protocol(&live, &tracker);
                assert_protocol(&hydrated, &restored);
            }
        }
    }
}

#[test]
fn snapshot_restore_must_not_reexecute_csi_embedded_linefeed() {
    // Verified baseline defect: ESC [ LF | 31mR. The live parser has
    // already executed LF; the old heuristic retained ESC [ LF. Canonical
    // hydration replays decoder transitions without replaying grid effects.
    // Protocol restore has no LF side effect.
    let mut source = stream();
    source.process(b"\x1b[\n");
    let prefix = wire_snapshot(&capture(&source));
    let mut hydrated = TerminalGridStream::from_snapshot(&prefix, GridLimits::default()).unwrap();
    let mut protocol = restored_protocol(&prefix);
    assert_eq!(
        capture(&hydrated),
        prefix,
        "hydration must not execute LF again"
    );
    source.process(b"31mR");
    hydrated.process(b"31mR");
    let _ = protocol.process(b"31mR");
    assert_eq!(capture(&hydrated), capture(&source));
    assert_protocol(&source, &protocol);
}
