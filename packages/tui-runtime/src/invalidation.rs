//! Coalesced asynchronous invalidation signal.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::sync::Notify;

/// Cloneable redraw/invalidation latch.
///
/// Requests transition one shared bit from clean to dirty. Repeated requests while dirty are
/// coalesced rather than queued. Consumers call [`Self::wait`] and then [`Self::take`] to observe
/// and clear the pending invalidation without losing a request racing with the clear operation.
#[derive(Debug, Clone, Default)]
pub struct InvalidationSignal {
    shared: Arc<InvalidationShared>,
}

#[derive(Debug, Default)]
struct InvalidationShared {
    dirty: AtomicBool,
    requests: AtomicU64,
    coalesced: AtomicU64,
    notify: Notify,
}

impl InvalidationSignal {
    /// Create a clean invalidation signal.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark the signal dirty and wake one waiter.
    pub fn request(&self) {
        self.shared.requests.fetch_add(1, Ordering::Relaxed);
        if self.shared.dirty.swap(true, Ordering::AcqRel) {
            self.shared.coalesced.fetch_add(1, Ordering::Relaxed);
        }
        self.shared.notify.notify_one();
    }

    /// Return and clear the current dirty state.
    #[must_use]
    pub fn take(&self) -> bool {
        self.shared.dirty.swap(false, Ordering::AcqRel)
    }

    /// Return whether an invalidation is pending.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        self.shared.dirty.load(Ordering::Acquire)
    }

    /// Wait until an invalidation is pending.
    pub async fn wait(&self) {
        loop {
            let notified = self.shared.notify.notified();
            if self.is_pending() {
                return;
            }
            notified.await;
        }
    }

    /// Return total invalidation requests.
    #[must_use]
    pub fn requests(&self) -> u64 {
        self.shared.requests.load(Ordering::Relaxed)
    }

    /// Return requests coalesced while already dirty.
    #[must_use]
    pub fn coalesced(&self) -> u64 {
        self.shared.coalesced.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::InvalidationSignal;

    #[tokio::test]
    async fn repeated_requests_coalesce_without_losing_wakeup() {
        let signal = InvalidationSignal::new();
        signal.request();
        signal.request();
        signal.wait().await;

        assert!(signal.take());
        assert!(!signal.take());
        assert_eq!(signal.requests(), 2);
        assert_eq!(signal.coalesced(), 1);
    }

    #[tokio::test]
    async fn request_after_take_wakes_next_wait() {
        let signal = InvalidationSignal::new();
        signal.request();
        assert!(signal.take());

        let waiter = signal.clone();
        let task = tokio::spawn(async move {
            waiter.wait().await;
            waiter.take()
        });
        tokio::task::yield_now().await;
        assert!(!task.is_finished());
        signal.request();
        assert!(task.await.expect("waiter succeeds"));
    }
}
