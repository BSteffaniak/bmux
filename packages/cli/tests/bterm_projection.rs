//! Public capture regressions shared by terminal and native consumers.
//!
//! These tests exercise the real synchronous lowering boundary, not encoded ANSI
//! or the browser transport. No display server or terminal writer is involved.
use bmux_attach_pipeline::projection::{self, PaintOperation, ProjectionFailure};
use bmux_tui::geometry::Rect;
use bmux_tui::style::Style;

fn fill() -> PaintOperation {
    PaintOperation::Fill {
        rect: Rect::new(2, 3, 7, 5),
        style: Style::default(),
    }
}

#[test]
fn neutral_capture_preserves_operations_without_enabling_terminal_encoding() {
    assert!(projection::encoding_terminal());
    let (value, neutral) = projection::capture_neutral(|| {
        assert!(!projection::encoding_terminal());
        projection::record(fill());
        42
    });
    let (_, terminal) = projection::capture(|| {
        assert!(projection::encoding_terminal());
        projection::record(fill());
    });
    assert_eq!(value, 42);
    assert_eq!(neutral, terminal);
    assert_eq!(neutral, vec![fill()]);
    assert!(projection::encoding_terminal());
    assert!(!projection::capturing());
}

#[test]
fn unwind_does_not_poison_the_next_output_capture() {
    let failure = std::panic::catch_unwind(|| {
        projection::capture_neutral(|| {
            projection::record(fill());
            panic!("simulated composition failure");
        });
    });
    assert!(failure.is_err());
    assert!(projection::encoding_terminal());
    assert!(!projection::capturing());
    let (_, operations) = projection::capture_neutral(|| projection::record(fill()));
    assert_eq!(operations, vec![fill()]);
}

#[test]
fn rejected_nested_capture_preserves_outer_capture_and_encoding_mode() {
    let (_, operations) = projection::capture_neutral(|| {
        projection::record(fill());
        assert!(std::panic::catch_unwind(|| projection::capture_neutral(|| ())).is_err());
        assert!(projection::capturing());
        assert!(!projection::encoding_terminal());
        projection::record(fill());
    });
    assert_eq!(operations, vec![fill(), fill()]);
    assert!(projection::encoding_terminal());
}

#[test]
fn capture_scope_does_not_leak_to_another_composition_thread() {
    let (_, operations) = projection::capture_neutral(|| {
        std::thread::spawn(|| {
            assert!(projection::encoding_terminal());
            assert!(!projection::capturing());
            projection::record(PaintOperation::Unsupported);
            let (_, other) = projection::capture_neutral(|| projection::record(fill()));
            assert_eq!(other, vec![fill()]);
        })
        .join()
        .expect("independent composition thread");
        projection::record(fill());
    });
    assert_eq!(operations, vec![fill()]);
}

#[test]
fn over_budget_capture_discards_partial_scene_before_allocating_payload() {
    let (_, operations) = projection::capture_neutral(|| {
        projection::record(fill());
        projection::record_with(usize::MAX, || {
            panic!("rejected payload must not be allocated")
        });
        projection::record(fill());
    });
    assert!(matches!(
        operations.as_slice(),
        [PaintOperation::Rejected(ProjectionFailure::Budget { .. })]
    ));
    let (_, next) = projection::capture_neutral(|| projection::record(fill()));
    assert_eq!(
        next,
        vec![fill()],
        "overflow must not poison subsequent frames"
    );
}
