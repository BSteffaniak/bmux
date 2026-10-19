//! Process-local retry retention for the native buffer-growth protocol.
//!
//! Completed outcomes (including failures) live for at least 60 seconds after
//! completion, even after successful delivery. No capacity eviction occurs during
//! that interval: admission fails closed instead. Expired entries are reclaimed
//! on admission; in-flight entries never expire. After expiry, an ID may execute
//! again. This is not durable idempotency across unload/restart or a replacement
//! for service-side authorization/idempotency. Callers must not retry after the
//! interval or treat a new invocation ID as a buffer-growth retry.
//!
//! The key is (plugin type, provider instance address, invocation ID). This is
//! the per-dispatch envelope ID, not the inherited cancellation-scope ID inside
//! the request context. The host allocates it once outside buffer-growth retries;
//! nested and sibling dispatches get new IDs without changing cancellation.
//! Export kind and the entire encoded request must match. Successful delivery retains bytes
//! too: without an explicit acknowledgement ABI, dropping them would break replay.
//! Defaults allow 65,536 outcomes per 60-second window (over 1,000 calls/second),
//! with 512 MiB retained/reserved payload bytes and 64 MiB per response. There is
//! no separate request-size restriction: decoding retains its existing transport
//! limits; cache admission is bounded by the aggregate byte budget. Each new call
//! reserves a 4 KiB response slot before execution, not the maximum possible
//! response (which would impose an eight-call nesting limit). Larger encoded
//! responses acquire their actual additional bytes atomically at settlement.
//! Oversized or over-budget responses settle as retained terminal encoding
//! failures; no success is delivered without retaining its exact bytes. Such a
//! failure can follow a side effect: it is not permission to retry the mutation
//! with a new ID. Response encoding itself remains bounded by the service codec
//! and per-response limit; aggregate accounting bounds retained cache storage,
//! not temporary allocations made inside provider code or the response encoder.
//!
//! Process startup may tune `BMUX_NATIVE_REPLAY_MAX_ENTRIES`,
//! `BMUX_NATIVE_REPLAY_MAX_BYTES` and `BMUX_NATIVE_REPLAY_RETENTION_MS` (positive
//! integers). These are read once by the global cache; invalid configuration
//! disables admission rather than weakening retention. Memory/count exhaustion
//! returns plugin-unavailable before execution, never evicts unexpired outcomes.
//! Metadata is bounded by entry count in addition to the payload byte budget.
//! Expiry uses a completion-ordered queue, not a full scan on every input call.
//! Calls require canonical current envelopes with explicit invocation IDs; old
//! pre-ID migration frames cannot safely participate in this retry protocol.
use std::any::TypeId;
use std::collections::{HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use super::{SERVICE_STATUS_INVALID_ARGUMENT, SERVICE_STATUS_PLUGIN_UNAVAILABLE};

const RETENTION: Duration = Duration::from_mins(1);
const MAX_ENTRIES: usize = 65_536;
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
const RESPONSE_SLOT_BYTES: usize = 4096;
const MAX_RETAINED_BYTES: usize = 512 * 1024 * 1024;

type EncodedResult = Result<Arc<[u8]>, i32>;

#[derive(Clone, Hash, PartialEq, Eq)]
struct Key {
    plugin: TypeId,
    instance: usize,
    invocation: crate::PluginInvocationId,
}

struct Outcome {
    result: EncodedResult,
}

struct Entry {
    export: u8,
    input: Vec<u8>,
    owner: ThreadId,
    outcome: OnceLock<Outcome>,
}

#[derive(Default)]
pub(super) struct Replay {
    state: Mutex<State>,
    limits: Limits,
}

struct Limits {
    entries: usize,
    bytes: usize,
    retention: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            entries: MAX_ENTRIES,
            bytes: MAX_RETAINED_BYTES,
            retention: RETENTION,
        }
    }
}

impl Limits {
    fn from_lookup(lookup: impl Fn(&str) -> Result<String, std::env::VarError>) -> Option<Self> {
        let number = |name: &str, default: usize| -> Option<usize> {
            match lookup(name) {
                Ok(value) => value.parse().ok().filter(|value| *value > 0),
                Err(std::env::VarError::NotPresent) => Some(default),
                Err(std::env::VarError::NotUnicode(_)) => None,
            }
        };
        Some(Self {
            entries: number("BMUX_NATIVE_REPLAY_MAX_ENTRIES", MAX_ENTRIES)?,
            bytes: number("BMUX_NATIVE_REPLAY_MAX_BYTES", MAX_RETAINED_BYTES)?,
            retention: Duration::from_millis(
                u64::try_from(number("BMUX_NATIVE_REPLAY_RETENTION_MS", 60_000)?).ok()?,
            ),
        })
    }
}

#[derive(Default)]
struct State {
    entries: HashMap<Key, Arc<Entry>>,
    completed: VecDeque<(Instant, Key)>,
    // In-flight entries reserve a small response slot before any side effect.
    bytes: usize,
}

impl Replay {
    pub(super) fn invoke<P: 'static>(
        &self,
        instance: usize,
        export: u8,
        invocation: crate::PluginInvocationId,
        input: &[u8],
        execute: impl FnOnce() -> Result<Vec<u8>, i32>,
    ) -> EncodedResult {
        let key = Key {
            plugin: TypeId::of::<P>(),
            instance,
            invocation,
        };
        let (entry, created) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| SERVICE_STATUS_PLUGIN_UNAVAILABLE)?;
            let now = Instant::now();
            while state.completed.front().is_some_and(|(completed, _)| {
                now.duration_since(*completed) >= self.limits.retention
            }) {
                let (_, expired) = state.completed.pop_front().expect("front checked");
                if let Some(entry) = state.entries.remove(&expired) {
                    let outcome = entry.outcome.get().expect("only completed entries expire");
                    state.bytes -=
                        entry.input.len() + outcome.result.as_ref().map_or(0, |bytes| bytes.len());
                }
            }
            let selected = if let Some(entry) = state.entries.get(&key) {
                if entry.input != input || entry.export != export {
                    return Err(SERVICE_STATUS_INVALID_ARGUMENT);
                }
                // Reject recursive re-entry rather than deadlocking on our own outcome.
                if entry.owner == thread::current().id() && entry.outcome.get().is_none() {
                    return Err(SERVICE_STATUS_PLUGIN_UNAVAILABLE);
                }
                (Arc::clone(entry), false)
            } else {
                let reservation = input
                    .len()
                    .checked_add(RESPONSE_SLOT_BYTES)
                    .ok_or(SERVICE_STATUS_PLUGIN_UNAVAILABLE)?;
                if state.entries.len() >= self.limits.entries
                    || reservation > self.limits.bytes.saturating_sub(state.bytes)
                {
                    return Err(SERVICE_STATUS_PLUGIN_UNAVAILABLE);
                }
                let entry = Arc::new(Entry {
                    export,
                    input: input.to_vec(),
                    owner: thread::current().id(),
                    outcome: OnceLock::new(),
                });
                state.bytes += reservation;
                state.entries.insert(key.clone(), Arc::clone(&entry));
                (entry, true)
            };
            drop(state);
            selected
        };
        if created {
            // A panic must also settle the identity; OnceLock alone would retry it.
            let mut result = catch_unwind(AssertUnwindSafe(execute))
                .unwrap_or(Err(SERVICE_STATUS_PLUGIN_UNAVAILABLE))
                .and_then(|bytes| {
                    if bytes.len() > MAX_RESPONSE_BYTES {
                        // Never rerun an already executed mutation to recover oversized output.
                        Err(super::SERVICE_STATUS_ENCODE_FAILED)
                    } else {
                        Ok(Arc::<[u8]>::from(bytes))
                    }
                });
            if let Ok(mut state) = self.state.lock() {
                let response_bytes = result.as_ref().map_or(0, |bytes| bytes.len());
                let additional = response_bytes.saturating_sub(RESPONSE_SLOT_BYTES);
                if additional > self.limits.bytes.saturating_sub(state.bytes) {
                    // Side effects may have happened. Retain a terminal failure,
                    // not a success whose bytes cannot be replayed.
                    result = Err(super::SERVICE_STATUS_ENCODE_FAILED);
                }
                state.bytes -= RESPONSE_SLOT_BYTES;
                state.bytes += result.as_ref().map_or(0, |bytes| bytes.len());
                let _ = entry.outcome.set(Outcome { result });
                state.completed.push_back((Instant::now(), key));
            } else {
                let _ = entry.outcome.set(Outcome { result });
            }
        }
        entry.outcome.wait().result.clone()
    }
}

pub(super) fn global() -> &'static Replay {
    static REPLAY: OnceLock<Replay> = OnceLock::new();
    REPLAY.get_or_init(|| Replay {
        limits: Limits::from_lookup(|name| std::env::var(name)).unwrap_or_else(|| Limits {
            entries: 0,
            ..Limits::default()
        }),
        state: Mutex::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn simultaneous_first_calls_execute_once_and_reject_inflight_conflicts() {
        let replay = Replay::default();
        let invocation = crate::PluginInvocationId::new();
        let calls = AtomicUsize::new(0);
        let entered = Barrier::new(2);
        let finish = Barrier::new(2);
        thread::scope(|scope| {
            let first = scope.spawn(|| {
                replay.invoke::<()>(1, 0, invocation.clone(), b"input", || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    entered.wait();
                    finish.wait();
                    Ok(vec![42; 17_000])
                })
            });
            entered.wait();
            assert_eq!(
                replay.invoke::<()>(1, 0, invocation.clone(), b"conflict", || panic!(
                    "must reject"
                )),
                Err(SERVICE_STATUS_INVALID_ARGUMENT)
            );
            assert_eq!(
                replay.invoke::<()>(1, 1, invocation.clone(), b"input", || panic!(
                    "wrong export"
                )),
                Err(SERVICE_STATUS_INVALID_ARGUMENT)
            );
            let duplicates: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        replay.invoke::<()>(1, 0, invocation.clone(), b"input", || {
                            panic!("must replay")
                        })
                    })
                })
                .collect();
            finish.wait();
            let outcome = first.join().unwrap().unwrap();
            for duplicate in duplicates {
                assert_eq!(duplicate.join().unwrap().unwrap(), outcome);
            }
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Equal IDs on distinct instances and plugin types are independent.
        assert!(
            replay
                .invoke::<()>(2, 0, invocation.clone(), b"input", || Ok(vec![1]))
                .is_ok()
        );
        assert!(
            replay
                .invoke::<u8>(1, 0, invocation, b"input", || Ok(vec![2]))
                .is_ok()
        );
    }

    #[test]
    fn failures_and_panics_are_terminal_until_expiry() {
        let replay = Replay::default();
        let id = crate::PluginInvocationId::new();
        assert_eq!(
            replay.invoke::<()>(1, 0, id.clone(), b"x", || panic!(
                "side effect already happened"
            )),
            Err(SERVICE_STATUS_PLUGIN_UNAVAILABLE)
        );
        assert_eq!(
            replay.invoke::<()>(1, 0, id.clone(), b"x", || panic!("must not retry")),
            Err(SERVICE_STATUS_PLUGIN_UNAVAILABLE)
        );
        {
            let mut state = replay.state.lock().unwrap();
            state.completed.front_mut().unwrap().0 = Instant::now()
                .checked_sub(RETENTION)
                .expect("test clock supports retention interval");
        }
        assert_eq!(
            &*replay.invoke::<()>(1, 0, id, b"x", || Ok(vec![1])).unwrap(),
            &[1]
        );
        assert_eq!(replay.state.lock().unwrap().bytes, 2);
    }

    #[test]
    fn recursive_duplicate_is_rejected_and_original_outcome_settles() {
        let replay = Replay::default();
        let id = crate::PluginInvocationId::new();
        assert!(
            replay
                .invoke::<()>(1, 0, id.clone(), b"x", || {
                    assert_eq!(
                        replay.invoke::<()>(1, 0, id.clone(), b"x", || panic!("recursive retry")),
                        Err(SERVICE_STATUS_PLUGIN_UNAVAILABLE)
                    );
                    Ok(vec![1])
                })
                .is_ok()
        );
        assert_eq!(
            &*replay
                .invoke::<()>(1, 0, id, b"x", || panic!("must replay"))
                .unwrap(),
            &[1]
        );
    }

    #[test]
    fn inflight_reservations_backpressure_before_side_effects() {
        let replay = Replay::default();
        // Simulate reservations held by in-flight requests; no capacity eviction.
        replay.state.lock().unwrap().bytes = MAX_RETAINED_BYTES;
        assert_eq!(
            replay.invoke::<()>(1, 0, crate::PluginInvocationId::new(), b"x", || panic!(
                "no reservation"
            )),
            Err(SERVICE_STATUS_PLUGIN_UNAVAILABLE)
        );
        assert!(replay.state.lock().unwrap().entries.is_empty());
    }

    #[test]
    fn startup_limits_are_explicit_and_invalid_values_disable_admission() {
        let defaults = Limits::from_lookup(|_| Err(std::env::VarError::NotPresent)).unwrap();
        assert_eq!(defaults.entries, MAX_ENTRIES);
        assert_eq!(defaults.bytes, MAX_RETAINED_BYTES);
        assert_eq!(defaults.retention, RETENTION);
        let configured = Limits::from_lookup(|name| {
            Ok(match name {
                "BMUX_NATIVE_REPLAY_MAX_ENTRIES" => "100000",
                "BMUX_NATIVE_REPLAY_MAX_BYTES" => "1073741824",
                "BMUX_NATIVE_REPLAY_RETENTION_MS" => "120000",
                _ => panic!("unexpected setting"),
            }
            .into())
        })
        .unwrap();
        assert_eq!(configured.entries, 100_000);
        assert_eq!(configured.bytes, 1024 * 1024 * 1024);
        assert_eq!(configured.retention, Duration::from_mins(2));
        for invalid in ["0", "-1", "garbage", "999999999999999999999999999999"] {
            assert!(Limits::from_lookup(|_| Ok(invalid.into())).is_none());
        }
    }

    #[test]
    fn expiry_queue_reclaims_only_elapsed_outcomes_and_updates_payload_budget() {
        let replay = Replay::default();
        let old = crate::PluginInvocationId::new();
        let fresh = crate::PluginInvocationId::new();
        replay
            .invoke::<()>(1, 0, old.clone(), b"old", || Ok(vec![1]))
            .unwrap();
        replay
            .invoke::<()>(1, 0, fresh.clone(), b"fresh", || Ok(vec![2]))
            .unwrap();
        replay
            .state
            .lock()
            .unwrap()
            .completed
            .front_mut()
            .unwrap()
            .0 = Instant::now()
            .checked_sub(RETENTION)
            .expect("test clock supports retention interval");
        // Admission reclaims the expired head, but never the still-live tail.
        assert_eq!(
            &*replay
                .invoke::<()>(1, 0, fresh, b"fresh", || panic!("not expired"))
                .unwrap(),
            &[2]
        );
        {
            let state = replay.state.lock().unwrap();
            assert_eq!(state.entries.len(), 1);
            assert_eq!(state.completed.len(), 1);
            assert_eq!(state.bytes, 6);
            drop(state);
        }
        assert_eq!(
            &*replay
                .invoke::<()>(1, 0, old, b"old", || Ok(vec![3]))
                .unwrap(),
            &[3]
        );
    }

    #[test]
    fn sustained_tiny_calls_retain_all_outcomes_without_256_call_ceiling() {
        let replay = Replay::default();
        let calls = AtomicUsize::new(0);
        let mut ids = Vec::new();
        // A busy interactive workload must fit within one retention window.
        for _ in 0..10_000 {
            let id = crate::PluginInvocationId::new();
            let result = replay
                .invoke::<()>(1, 0, id.clone(), b"input", || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(vec![42])
                })
                .unwrap();
            assert_eq!(&*result, &[42]);
            ids.push(id);
        }
        for id in ids {
            assert_eq!(
                &*replay
                    .invoke::<()>(1, 0, id.clone(), b"input", || panic!("must replay"))
                    .unwrap(),
                &[42]
            );
            assert_eq!(
                replay.invoke::<()>(1, 0, id, b"conflict", || panic!("must reject")),
                Err(SERVICE_STATUS_INVALID_ARGUMENT)
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 10_000);
        let state = replay.state.lock().unwrap();
        assert_eq!(state.entries.len(), 10_000);
        assert_eq!(state.completed.len(), 10_000);
        assert_eq!(state.bytes, 60_000);
        drop(state);
    }

    #[test]
    fn request_above_one_mib_replays_and_conflicts_without_transport_size_cap() {
        let replay = Replay::default();
        let id = crate::PluginInvocationId::new();
        let mut input = vec![42; 2 * 1024 * 1024];
        assert_eq!(
            &*replay
                .invoke::<()>(1, 0, id.clone(), &input, || Ok(vec![1]))
                .unwrap(),
            &[1]
        );
        assert_eq!(
            &*replay
                .invoke::<()>(1, 0, id.clone(), &input, || panic!("must replay"))
                .unwrap(),
            &[1]
        );
        input[0] = 0;
        assert_eq!(
            replay.invoke::<()>(1, 0, id, &input, || panic!("must reject")),
            Err(SERVICE_STATUS_INVALID_ARGUMENT)
        );
    }

    #[test]
    fn deep_nesting_retains_every_response_without_maximum_response_reservations() {
        fn nested(replay: &Replay, ids: &[crate::PluginInvocationId], calls: &AtomicUsize) {
            let Some((id, children)) = ids.split_first() else {
                return;
            };
            let result = replay
                .invoke::<()>(1, 0, id.clone(), b"nested", || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    nested(replay, children, calls);
                    Ok(vec![42; 17_000])
                })
                .unwrap();
            assert_eq!(&*result, &vec![42; 17_000]);
        }
        let replay = Replay::default();
        let ids: Vec<_> = (0..64).map(|_| crate::PluginInvocationId::new()).collect();
        let calls = AtomicUsize::new(0);
        nested(&replay, &ids, &calls);
        for id in ids {
            assert_eq!(
                &*replay
                    .invoke::<()>(1, 0, id, b"nested", || panic!("must replay"))
                    .unwrap(),
                &vec![42; 17_000]
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 64);
        let state = replay.state.lock().unwrap();
        assert_eq!(state.bytes, 64 * (6 + 17_000));
        drop(state);
    }

    #[test]
    fn parallel_small_outputs_do_not_reserve_maximum_response_each() {
        let replay = Replay::default();
        let barrier = Barrier::new(32);
        let calls = AtomicUsize::new(0);
        let ids: Vec<_> = (0..32).map(|_| crate::PluginInvocationId::new()).collect();
        thread::scope(|scope| {
            for id in &ids {
                let replay = &replay;
                let barrier = &barrier;
                let calls = &calls;
                scope.spawn(move || {
                    assert_eq!(
                        &*replay
                            .invoke::<()>(1, 0, id.clone(), b"parallel", || {
                                calls.fetch_add(1, Ordering::SeqCst);
                                barrier.wait();
                                Ok(vec![42; 17_000])
                            })
                            .unwrap(),
                        &vec![42; 17_000]
                    );
                });
            }
        });
        for id in ids {
            assert!(
                replay
                    .invoke::<()>(1, 0, id, b"parallel", || panic!("must replay"))
                    .is_ok()
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 32);
        assert_eq!(replay.state.lock().unwrap().bytes, 32 * (8 + 17_000));
    }

    #[test]
    fn output_budget_failure_is_terminal_and_does_not_discard_prior_success() {
        let replay = Replay {
            limits: Limits {
                bytes: 2 * RESPONSE_SLOT_BYTES,
                ..Limits::default()
            },
            ..Replay::default()
        };
        let retained = crate::PluginInvocationId::new();
        assert_eq!(
            &*replay
                .invoke::<()>(1, 0, retained.clone(), b"a", || Ok(vec![1; 4000]))
                .unwrap(),
            &vec![1; 4000]
        );
        let id = crate::PluginInvocationId::new();
        let calls = AtomicUsize::new(0);
        for _ in 0..2 {
            assert_eq!(
                replay.invoke::<()>(1, 0, id.clone(), b"b", || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(vec![2; RESPONSE_SLOT_BYTES + 1000])
                }),
                Err(super::super::SERVICE_STATUS_ENCODE_FAILED)
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            &*replay
                .invoke::<()>(1, 0, retained, b"a", || panic!("must replay success"))
                .unwrap(),
            &vec![1; 4000]
        );
        assert_eq!(replay.state.lock().unwrap().bytes, 4002);
    }

    #[test]
    fn byte_budget_rejects_large_requests_before_execution_without_partial_admission() {
        let replay = Replay {
            limits: Limits {
                bytes: RESPONSE_SLOT_BYTES + 3,
                ..Limits::default()
            },
            ..Replay::default()
        };
        assert_eq!(
            replay.invoke::<()>(1, 0, crate::PluginInvocationId::new(), b"four", || panic!(
                "no capacity"
            )),
            Err(SERVICE_STATUS_PLUGIN_UNAVAILABLE)
        );
        let state = replay.state.lock().unwrap();
        assert!(state.entries.is_empty());
        assert!(state.completed.is_empty());
        assert_eq!(state.bytes, 0);
        drop(state);
    }

    #[test]
    fn admission_is_bounded_without_evicting_completed_outcomes() {
        let replay = Replay {
            limits: Limits {
                entries: 4,
                ..Limits::default()
            },
            ..Replay::default()
        };
        for _ in 0..4 {
            assert!(
                replay
                    .invoke::<()>(1, 0, crate::PluginInvocationId::new(), b"x", || Ok(vec![]))
                    .is_ok()
            );
        }
        assert_eq!(
            replay.invoke::<()>(1, 0, crate::PluginInvocationId::new(), b"x", || panic!(
                "no capacity"
            )),
            Err(SERVICE_STATUS_PLUGIN_UNAVAILABLE)
        );
    }
}
