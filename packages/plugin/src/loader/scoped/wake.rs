//! Waker plumbing for scoped operations: observer signals and a deadline timer.
//!
//! Operations are driven by real wakers end to end. [`OperationSignal`] is the
//! observer side: its waker is handed to [`super::LoadedExecution::poll`] and a
//! wake (author future, settlement, cancellation, guest host-wake callback)
//! releases exactly one waiting observer, synchronously or asynchronously.
//! The timer fires wakes at explicit instants only: caller-supplied operation
//! deadlines, and the documented legacy repoll of native guests that did not
//! negotiate `CAP_HOST_WAKE_V1`.

use bmux_plugin_sdk::{PluginError, Result};
use std::{
    cmp::Reverse,
    collections::BinaryHeap,
    sync::{Arc, Condvar, Mutex, OnceLock},
    task::{Poll, Wake, Waker},
    time::{Duration, Instant},
};

/// Repoll interval for legacy native guests without `CAP_HOST_WAKE_V1`.
///
/// Such guests poll author futures with a no-op waker and promise no wake, so
/// the ABI requires periodic host polling. This applies to no other operation.
pub(super) const LEGACY_NATIVE_REPOLL: Duration = Duration::from_millis(2);

#[derive(Default)]
struct SignalState {
    woken: bool,
    task: Option<Waker>,
}

/// A wake-driven observer of one operation. A wake that arrives before the
/// observer waits is retained, so wakes are never lost; spurious wakes only
/// cause one extra poll.
#[derive(Default)]
pub struct OperationSignal {
    state: Mutex<SignalState>,
    changed: Condvar,
}
impl Wake for OperationSignal {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        let task = self.state.lock().ok().and_then(|mut state| {
            state.woken = true;
            state.task.take()
        });
        self.changed.notify_all();
        if let Some(task) = task {
            task.wake();
        }
    }
}
impl OperationSignal {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    #[must_use]
    pub fn waker(self: &Arc<Self>) -> Waker {
        Waker::from(Arc::clone(self))
    }
    /// Block the current thread until the next wake. For synchronous callers
    /// that cannot enter an async runtime; no periodic polling occurs.
    ///
    /// # Errors
    /// The signal state was poisoned.
    pub fn wait_blocking(&self) -> Result<()> {
        let state = self.state.lock().map_err(|_| poisoned())?;
        self.changed
            .wait_while(state, |state| !state.woken)
            .map_err(|_| poisoned())?
            .woken = false;
        Ok(())
    }
    /// Wait asynchronously until the next wake.
    ///
    /// # Errors
    /// The signal state was poisoned.
    pub async fn wait(&self) -> Result<()> {
        std::future::poll_fn(|cx| {
            let Ok(mut state) = self.state.lock() else {
                return Poll::Ready(Err(poisoned()));
            };
            if state.woken {
                state.woken = false;
                state.task = None;
                Poll::Ready(Ok(()))
            } else {
                state.task = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await
    }
}

fn poisoned() -> PluginError {
    PluginError::ServiceProtocol {
        details: "operation signal poisoned".into(),
    }
}

struct Timer {
    queue: Mutex<TimerQueue>,
    changed: Condvar,
}
#[derive(Default)]
struct TimerQueue {
    next: u64,
    heap: BinaryHeap<Reverse<(Instant, u64)>>,
    wakers: std::collections::HashMap<u64, Waker>,
}

fn timer() -> Result<&'static Timer> {
    static TIMER: OnceLock<std::result::Result<&'static Timer, String>> = OnceLock::new();
    TIMER
        .get_or_init(|| {
            let timer: &'static Timer = Box::leak(Box::new(Timer {
                queue: Mutex::new(TimerQueue::default()),
                changed: Condvar::new(),
            }));
            std::thread::Builder::new()
                .name("bmux-plugin-wake-timer".into())
                .spawn(move || timer.run())
                .map(|_| timer)
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .copied()
        .map_err(|details| PluginError::ServiceProtocol {
            details: format!("operation wake timer unavailable: {details}"),
        })
}

impl Timer {
    fn run(&self) {
        let Ok(mut queue) = self.queue.lock() else {
            return;
        };
        loop {
            let now = Instant::now();
            let mut due = Vec::new();
            while let Some(Reverse((at, id))) = queue.heap.peek().copied()
                && at <= now
            {
                queue.heap.pop();
                if let Some(waker) = queue.wakers.remove(&id) {
                    due.push(waker);
                }
            }
            if !due.is_empty() {
                drop(queue);
                for waker in due {
                    waker.wake();
                }
                let Ok(next) = self.queue.lock() else {
                    return;
                };
                queue = next;
                continue;
            }
            let next = queue.heap.peek().map(|Reverse((at, _))| *at);
            queue = match next {
                None => match self.changed.wait(queue) {
                    Ok(queue) => queue,
                    Err(_) => return,
                },
                Some(at) => match self
                    .changed
                    .wait_timeout(queue, at.saturating_duration_since(now))
                {
                    Ok((queue, _)) => queue,
                    Err(_) => return,
                },
            };
        }
    }
}

/// Wake `waker` once at `at`. Used only for explicit deadlines and the legacy
/// native repoll; never as a substitute for a real completion wake.
///
/// # Errors
/// The timer thread cannot be started or its state is poisoned.
pub(super) fn wake_at(at: Instant, waker: Waker) -> Result<()> {
    let timer = timer()?;
    let mut queue = timer.queue.lock().map_err(|_| poisoned())?;
    let id = queue.next;
    queue.next = queue.next.wrapping_add(1);
    queue.heap.push(Reverse((at, id)));
    queue.wakers.insert(id, waker);
    drop(queue);
    timer.changed.notify_all();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_retains_wake_that_precedes_wait() {
        let signal = OperationSignal::new();
        signal.waker().wake();
        signal.wait_blocking().unwrap();
        let waiter = Arc::clone(&signal);
        let thread = std::thread::spawn(move || waiter.wait_blocking().unwrap());
        signal.waker().wake_by_ref();
        thread.join().unwrap();
    }

    #[tokio::test]
    async fn async_signal_completes_from_foreign_thread_wake() {
        let signal = OperationSignal::new();
        let waker = signal.waker();
        let thread = std::thread::spawn(move || waker.wake());
        signal.wait().await.unwrap();
        thread.join().unwrap();
    }

    #[test]
    fn timer_wakes_at_the_requested_instant() {
        let signal = OperationSignal::new();
        let start = Instant::now();
        wake_at(start + Duration::from_millis(5), signal.waker()).unwrap();
        signal.wait_blocking().unwrap();
        assert!(start.elapsed() >= Duration::from_millis(5));
    }
}
