//! Ephemeral HUMAN input epochs. No durable admission or automatic effect replay.
//!
//! The per-stream lock spans one PTY acknowledgment; the bounded registry lock
//! never does. Concurrent requests fail busy rather than accumulating waiters.
use bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::{
    TerminalCaptureIntegrity, TerminalExecutionPhase, TerminalExecutionRef,
};
use bmux_pane_runtime_plugin_api::pane_runtime_human_input_v1::{
    HumanInputAck, HumanInputCapabilities, HumanInputError, HumanInputStream,
};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock, TryLockError};
use std::time::{Duration, Instant};
use uuid::Uuid;

const MAX_PAYLOAD_BYTES: usize = 16 * 1024;
const MAX_ACTIVE_STREAMS: usize = 32;
const DEDUP_WINDOW: usize = 32;
// Lost open responses/disconnects must not permanently exhaust the registry.
// Expiry invalidates an idle epoch; clients must never replay its pending bytes.
const IDLE_EXPIRY: Duration = Duration::from_mins(30);

static STREAMS: OnceLock<Registry> = OnceLock::new();

#[derive(Default)]
struct Registry {
    streams: Mutex<BTreeMap<Uuid, Arc<Mutex<StreamState>>>>,
}

struct StreamState {
    stream: HumanInputStream,
    next_sequence: u64,
    closed: bool,
    touched: Instant,
    retained: VecDeque<RetainedOutcome>,
}

struct RetainedOutcome {
    sequence: u64,
    data: Vec<u8>,
    ack: HumanInputAck,
}

fn closed(reason: &str) -> HumanInputError {
    HumanInputError::Closed {
        reason: reason.into(),
    }
}

fn busy() -> HumanInputError {
    HumanInputError::Capacity {
        reason: "stream has an outstanding request; do not automatically retry input".into(),
    }
}

impl Registry {
    fn open(&self, execution: TerminalExecutionRef) -> Result<HumanInputStream, HumanInputError> {
        let mut streams = self
            .streams
            .lock()
            .map_err(|_| closed("registry poisoned"))?;
        streams.retain(|_, entry| match entry.try_lock() {
            Ok(mut state) => {
                if state.touched.elapsed() >= IDLE_EXPIRY {
                    state.closed = true;
                }
                !state.closed
            }
            Err(TryLockError::WouldBlock) => true,
            Err(TryLockError::Poisoned(_)) => false,
        });
        if streams.len() >= MAX_ACTIVE_STREAMS {
            return Err(HumanInputError::Capacity {
                reason: "active HUMAN input stream capacity exhausted".into(),
            });
        }
        let stream_id = loop {
            let candidate = Uuid::new_v4();
            if !streams.contains_key(&candidate) {
                break candidate;
            }
        };
        let stream = HumanInputStream {
            execution,
            stream_id,
        };
        streams.insert(
            stream_id,
            Arc::new(Mutex::new(StreamState {
                stream: stream.clone(),
                next_sequence: 1,
                closed: false,
                touched: Instant::now(),
                retained: VecDeque::new(),
            })),
        );
        Ok(stream)
    }

    fn lookup(&self, id: Uuid) -> Result<Arc<Mutex<StreamState>>, HumanInputError> {
        self.streams
            .lock()
            .map_err(|_| closed("registry poisoned"))?
            .get(&id)
            .cloned()
            .ok_or_else(|| closed("unknown, closed, or previous-runtime stream epoch"))
    }

    fn remove(&self, id: Uuid) -> Result<(), HumanInputError> {
        self.streams
            .lock()
            .map_err(|_| closed("registry poisoned"))?
            .remove(&id);
        Ok(())
    }

    fn push_with(
        &self,
        stream: &HumanInputStream,
        sequence: u64,
        data: Vec<u8>,
        write: impl FnOnce(&TerminalExecutionRef, Vec<u8>) -> Result<(), String>,
    ) -> Result<HumanInputAck, HumanInputError> {
        if data.is_empty() || data.len() > MAX_PAYLOAD_BYTES {
            return Err(HumanInputError::Invalid {
                reason: "HUMAN input payload must contain 1..=16384 bytes".into(),
            });
        }
        let entry = self.lookup(stream.stream_id)?;
        let mut state = match entry.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => return Err(busy()),
            Err(TryLockError::Poisoned(_)) => {
                self.remove(stream.stream_id)?;
                return Err(closed(
                    "stream owner failed; outstanding effects are uncertain",
                ));
            }
        };
        state.validate(stream)?;
        if state.touched.elapsed() >= IDLE_EXPIRY {
            state.closed = true;
            self.remove(stream.stream_id)?;
            return Err(closed("idle stream epoch expired; do not replay input"));
        }
        state.touched = Instant::now();
        if let Some(retained) = state.retained.iter().find(|item| item.sequence == sequence) {
            return if retained.data == data {
                Ok(retained.ack.clone())
            } else {
                Err(HumanInputError::Conflict {
                    reason: "sequence reused with different payload".into(),
                })
            };
        }
        if sequence != state.next_sequence || sequence == u64::MAX {
            return Err(HumanInputError::Sequence {
                reason: format!(
                    "expected sequence {}; old evicted outcomes cannot be replayed",
                    state.next_sequence
                ),
            });
        }
        let written_bytes = u64::try_from(data.len()).map_err(|_| HumanInputError::Invalid {
            reason: "input length cannot be represented".into(),
        })?;
        if let Err(reason) = write(&stream.execution, data.clone()) {
            // Even a timeout may leave input queued. Invalidate the epoch before
            // releasing the per-stream lock; never repeat an ambiguous effect.
            state.closed = true;
            self.remove(stream.stream_id)?;
            return Err(HumanInputError::Uncertain { reason });
        }
        let ack = HumanInputAck {
            stream: stream.clone(),
            sequence,
            written_bytes,
        };
        state.next_sequence += 1;
        if state.retained.len() == DEDUP_WINDOW {
            state.retained.pop_front();
        }
        state.retained.push_back(RetainedOutcome {
            sequence,
            data,
            ack: ack.clone(),
        });
        Ok(ack)
    }

    fn close(&self, stream: &HumanInputStream) -> Result<(), HumanInputError> {
        let entry = self.lookup(stream.stream_id)?;
        let mut state = match entry.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => return Err(busy()),
            Err(TryLockError::Poisoned(_)) => {
                self.remove(stream.stream_id)?;
                return Err(closed(
                    "stream owner failed; outstanding effects are uncertain",
                ));
            }
        };
        state.validate(stream)?;
        state.closed = true;
        self.remove(stream.stream_id)
    }
}

impl StreamState {
    fn validate(&self, stream: &HumanInputStream) -> Result<(), HumanInputError> {
        if self.stream.execution != stream.execution {
            return Err(HumanInputError::Conflict {
                reason: "stream belongs to a different exact execution".into(),
            });
        }
        if self.closed {
            return Err(closed("stream epoch closed"));
        }
        Ok(())
    }
}

pub(crate) fn capabilities() -> HumanInputCapabilities {
    HumanInputCapabilities {
        protocol_version: 1,
        max_payload_bytes: 16384,
        max_active_streams: 32,
        dedup_window: 32,
    }
}

fn validate_live(execution: &TerminalExecutionRef) -> Result<(), HumanInputError> {
    let status = super::execution::status(execution)
        .map_err(|error| HumanInputError::Authority { error })?;
    if !matches!(status.phase, TerminalExecutionPhase::Running)
        || status.terminal_session_ended
        || status.error.is_some()
        || matches!(
            status.capture_integrity,
            TerminalCaptureIntegrity::Incomplete
        )
    {
        return Err(closed("execution is not accepting HUMAN input"));
    }
    Ok(())
}

pub(crate) fn open(execution: TerminalExecutionRef) -> Result<HumanInputStream, HumanInputError> {
    validate_live(&execution)?;
    STREAMS.get_or_init(Registry::default).open(execution)
}

pub(crate) fn push(
    stream: &HumanInputStream,
    sequence: u64,
    data: Vec<u8>,
) -> Result<HumanInputAck, HumanInputError> {
    let registry = STREAMS.get_or_init(Registry::default);
    if let Err(error) = validate_live(&stream.execution) {
        // This is observation, not durable reconciliation. No disk I/O occurs.
        // Invalidity never authorizes replay, including retained duplicate keys.
        if let Ok(entry) = registry.lookup(stream.stream_id)
            && let Ok(mut state) = entry.try_lock()
            && state.stream.execution == stream.execution
        {
            state.closed = true;
            registry.remove(stream.stream_id)?;
        }
        return Err(error);
    }
    registry.push_with(stream, sequence, data, |execution, bytes| {
        // The effect owner records successful PTY writes before acknowledging.
        // This closure runs only for a fresh sequence, never retained outcomes.
        // Its error may follow a PTY effect or failed capture enqueue: close the
        // epoch as uncertain instead of retrying either write or input capture.
        super::execution::send_human_recorded_acknowledged(execution, bytes)
            .map_err(|error| format!("{error:?}"))
    })
}

/// Closes only volatile input state; recording finalization must drain separately.
pub(crate) fn close(stream: &HumanInputStream) -> Result<(), HumanInputError> {
    STREAMS.get_or_init(Registry::default).close(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn execution() -> TerminalExecutionRef {
        TerminalExecutionRef {
            session_id: Uuid::new_v4(),
            pane_id: Uuid::new_v4(),
            execution_token: Uuid::new_v4(),
        }
    }

    fn unexpected_write(_: &TerminalExecutionRef, _: Vec<u8>) -> Result<(), String> {
        panic!("rejected or duplicate request must not write")
    }

    #[test]
    fn recorded_effect_runs_once_and_duplicate_does_not_reenqueue_capture() {
        let registry = Registry::default();
        let stream = registry.open(execution()).unwrap();
        let mut writes = Vec::new();
        let mut recorded = Vec::new();
        let ack = registry
            .push_with(&stream, 1, b"key".to_vec(), |_, bytes| {
                // Models the coordinator helper's ordered PTY-write then bounded
                // event enqueue contract, not a durable recording acknowledgment.
                writes.push(bytes.clone());
                recorded.push(bytes);
                Ok(())
            })
            .unwrap();
        let duplicate = registry
            .push_with(&stream, 1, b"key".to_vec(), unexpected_write)
            .unwrap();
        assert_eq!(ack, duplicate);
        assert_eq!(writes, [b"key".to_vec()]);
        assert_eq!(recorded, writes);
        assert!(matches!(
            registry.push_with(&stream, 1, b"changed".to_vec(), unexpected_write),
            Err(HumanInputError::Conflict { .. })
        ));
    }

    #[test]
    fn capture_enqueue_failure_after_write_closes_epoch_without_repeating_effect() {
        let registry = Registry::default();
        let stream = registry.open(execution()).unwrap();
        let mut writes = Vec::new();
        let result = registry.push_with(&stream, 1, b"key".to_vec(), |_, bytes| {
            writes.push(bytes);
            Err("PTY written; recording input enqueue failed".into())
        });
        assert!(matches!(result, Err(HumanInputError::Uncertain { reason })
            if reason.contains("recording input enqueue failed")));
        assert!(matches!(
            registry.push_with(&stream, 1, b"key".to_vec(), unexpected_write),
            Err(HumanInputError::Closed { .. })
        ));
        assert_eq!(writes, [b"key".to_vec()]);
        assert!(registry.streams.lock().unwrap().is_empty());
    }

    #[test]
    fn duplicate_retains_outcome_conflict_and_gaps_do_not_write() {
        let registry = Registry::default();
        let stream = registry.open(execution()).unwrap();
        let writes = AtomicUsize::new(0);
        let ack = registry
            .push_with(&stream, 1, b"hello".to_vec(), |target, bytes| {
                assert_eq!(*target, stream.execution);
                assert_eq!(bytes, b"hello");
                writes.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .unwrap();
        let duplicate = registry
            .push_with(&stream, 1, b"hello".to_vec(), unexpected_write)
            .unwrap();
        assert_eq!(ack.sequence, duplicate.sequence);
        assert_eq!(ack.written_bytes, duplicate.written_bytes);
        assert_eq!(ack.stream, duplicate.stream);
        assert_eq!(writes.load(Ordering::SeqCst), 1);
        assert!(matches!(
            registry.push_with(&stream, 1, b"other".to_vec(), unexpected_write),
            Err(HumanInputError::Conflict { .. })
        ));
        for sequence in [0, 3, u64::MAX] {
            assert!(matches!(
                registry.push_with(&stream, sequence, vec![1], unexpected_write),
                Err(HumanInputError::Sequence { .. })
            ));
        }
    }

    #[test]
    fn payload_and_dedup_memory_are_bounded() {
        let registry = Registry::default();
        let stream = registry.open(execution()).unwrap();
        for bytes in [Vec::new(), vec![0; MAX_PAYLOAD_BYTES + 1]] {
            assert!(matches!(
                registry.push_with(&stream, 1, bytes, unexpected_write),
                Err(HumanInputError::Invalid { .. })
            ));
        }
        for sequence in 1..=33 {
            registry
                .push_with(&stream, sequence, vec![0; MAX_PAYLOAD_BYTES], |_, _| Ok(()))
                .unwrap();
        }
        assert!(matches!(
            registry.push_with(&stream, 1, vec![0; MAX_PAYLOAD_BYTES], unexpected_write),
            Err(HumanInputError::Sequence { .. })
        ));
        let entry = registry.lookup(stream.stream_id).unwrap();
        assert_eq!(entry.lock().unwrap().retained.len(), DEDUP_WINDOW);
        registry
            .push_with(&stream, 33, vec![0; MAX_PAYLOAD_BYTES], unexpected_write)
            .unwrap();
    }

    #[test]
    fn poisoned_owner_invalidates_epoch_without_replay() {
        let registry = Registry::default();
        let stream = registry.open(execution()).unwrap();
        let result = std::panic::catch_unwind(|| {
            let _ = registry.push_with(&stream, 1, vec![1], |_, _| {
                panic!("owner lost during write");
            });
        });
        assert!(result.is_err());
        assert!(matches!(
            registry.push_with(&stream, 1, vec![1], unexpected_write),
            Err(HumanInputError::Closed { .. })
        ));
        assert!(registry.streams.lock().unwrap().is_empty());
    }

    #[test]
    fn uncertain_effect_invalidates_epoch_without_replay() {
        let registry = Registry::default();
        let stream = registry.open(execution()).unwrap();
        assert!(matches!(
            registry.push_with(&stream, 1, vec![1], |_, _| Err("timeout".into())),
            Err(HumanInputError::Uncertain { .. })
        ));
        assert!(matches!(
            registry.push_with(&stream, 1, vec![1], unexpected_write),
            Err(HumanInputError::Closed { .. })
        ));
        assert!(registry.streams.lock().unwrap().is_empty());
    }

    #[test]
    fn exact_execution_close_restart_and_capacity_are_enforced() {
        let registry = Registry::default();
        let stream = registry.open(execution()).unwrap();
        let mut wrong = stream.clone();
        wrong.execution.execution_token = Uuid::new_v4();
        assert!(matches!(
            registry.push_with(&wrong, 1, vec![1], unexpected_write),
            Err(HumanInputError::Conflict { .. })
        ));
        assert!(registry.close(&wrong).is_err());
        assert!(Registry::default().close(&stream).is_err());
        for _ in 1..MAX_ACTIVE_STREAMS {
            registry.open(execution()).unwrap();
        }
        assert!(matches!(
            registry.open(execution()),
            Err(HumanInputError::Capacity { .. })
        ));
        registry.close(&stream).unwrap();
        assert!(registry.close(&stream).is_err());
        registry.open(execution()).unwrap();
        assert!(matches!(
            registry.push_with(&stream, 1, vec![1], unexpected_write),
            Err(HumanInputError::Closed { .. })
        ));
    }

    #[test]
    fn idle_epochs_release_capacity_and_cannot_replay() {
        let registry = Registry::default();
        let stream = registry.open(execution()).unwrap();
        for _ in 1..MAX_ACTIVE_STREAMS {
            registry.open(execution()).unwrap();
        }
        let entry = registry.lookup(stream.stream_id).unwrap();
        entry.lock().unwrap().touched = Instant::now().checked_sub(IDLE_EXPIRY).unwrap();
        registry.open(execution()).unwrap();
        assert!(matches!(
            registry.push_with(&stream, 1, vec![1], unexpected_write),
            Err(HumanInputError::Closed { .. })
        ));
        assert_eq!(registry.streams.lock().unwrap().len(), MAX_ACTIVE_STREAMS);
    }

    #[test]
    fn pty_wait_does_not_hold_registry_and_concurrent_requests_fail_busy() {
        let registry = Registry::default();
        let stream = registry.open(execution()).unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        std::thread::scope(|scope| {
            let worker_registry = &registry;
            let worker_stream = &stream;
            let worker = scope.spawn(move || {
                worker_registry.push_with(worker_stream, 1, vec![1], |_, _| {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                })
            });
            started_rx.recv().unwrap();
            assert!(matches!(
                registry.push_with(&stream, 1, vec![1], unexpected_write),
                Err(HumanInputError::Capacity { .. })
            ));
            assert!(matches!(
                registry.close(&stream),
                Err(HumanInputError::Capacity { .. })
            ));
            let other = registry.open(execution()).unwrap();
            registry
                .push_with(&other, 1, vec![2], |_, _| Ok(()))
                .unwrap();
            registry.close(&other).unwrap();
            release_tx.send(()).unwrap();
            worker.join().unwrap().unwrap();
        });
    }
}
