//! Asynchronous access to the process-wide input reader.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::Stream;

use super::Event;
use super::reader::{self, Filter, InternalEvent, Waker};

/// A stream of input events from the process-wide reader.
///
/// Polling checks for an event without blocking. When none is ready, a helper
/// thread waits on the reader and wakes the task once input arrives, so
/// [`super::poll`] and [`super::read`] stay usable between polls. Dropping the
/// stream interrupts that wait.
pub struct EventStream {
    reader_waker: Option<Waker>,
    wait_pending: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    tasks: SyncSender<WaitTask>,
}

struct WaitTask {
    waker: std::task::Waker,
    wait_pending: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
}

impl Default for EventStream {
    fn default() -> Self {
        let (tasks, receiver) = mpsc::sync_channel::<WaitTask>(1);
        std::thread::Builder::new()
            .name("bmux-terminal-events".to_owned())
            .spawn(move || {
                while let Ok(task) = receiver.recv() {
                    loop {
                        if matches!(reader::poll_internal(None, Filter::Event), Ok(true)) {
                            break;
                        }
                        if task.shutdown.load(Ordering::SeqCst) {
                            break;
                        }
                    }
                    task.wait_pending.store(false, Ordering::SeqCst);
                    task.waker.wake();
                }
            })
            .expect("spawn terminal event stream thread");
        Self {
            reader_waker: reader::global_waker(),
            wait_pending: Arc::new(AtomicBool::new(false)),
            shutdown: Arc::new(AtomicBool::new(false)),
            tasks,
        }
    }
}

impl EventStream {
    /// Create a stream over the process-wide reader.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl std::fmt::Debug for EventStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventStream").finish_non_exhaustive()
    }
}

impl Stream for EventStream {
    type Item = io::Result<Event>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match reader::poll_internal(Some(Duration::ZERO), Filter::Event) {
            Ok(true) => match reader::read_internal(Filter::Event) {
                Ok(InternalEvent::Event(event)) => Poll::Ready(Some(Ok(event))),
                Ok(_) => unreachable!("the event filter only accepts user-facing events"),
                Err(error) => Poll::Ready(Some(Err(error))),
            },
            Ok(false) => {
                // Hand one wait to the helper thread at a time.
                if self
                    .wait_pending
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    self.shutdown.store(false, Ordering::SeqCst);
                    let _ = self.tasks.send(WaitTask {
                        waker: cx.waker().clone(),
                        wait_pending: Arc::clone(&self.wait_pending),
                        shutdown: Arc::clone(&self.shutdown),
                    });
                }
                Poll::Pending
            }
            Err(error) => Poll::Ready(Some(Err(error))),
        }
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(waker) = &self.reader_waker {
            waker.wake();
        }
    }
}
