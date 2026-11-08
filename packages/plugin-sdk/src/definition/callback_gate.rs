//! Bounded, wake-driven reader/writer admission for synchronous author adapters.
//! This is local exclusion, never invocation authority or a recursion detector.
use crate::{PluginCommandError, PluginResult};
use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
};

const MAX_WAITERS: usize = 64;

#[derive(Default)]
pub(super) struct CallbackGate(Mutex<State>);
#[derive(Default)]
struct State {
    readers: usize,
    writer: bool,
    next: u64,
    waiters: VecDeque<Waiter>,
}
struct Waiter {
    id: u64,
    exclusive: bool,
    wake: Waker,
}
pub(super) struct Admission {
    gate: Arc<CallbackGate>,
    exclusive: bool,
    id: Option<u64>,
}
pub(super) struct Permit {
    gate: Arc<CallbackGate>,
    exclusive: bool,
}
fn poisoned() -> PluginCommandError {
    PluginCommandError::failed("callback admission state poisoned")
}
fn wake(wakers: Vec<Waker>) {
    for waker in wakers {
        waker.wake();
    }
}
impl State {
    fn wakeable(&self) -> Vec<Waker> {
        if self.writer || self.waiters.is_empty() {
            return Vec::new();
        }
        if self.waiters[0].exclusive {
            return if self.readers == 0 {
                vec![self.waiters[0].wake.clone()]
            } else {
                Vec::new()
            };
        }
        self.waiters
            .iter()
            .take_while(|waiter| !waiter.exclusive)
            .map(|waiter| waiter.wake.clone())
            .collect()
    }
}
impl CallbackGate {
    pub(super) fn acquire(self: &Arc<Self>, exclusive: bool) -> Admission {
        Admission {
            gate: Arc::clone(self),
            exclusive,
            id: None,
        }
    }
    /// Admit only if the gate is free right now and no waiter is queued.
    ///
    /// Never enqueues: queued waiters precede newcomers, so a caller that must
    /// not wait (a re-entrant entry whose own ancestor may hold the gate or be
    /// ahead of a queued writer) gets `Ok(None)` instead of a deadlock.
    pub(super) fn try_acquire(self: &Arc<Self>, exclusive: bool) -> PluginResult<Option<Permit>> {
        let mut state = self.0.lock().map_err(|_| poisoned())?;
        if state.writer || (exclusive && state.readers > 0) || !state.waiters.is_empty() {
            return Ok(None);
        }
        if exclusive {
            state.writer = true;
        } else {
            state.readers += 1;
        }
        drop(state);
        Ok(Some(Permit {
            gate: Arc::clone(self),
            exclusive,
        }))
    }
}
impl Future for Admission {
    type Output = PluginResult<Permit>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let gate = Arc::clone(&self.gate);
        let Ok(mut state) = gate.0.lock() else {
            return Poll::Ready(Err(poisoned()));
        };
        let position = self
            .id
            .and_then(|id| state.waiters.iter().position(|w| w.id == id));
        if self.id.is_some() && position.is_none() {
            return Poll::Ready(Err(PluginCommandError::failed(
                "callback admission identity lost",
            )));
        }
        let precedes_writer = match position {
            None => state.waiters.is_empty(),
            Some(position) if self.exclusive => position == 0,
            Some(position) => !state.waiters.iter().take(position).any(|w| w.exclusive),
        };
        let available = !state.writer && (!self.exclusive || state.readers == 0);
        if available && precedes_writer {
            if let Some(position) = position {
                let _ = state.waiters.remove(position);
            }
            self.id = None;
            if self.exclusive {
                state.writer = true;
            } else {
                state.readers += 1;
            }
            let wakers = state.wakeable();
            drop(state);
            wake(wakers);
            return Poll::Ready(Ok(Permit {
                gate,
                exclusive: self.exclusive,
            }));
        }
        if let Some(position) = position {
            state.waiters[position].wake.clone_from(cx.waker());
        } else {
            if state.waiters.len() >= MAX_WAITERS {
                return Poll::Ready(Err(PluginCommandError::unavailable(
                    "callback admission queue full",
                )));
            }
            let Some(next) = state.next.checked_add(1) else {
                return Poll::Ready(Err(PluginCommandError::failed(
                    "callback admission identity exhausted",
                )));
            };
            state.next = next;
            self.id = Some(next);
            state.waiters.push_back(Waiter {
                id: next,
                exclusive: self.exclusive,
                wake: cx.waker().clone(),
            });
        }
        Poll::Pending
    }
}
impl Drop for Admission {
    fn drop(&mut self) {
        let Some(id) = self.id else {
            return;
        };
        if let Ok(mut state) = self.gate.0.lock() {
            state.waiters.retain(|waiter| waiter.id != id);
            let wakers = state.wakeable();
            drop(state);
            wake(wakers);
        }
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        if let Ok(mut state) = self.gate.0.lock() {
            if self.exclusive {
                state.writer = false;
            } else {
                state.readers -= 1;
            }
            let wakers = state.wakeable();
            drop(state);
            wake(wakers);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn poll(admission: &mut Admission) -> Poll<PluginResult<Permit>> {
        Pin::new(admission).poll(&mut Context::from_waker(Waker::noop()))
    }
    fn ready(admission: &mut Admission) -> Permit {
        let Poll::Ready(Ok(permit)) = poll(admission) else {
            panic!("admission not ready");
        };
        permit
    }
    #[test]
    fn independent_readers_overlap_writer_prevents_overtaking() {
        let gate = Arc::new(CallbackGate::default());
        let first = ready(&mut gate.acquire(false));
        let second = ready(&mut gate.acquire(false));
        let mut writer = gate.acquire(true);
        assert!(poll(&mut writer).is_pending());
        let mut reader = gate.acquire(false);
        assert!(poll(&mut reader).is_pending());
        drop(first);
        assert!(poll(&mut writer).is_pending());
        drop(second);
        let writing = ready(&mut writer);
        assert!(poll(&mut reader).is_pending());
        drop(writing);
        drop(ready(&mut reader));
    }
    #[test]
    fn try_acquire_never_queues_or_overtakes() {
        let gate = Arc::new(CallbackGate::default());
        let writer = gate.try_acquire(true).unwrap().expect("free gate admits");
        assert!(gate.try_acquire(false).unwrap().is_none());
        assert!(gate.try_acquire(true).unwrap().is_none());
        assert!(gate.0.lock().unwrap().waiters.is_empty());
        drop(writer);
        let reader = gate.try_acquire(false).unwrap().expect("readers share");
        let second = gate.try_acquire(false).unwrap().expect("readers share");
        assert!(gate.try_acquire(true).unwrap().is_none());
        let mut queued = gate.acquire(true);
        assert!(poll(&mut queued).is_pending());
        // A queued writer precedes newcomers, so a reader must not overtake it.
        assert!(gate.try_acquire(false).unwrap().is_none());
        drop((reader, second));
        drop(ready(&mut queued));
        drop(
            gate.try_acquire(true)
                .unwrap()
                .expect("released gate admits"),
        );
    }
    #[test]
    fn queued_drop_removes_writer_without_entry() {
        let gate = Arc::new(CallbackGate::default());
        let first = ready(&mut gate.acquire(false));
        let mut writer = gate.acquire(true);
        assert!(poll(&mut writer).is_pending());
        let mut reader = gate.acquire(false);
        assert!(poll(&mut reader).is_pending());
        drop(writer);
        drop(ready(&mut reader));
        drop(first);
    }
    #[test]
    fn bounded_queue_and_distinct_poison() {
        let gate = Arc::new(CallbackGate::default());
        let permit = ready(&mut gate.acquire(true));
        let mut queued: Vec<_> = (0..MAX_WAITERS).map(|_| gate.acquire(false)).collect();
        for waiter in &mut queued {
            assert!(poll(waiter).is_pending());
        }
        assert!(matches!(
            poll(&mut gate.acquire(false)),
            Poll::Ready(Err(_))
        ));
        drop(queued);
        drop(permit);
        let owner = Arc::clone(&gate);
        assert!(
            std::thread::spawn(move || {
                let _guard = owner.0.lock().unwrap();
                panic!("poison");
            })
            .join()
            .is_err()
        );
        assert!(matches!(
            poll(&mut gate.acquire(false)),
            Poll::Ready(Err(_))
        ));
    }
}
