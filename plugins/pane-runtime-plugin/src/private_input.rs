//! Pane-owned raw input capture barriers. Legacy suppression is absorbing; v2
//! restores only after matching producer acknowledgments and a FIFO PTY fence.
//!
//! A successful acquisition proves only BMUX raw-pane recording suppression. It
//! does not attest to external recorders, or permit a frontend to skip its ingress
//! drain/focus barrier. No timer automatically re-enables recording.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, OnceLock};
use uuid::Uuid;

static SUPPRESSED: OnceLock<Mutex<BTreeSet<Uuid>>> = OnceLock::new();

fn registry() -> &'static Mutex<BTreeSet<Uuid>> {
    SUPPRESSED.get_or_init(|| Mutex::new(BTreeSet::new()))
}

/// Serialize acknowledgment with every raw-input recording copy.
pub(crate) fn suppress(pane_id: Uuid) -> bool {
    let Ok(panes) = producers().lock() else {
        return false;
    };
    if panes
        .get(&pane_id)
        .is_some_and(|state| state.active_nonce.is_some() || state.blocked)
    {
        return false;
    }
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

#[derive(Default)]
struct ProducerState {
    members: BTreeSet<Uuid>,
    blocked: bool,
    active_nonce: Option<Uuid>,
    generation: u64,
    barrier: Option<ProducerBarrier>,
}
struct ProducerBarrier {
    nonce: Uuid,
    entering: bool,
    expected: BTreeSet<Uuid>,
    acknowledged: BTreeSet<Uuid>,
    denied: bool,
}
static PRODUCERS: OnceLock<Mutex<BTreeMap<Uuid, ProducerState>>> = OnceLock::new();
fn producers() -> &'static Mutex<BTreeMap<Uuid, ProducerState>> {
    PRODUCERS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Membership mutation invalidates any proof in flight; unknown producers cannot inherit it.
pub(crate) fn register_producer(pane: Uuid, client: Uuid) -> bool {
    register_producers(&[pane], client)
}

/// Admit all panes atomically: a denied pane must not leave partial membership.
pub(crate) fn register_producers(requested: &[Uuid], client: Uuid) -> bool {
    let Ok(mut panes) = producers().lock() else {
        return false;
    };
    let requested = requested.iter().copied().collect::<BTreeSet<_>>();
    let new_panes = requested
        .iter()
        .filter(|pane| !panes.contains_key(pane))
        .count();
    if panes.len() + new_panes > 4096
        || requested.iter().any(|pane| {
            panes.get(pane).is_some_and(|state| {
                !state.members.contains(&client)
                    && (state.active_nonce.is_some() || state.members.len() >= 256)
            })
        })
    {
        return false;
    }
    for pane in requested {
        let state = panes.entry(pane).or_default();
        if state.members.insert(client)
            && let Some(barrier) = &mut state.barrier
        {
            barrier.denied = true;
        }
    }
    true
}

/// Detachment revokes admission, never execution lifetime or private suppression.
pub(crate) fn unregister_producer(pane: Uuid, client: Uuid) {
    let Ok(mut panes) = producers().lock() else {
        return;
    };
    if let Some(state) = panes.get_mut(&pane)
        && state.members.remove(&client)
        && let Some(barrier) = &mut state.barrier
    {
        barrier.denied = true;
    }
}

pub(crate) fn begin_barrier(pane: Uuid, nonce: Uuid, entering: bool) -> Option<u64> {
    let mut panes = producers().lock().ok()?;
    let state = panes.get_mut(&pane)?;
    if state.barrier.is_some()
        || (entering && state.active_nonce.is_some())
        || (!entering && state.active_nonce != Some(nonce))
    {
        return None;
    }
    state.blocked = true;
    if entering {
        let Ok(mut suppressed) = registry().lock() else {
            return None;
        };
        // Never restore legacy/foreign suppression under a v2 receipt.
        if suppressed.contains(&pane) || suppressed.len() >= 4096 {
            return None;
        }
        suppressed.insert(pane);
    }
    state.generation = state.generation.checked_add(1)?;
    state.barrier = Some(ProducerBarrier {
        nonce,
        entering,
        expected: state.members.clone(),
        acknowledged: BTreeSet::new(),
        denied: state.members.is_empty(),
    });
    Some(state.generation)
}

pub(crate) fn pending_barriers(
    client: Uuid,
) -> Vec<bmux_pane_runtime_plugin_api::attach_runtime_commands::InputProducerBarrierV2> {
    let Ok(panes) = producers().lock() else {
        return Vec::new();
    };
    // Every registered producer remains listed until the complete barrier settles,
    // including after its own acknowledgment, so it stays paused until peers drain.
    panes
        .iter()
        .filter_map(|(pane, state)| {
            let barrier = state.barrier.as_ref()?;
            barrier.expected.contains(&client).then_some(
                bmux_pane_runtime_plugin_api::attach_runtime_commands::InputProducerBarrierV2 {
                    pane_id: *pane,
                    nonce: barrier.nonce,
                    generation: state.generation,
                    entering: barrier.entering,
                },
            )
        })
        .take(256)
        .collect()
}

pub(crate) fn acknowledge_producer(
    pane: Uuid,
    nonce: Uuid,
    generation: u64,
    client: Uuid,
    supported: bool,
) -> bool {
    let Ok(mut panes) = producers().lock() else {
        return false;
    };
    let Some(state) = panes.get_mut(&pane) else {
        return false;
    };
    if state.generation != generation {
        return false;
    }
    let Some(barrier) = &mut state.barrier else {
        return false;
    };
    if barrier.nonce != nonce || !barrier.expected.contains(&client) {
        return false;
    }
    if !supported {
        barrier.denied = true;
    }
    barrier.acknowledged.insert(client);
    !barrier.denied
}

pub(crate) fn barrier_ready(pane: Uuid, nonce: Uuid, generation: u64) -> bool {
    let Ok(panes) = producers().lock() else {
        return false;
    };
    let Some(state) = panes.get(&pane) else {
        return false;
    };
    state.generation == generation
        && state.barrier.as_ref().is_some_and(|barrier| {
            barrier.nonce == nonce
                && !barrier.denied
                && barrier.expected == state.members
                && barrier.acknowledged == barrier.expected
        })
}

/// Consume an exact producer proof. Failure keeps admission blocked and recording suppressed.
pub(crate) fn finish_barrier(pane: Uuid, nonce: Uuid, generation: u64) -> bool {
    let Ok(mut panes) = producers().lock() else {
        return false;
    };
    let Some(state) = panes.get_mut(&pane) else {
        return false;
    };
    let Some(barrier) = &state.barrier else {
        return false;
    };
    if generation != state.generation
        || nonce != barrier.nonce
        || barrier.denied
        || barrier.expected != state.members
        || barrier.acknowledged != barrier.expected
    {
        return false;
    }
    if !barrier.entering && !restore_recording(pane) {
        return false;
    }
    state.active_nonce = barrier.entering.then_some(nonce);
    state.barrier = None;
    state.blocked = false;
    true
}

pub(crate) fn restore_recording(pane: Uuid) -> bool {
    registry().lock().is_ok_and(|mut panes| panes.remove(&pane))
}

/// Hold admission proof until the input has been enqueued, serializing with barriers.
pub(crate) fn with_producer_admission<T>(
    pane: Uuid,
    client: Uuid,
    input: impl FnOnce() -> T,
) -> Option<T> {
    let panes = producers().lock().ok()?;
    let state = panes.get(&pane)?;
    if state.blocked || !state.members.contains(&client) {
        return None;
    }
    let result = input();
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
    fn multipane_admission_failure_does_not_register_other_panes() {
        let open = Uuid::new_v4();
        let private = Uuid::new_v4();
        let owner = Uuid::new_v4();
        let newcomer = Uuid::new_v4();
        let nonce = Uuid::new_v4();
        assert!(register_producer(private, owner));
        let generation = begin_barrier(private, nonce, true).unwrap();
        assert!(acknowledge_producer(
            private, nonce, generation, owner, true
        ));
        assert!(finish_barrier(private, nonce, generation));
        assert!(!register_producers(&[open, private], newcomer));
        assert!(with_producer_admission(open, newcomer, || ()).is_none());
        assert!(with_producer_admission(private, newcomer, || ()).is_none());
    }

    #[test]
    fn detached_producer_invalidates_inflight_proof_without_releasing_suppression() {
        let pane = Uuid::new_v4();
        let client = Uuid::new_v4();
        let nonce = Uuid::new_v4();
        assert!(register_producer(pane, client));
        let generation = begin_barrier(pane, nonce, true).unwrap();
        assert!(acknowledge_producer(pane, nonce, generation, client, true));
        unregister_producer(pane, client);
        assert!(!finish_barrier(pane, nonce, generation));
        assert!(with_producer_admission(pane, client, || ()).is_none());
        assert!(while_capture_allowed(pane, || ()).is_none());
    }

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
    fn producers_require_exact_membership_identity_nonce_and_generation() {
        let pane = Uuid::new_v4();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let nonce = Uuid::new_v4();
        assert!(register_producer(pane, a));
        assert!(register_producer(pane, b));
        let generation = begin_barrier(pane, nonce, true).unwrap();
        assert!(with_producer_admission(pane, a, || ()).is_none());
        assert!(!acknowledge_producer(
            pane,
            nonce,
            generation,
            Uuid::new_v4(),
            true
        ));
        assert!(!acknowledge_producer(pane, nonce, generation + 1, a, true));
        assert!(!acknowledge_producer(
            pane,
            Uuid::new_v4(),
            generation,
            a,
            true
        ));
        assert!(acknowledge_producer(pane, nonce, generation, a, true));
        assert!(!finish_barrier(pane, nonce, generation)); // lost peer acknowledgment
        assert!(acknowledge_producer(pane, nonce, generation, b, true));
        assert!(barrier_ready(pane, nonce, generation));
        assert!(finish_barrier(pane, nonce, generation));
        assert!(!finish_barrier(pane, nonce, generation));
        assert_eq!(with_capture_barrier(pane, |capture| capture), Some(false));
        assert!(!register_producer(pane, Uuid::new_v4()));
        assert!(!suppress(pane)); // legacy suppression cannot be restored by v2
        let exit = begin_barrier(pane, nonce, false).unwrap();
        assert!(!barrier_ready(pane, nonce, generation));
        assert!(acknowledge_producer(pane, nonce, exit, a, true));
        assert!(acknowledge_producer(pane, nonce, exit, b, true));
        assert!(finish_barrier(pane, nonce, exit));
        assert_eq!(with_capture_barrier(pane, |capture| capture), Some(true));
    }

    #[test]
    fn unsupported_or_changed_producers_cannot_release_suppression() {
        for changed in [false, true] {
            let pane = Uuid::new_v4();
            let client = Uuid::new_v4();
            let nonce = Uuid::new_v4();
            assert!(register_producer(pane, client));
            let generation = begin_barrier(pane, nonce, true).unwrap();
            if changed {
                assert!(register_producer(pane, Uuid::new_v4()));
            }
            acknowledge_producer(pane, nonce, generation, client, changed);
            assert!(!finish_barrier(pane, nonce, generation));
            assert_eq!(with_capture_barrier(pane, |capture| capture), Some(false));
            assert!(with_producer_admission(pane, client, || ()).is_none());
        }
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
