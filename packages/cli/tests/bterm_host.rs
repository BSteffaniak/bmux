//! Public native host lifecycle/backpressure contracts, without a daemon/window.
//!
//! Routing/reducer rollback tests require the owner-private attach state fixture;
//! these tests ensure constructing and closing the actual host has no output side
//! effects and that its public queue never reports a dropped event as accepted.
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use bmux_cli::attach::{NativeAttachEvent, NativeAttachHost};

#[test]
fn constructing_invalid_viewports_rejects_before_any_output() {
    for (columns, rows) in [(0, 24), (80, 0), (0, 0)] {
        assert!(
            NativeAttachHost::channel(columns, rows, 1, |_| {
                panic!("invalid host must not invoke presentation")
            })
            .is_err()
        );
    }
}

#[test]
fn host_construction_and_drop_never_acknowledge_an_output() {
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&accepted);
    let (handle, host) = NativeAttachHost::channel(80, 24, 1, move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(())
    })
    .unwrap();
    assert_eq!(accepted.load(Ordering::SeqCst), 0);
    drop(host);
    assert!(handle.try_send(NativeAttachEvent::Close).is_err());
    assert_eq!(accepted.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cloned_input_handles_observe_receiver_closure_for_both_send_paths() {
    let (handle, host) =
        NativeAttachHost::channel(80, 24, 1, |_| panic!("no composition has been requested"))
            .unwrap();
    let clone = handle.clone();
    drop(host);
    assert!(handle.try_send(NativeAttachEvent::Close).is_err());
    assert!(clone.send(NativeAttachEvent::Close).await.is_err());
}

#[tokio::test]
async fn saturated_input_queue_applies_backpressure_and_drop_releases_waiter() {
    let (handle, host) = NativeAttachHost::channel(80, 24, 1, |_| {
        panic!("queue admission must not invoke presentation")
    })
    .unwrap();
    let mut accepted = 0;
    while handle.try_send(NativeAttachEvent::Close).is_ok() {
        accepted += 1;
        assert!(accepted < 65_536, "input queue must be bounded");
    }
    assert!(accepted > 0);
    let send = handle.send(NativeAttachEvent::Close);
    tokio::pin!(send);
    // A full queue must not silently drop or report successful delivery.
    tokio::select! {
        biased;
        result = &mut send => panic!("full queue did not backpressure: {result:?}"),
        () = std::future::ready(()) => {}
    }
    drop(host);
    assert!(send.await.is_err());
}
