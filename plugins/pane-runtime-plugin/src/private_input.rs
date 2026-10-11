//! Pane-owned raw input capture barrier. Suppression is absorbing until pane teardown.
//!
//! A successful acquisition proves only BMUX raw-pane recording suppression. It
//! does not attest to external recorders, or permit a frontend to skip its ingress
//! drain/focus barrier. No timer automatically re-enables recording.
use std::collections::BTreeSet;
use std::sync::{Mutex, OnceLock};
use uuid::Uuid;

static SUPPRESSED: OnceLock<Mutex<BTreeSet<Uuid>>> = OnceLock::new();

fn registry() -> &'static Mutex<BTreeSet<Uuid>> {
    SUPPRESSED.get_or_init(|| Mutex::new(BTreeSet::new()))
}

/// Serialize acknowledgment with every raw-input recording copy.
pub(crate) fn suppress(pane_id: Uuid) -> bool {
    registry().lock().is_ok_and(|mut panes| {
        if !panes.contains(&pane_id) && panes.len() >= 4096 {
            return false;
        }
        panes.insert(pane_id);
        true
    })
}

/// Hold the barrier through copying and recording. Poison fails closed.
pub(crate) fn with_capture_barrier<T>(pane_id: Uuid, input: impl FnOnce(bool) -> T) -> Option<T> {
    let panes = registry().lock().ok()?;
    let result = input(!panes.contains(&pane_id));
    drop(panes);
    Some(result)
}

#[cfg(test)]
fn while_capture_allowed<T>(pane_id: Uuid, record: impl FnOnce() -> T) -> Option<T> {
    with_capture_barrier(pane_id, |capture| capture.then(record)).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn acknowledgment_serializes_after_inflight_recording() {
        let pane = Uuid::new_v4();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let recorder = std::thread::spawn(move || {
            with_capture_barrier(pane, |capture| {
                assert!(capture);
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })
        });
        entered_rx.recv().unwrap();
        let (ack_tx, ack_rx) = std::sync::mpsc::channel();
        let owner = std::thread::spawn(move || {
            ack_tx.send(suppress(pane)).unwrap();
        });
        assert!(matches!(
            ack_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        release_tx.send(()).unwrap();
        assert_eq!(recorder.join().unwrap(), Some(()));
        assert!(ack_rx.recv().unwrap());
        owner.join().unwrap();
        assert_eq!(with_capture_barrier(pane, |capture| capture), Some(false));
    }

    #[test]
    fn barrier_allows_private_delivery_without_creating_recording_copy() {
        let pane = Uuid::new_v4();
        assert!(suppress(pane));
        let mut delivered = false;
        let mut copied = false;
        assert_eq!(
            with_capture_barrier(pane, |capture| {
                delivered = true;
                if capture {
                    copied = true;
                }
                7
            }),
            Some(7)
        );
        assert!(delivered);
        assert!(!copied);
    }

    #[test]
    fn acknowledged_suppression_prevents_all_future_recording_copies() {
        let pane = Uuid::new_v4();
        assert_eq!(while_capture_allowed(pane, || 1), Some(1));
        assert!(suppress(pane));
        assert_eq!(
            while_capture_allowed(pane, || panic!("must not copy private input")),
            None::<()>
        );
        assert!(suppress(pane));
        assert_eq!(while_capture_allowed(pane, || 1), None);
    }
}
