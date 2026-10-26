//! Canonical OSC 22 state, byte ordering, and recovery regression tests.

use crate::{GridLimits, ProtocolState, TerminalGridStream, TerminalProtocolTracker};
use bmux_terminal_protocol::pointer_shape::{PointerShape, PointerShapeState};

fn stream() -> TerminalGridStream {
    TerminalGridStream::new(20, 3, GridLimits::default()).unwrap()
}

fn current(stream: &TerminalGridStream) -> Option<PointerShape> {
    stream
        .grid()
        .protocol_state()
        .pointer_shapes
        .current(stream.grid().mode() == crate::GridMode::Alternate)
}

#[test]
fn queries_observe_exact_byte_order_at_every_chunk_split() {
    let input =
        b"\x1b]22;pointer\x1b\\\x1b]22;?__current__\x07\x1b]22;text\x07\x1b]22;?__current__\x1b\\";
    for split in 0..=input.len() {
        let mut grid = stream();
        let mut answers = Vec::new();
        let mut callback = |query: &[u8], shape| answers.push((query.to_vec(), shape));
        grid.process_with_pointer_queries(&input[..split], &mut callback);
        grid.process_with_pointer_queries(&input[split..], &mut callback);
        assert_eq!(
            answers,
            vec![
                (b"__current__".to_vec(), Some(PointerShape::Pointer)),
                (b"__current__".to_vec(), Some(PointerShape::Text)),
            ],
            "split {split}"
        );
        assert_eq!(current(&grid), Some(PointerShape::Text));
    }
}

#[test]
fn setters_wait_for_complete_st_and_cancelled_requests_do_not_apply() {
    let mut grid = stream();
    grid.process(b"\x1b]22;pointer\x1b");
    assert_eq!(current(&grid), None);
    grid.process(b"\\");
    assert_eq!(current(&grid), Some(PointerShape::Pointer));
    for cancelled in [
        b"\x1b]22;text\x18".as_slice(),
        b"\x1b]22;text\x1a",
        b"\x1b]22;text\x1b[0m",
    ] {
        grid.process(cancelled);
        assert_eq!(current(&grid), Some(PointerShape::Pointer));
    }
}

#[test]
fn tracker_and_grid_agree_on_stacks_screens_and_reset() {
    let mut grid = stream();
    let mut tracker = TerminalProtocolTracker::new();
    let chunks: &[&[u8]] = &[
        b"\x1b]22;>text,pointer\x07",
        b"\x1b]22;=\x07",
        b"\x1b]22;<ignored\x07",
        b"\x1b[?1049h",
        b"\x1b]22;>wait,grab\x1b",
        b"\\",
        b"\x1b[?1049l",
        b"\x1b[?1049h",
        b"\x1b]22;<\x07",
        b"\x1bc",
    ];
    for chunk in chunks {
        grid.process(chunk);
        let _ = tracker.process(chunk);
        assert_eq!(tracker.protocol_state(), grid.grid().protocol_state());
        assert_eq!(
            tracker.alternate_screen(),
            grid.grid().mode() == crate::GridMode::Alternate
        );
    }
    assert_eq!(
        grid.grid().protocol_state().pointer_shapes,
        PointerShapeState::default()
    );
}

#[test]
fn snapshot_and_tracker_restore_then_pop_preserve_unset_and_inactive_stack() {
    let mut producer = stream();
    producer.process(b"\x1b]22;>text,pointer\x07\x1b]22;\x07\x1b[?1049h\x1b]22;>wait,grab\x07");
    let snapshot = producer.snapshot(0, 3);
    let encoded = serde_json::to_vec(&snapshot).unwrap();
    let snapshot = serde_json::from_slice(&encoded).unwrap();
    let mut restored = TerminalGridStream::from_snapshot(&snapshot, GridLimits::default()).unwrap();
    let mut tracker = TerminalProtocolTracker::new();
    tracker.restore(snapshot.protocol, true, &snapshot.pending_bytes);
    for (bytes, expected) in [
        (b"\x1b]22;<\x07".as_slice(), Some(PointerShape::Wait)),
        (b"\x1b[?1049l", None),
        (b"\x1b]22;<\x07", Some(PointerShape::Text)),
    ] {
        restored.process(bytes);
        let _ = tracker.process(bytes);
        assert_eq!(current(&restored), expected);
        assert_eq!(restored.grid().protocol_state(), tracker.protocol_state());
    }
}

#[test]
fn every_split_snapshot_restores_pending_set_push_pop_and_query() {
    let requests: &[&[u8]] = &[
        b"\x1b]22;>wait,pointer\x1b\\",
        b"\x1b]22;\x07",
        b"\x1b]22;<\x1b\\",
        b"\x1b]22;?__current__,text\x1b\\",
    ];
    for request in requests {
        for split in 0..=request.len() {
            let mut producer = stream();
            producer.process(b"\x1b]22;>text,grab\x07");
            producer.process(&request[..split]);
            let snapshot = producer.snapshot(0, 3);
            let mut restored =
                TerminalGridStream::from_snapshot(&snapshot, GridLimits::default()).unwrap();
            let mut tracker = TerminalProtocolTracker::new();
            tracker.restore(snapshot.protocol, false, &snapshot.pending_bytes);
            let mut expected_queries = Vec::new();
            let mut actual_queries = Vec::new();
            producer.process_with_pointer_queries(&request[split..], |query, shape| {
                expected_queries.push((query.to_vec(), shape));
            });
            restored.process_with_pointer_queries(&request[split..], |query, shape| {
                actual_queries.push((query.to_vec(), shape));
            });
            let _ = tracker.process(&request[split..]);
            assert_eq!(tracker.protocol_state(), restored.grid().protocol_state());
            assert_eq!(actual_queries, expected_queries, "split {split}");
            assert_eq!(
                restored.snapshot(0, 3),
                producer.snapshot(0, 3),
                "split {split}"
            );
        }
    }
}

#[test]
fn deltas_preserve_stack_only_changes_and_pending_payloads() {
    let limits = GridLimits::default();
    let mut producer = stream();
    let mut consumer = stream();
    let content_revision = producer.grid().content_revision();
    for chunk in [
        b"\x1b]22;>text\x07".as_slice(),
        b"\x1b]22;>text\x07",
        b"\x1b]22;>wait,",
        b"pointer\x1b",
        b"\\",
    ] {
        let delta = producer
            .process_delta(chunk)
            .expect("replicated state changed");
        assert!(delta.row_updates.is_empty());
        assert_eq!(delta.content_revision, content_revision);
        let wire = serde_json::to_vec(&delta).unwrap();
        let decoded = serde_json::from_slice(&wire).unwrap();
        consumer.apply_delta(&decoded, limits).unwrap();
        assert_eq!(consumer.snapshot(0, 3), producer.snapshot(0, 3));
    }
    for expected in [
        Some(PointerShape::Wait),
        Some(PointerShape::Text),
        Some(PointerShape::Text),
        None,
    ] {
        consumer.process(b"\x1b]22;<\x07");
        assert_eq!(current(&consumer), expected);
    }
    let revision = producer.grid().revision();
    producer.process_with_pointer_queries(b"\x1b]22;?__current__\x07", |_, _| {});
    producer.process(b"\x1b]22;unknown\x07");
    producer.process(b"\x1b]22;pointer\x07");
    assert_eq!(producer.grid().revision(), revision);
}

#[test]
fn oversized_requests_are_discarded_whole_even_after_hydration() {
    for query in [false, true] {
        let mut prefix = if query {
            b"\x1b]22;?__current__,".to_vec()
        } else {
            b"\x1b]22;>pointer,".to_vec()
        };
        prefix.extend(std::iter::repeat_n(b'x', 100_000));
        let mut producer = stream();
        producer.process(&prefix);
        let snapshot = producer.snapshot(0, 3);
        assert!(snapshot.pending_bytes.len() < 2048);
        let mut restored =
            TerminalGridStream::from_snapshot(&snapshot, GridLimits::default()).unwrap();
        for grid in [&mut producer, &mut restored] {
            let mut queries = 0;
            grid.process_with_pointer_queries(b"\x1b\\", |_, _| queries += 1);
            assert_eq!(queries, 0);
            assert_eq!(current(grid), None);
            grid.process(b"\x1b]22;text\x07");
            assert_eq!(current(grid), Some(PointerShape::Text));
        }
    }
}

#[test]
fn bounded_stacks_evict_oldest_entry_through_both_parser_paths() {
    let mut grid = stream();
    let mut tracker = TerminalProtocolTracker::new();
    let mut bytes = b"\x1b]22;>text".to_vec();
    for _ in 0..16 {
        bytes.extend_from_slice(b",pointer");
    }
    bytes.push(0x07);
    grid.process(&bytes);
    let _ = tracker.process(&bytes);
    assert_eq!(
        grid.grid()
            .protocol_state()
            .pointer_shapes
            .stack(false)
            .len(),
        16
    );
    for remaining in (0..16).rev() {
        grid.process(b"\x1b]22;<\x07");
        let _ = tracker.process(b"\x1b]22;<\x07");
        assert_eq!(grid.grid().protocol_state(), tracker.protocol_state());
        assert_eq!(
            current(&grid),
            if remaining == 0 {
                None
            } else {
                Some(PointerShape::Pointer)
            }
        );
    }
}

#[test]
fn malformed_and_parameter_overflow_requests_never_execute_valid_prefixes() {
    let mut grid = stream();
    for request in [
        b"\x1b]22;>pointer;ignored\x07".as_slice(),
        b"\x1b]22;>pointer\n\x07",
        b"\x1b]22;>pointer;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;;\x07",
        b"\x1b]22;?__current__;ignored\x07",
    ] {
        let mut queries = 0;
        grid.process_with_pointer_queries(request, |_, _| queries += 1);
        assert_eq!(queries, 0);
        assert_eq!(current(&grid), None);
    }
}

#[test]
fn state_wire_rejects_unknown_versions_corrupt_stacks_and_shapes() {
    fn assert_copy<T: Copy>() {}
    assert_copy::<ProtocolState>();
    let empty: ProtocolState = serde_json::from_str("{}").unwrap();
    assert_eq!(empty.pointer_shapes, PointerShapeState::default());
    let mut grid = stream();
    grid.process(b"\x1b]22;>text,pointer\x07");
    let protocol = grid.grid().protocol_state();
    let wire = serde_json::to_value(protocol).unwrap();
    assert_eq!(
        serde_json::from_value::<ProtocolState>(wire.clone()).unwrap(),
        protocol
    );
    for (pointer, value) in [
        ("/pointer_shapes/version", serde_json::json!(2)),
        ("/pointer_shapes/main/len", serde_json::json!(17)),
        ("/pointer_shapes/main/entries/15", serde_json::json!("wait")),
        (
            "/pointer_shapes/main/entries/0",
            serde_json::json!("unsupported"),
        ),
        ("/pointer_shapes/main/entries", serde_json::json!([])),
    ] {
        let mut corrupt = wire.clone();
        *corrupt.pointer_mut(pointer).unwrap() = value;
        assert!(
            serde_json::from_value::<ProtocolState>(corrupt).is_err(),
            "{pointer}"
        );
    }
    for shape in PointerShape::ALL {
        let encoded = serde_json::to_value(shape).unwrap();
        assert_eq!(encoded, serde_json::json!(shape.as_str()));
    }
}
