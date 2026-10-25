use super::*;

#[test]
fn generations_reject_queued_typing_paste_and_completion_across_both_barriers() {
    let generation = Generation {
        value: AtomicU64::new(0),
        paused: AtomicBool::new(false),
    };
    assert!(generation.current(0));
    generation.fence().unwrap();
    assert!(!generation.current(0));
    assert!(!generation.current(1));
    generation.paused.store(false, Ordering::SeqCst);
    assert!(generation.current(1));
    generation.fence().unwrap();
    assert!(!generation.current(1));
    generation.paused.store(false, Ordering::SeqCst);
    assert!(generation.current(2));
}

#[test]
fn decoded_event_crossing_entry_fence_never_enters_the_new_private_buffer() {
    struct Capture;
    impl CaptureSuppression for Capture {
        fn suppress(&mut self) -> Result<(), PrivateInputError> {
            Ok(())
        }
        fn drain_and_resume(&mut self) -> Result<(), PrivateInputError> {
            Ok(())
        }
    }
    let generation = Generation {
        value: AtomicU64::new(0),
        paused: AtomicBool::new(false),
    };
    let gate = PrivateInputGate::new(Capture);
    let lease = gate.acquire().unwrap();
    generation.fence().unwrap();
    generation.paused.store(false, Ordering::SeqCst);
    assert!(matches!(
        gate.route_checked(
            bmux_tui::event::Event::Paste("stale-private-canary".into()),
            || generation.current(0)
        ),
        PrivateInputRoute::Consumed
    ));
    assert!(matches!(
        gate.route(bmux_tui::event::Event::Key(
            bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Enter)
        )),
        PrivateInputRoute::Consumed
    ));
    assert!(matches!(
        gate.route_checked(
            bmux_tui::event::Event::Paste("fresh-private-canary".into()),
            || generation.current(1)
        ),
        PrivateInputRoute::Consumed
    ));
    let PrivateInputRoute::Submitted(bytes) = gate.route(bmux_tui::event::Event::Key(
        bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Enter),
    )) else {
        panic!("missing completion");
    };
    assert_eq!(bytes.expose(), b"fresh-private-canary");
    assert!(matches!(
        gate.route(bmux_tui::event::Event::Focus(
            bmux_tui::event::FocusEvent::Lost
        )),
        PrivateInputRoute::Consumed
    ));
    drop(lease);
    gate.drain_and_resume().unwrap();
}

#[test]
fn lost_ack_and_generation_exhaustion_keep_admission_paused() {
    let generation = Generation {
        value: AtomicU64::new(u64::MAX),
        paused: AtomicBool::new(false),
    };
    assert_eq!(generation.fence(), Err(PrivateInputError::BarrierFailed));
    assert!(!generation.current(u64::MAX));
    // A failed acknowledgment never runs the restoration branch.
    let generation = Generation {
        value: AtomicU64::new(0),
        paused: AtomicBool::new(false),
    };
    generation.fence().unwrap();
    assert!(!generation.current(0));
    assert!(!generation.current(1));
}
