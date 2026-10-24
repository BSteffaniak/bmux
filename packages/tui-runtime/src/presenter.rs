//! Runtime presentation contract.

use bmux_tui::geometry::Size;

/// Reason the runtime requests a backend reset before the next presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetReason {
    /// Application state invalidated all retained presentation state.
    Application,
    /// Terminal viewport changed.
    Resize,
}

/// Neutral report from one successfully committed presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PresentReport {
    /// Number of terminal cells changed, when the presenter can report it.
    pub changed_cells: usize,
    /// Whether the presentation repainted the complete surface.
    pub full_repaint: bool,
}

/// Presents the current application state to a terminal backend.
pub trait Presenter<P> {
    /// Presentation error.
    type Error;

    /// Notify the presenter that the terminal viewport changed.
    fn resize(&mut self, _size: Size) {}

    /// Reset retained backend state before a future presentation.
    fn reset(&mut self, reason: ResetReason);

    /// Present the current application state.
    ///
    /// # Errors
    ///
    /// Returns a presenter-defined error when frame construction or terminal output fails. A
    /// failed presentation must not be treated as committed by the presenter.
    fn present(&mut self, program: &mut P) -> Result<PresentReport, Self::Error>;

    /// Start one presentation. `None` transfers delivery to the pollable path.
    /// No further frame will be prepared until that delivery settles.
    /// # Errors
    /// Returns preparation or immediate delivery errors.
    fn start_present(&mut self, program: &mut P) -> Result<Option<PresentReport>, Self::Error> {
        self.present(program).map(Some)
    }

    /// Poll the single pending delivery; publish only its exact successfully flushed candidate.
    /// # Errors
    /// Returns delivery errors without advancing committed authority.
    fn poll_present(
        &mut self,
        _cx: &mut std::task::Context<'_>,
        _program: &mut P,
    ) -> std::task::Poll<Result<PresentReport, Self::Error>> {
        std::task::Poll::Pending
    }

    /// Cancel and reclaim owned output work before returning. Must not detach blocked tasks.
    fn cancel_present(&mut self) {}
}

/// Owned output transport for asynchronous presentation.
///
/// Arbitrary blocking `Write` is intentionally not adapted.
/// `deliver` must check cancellation between bounded nonblocking operations,
/// including flush, and return promptly on cancellation. It must not detach work.
/// This is a behavioral/liveness contract, not a memory-safety invariant.
pub trait CancellableOutput: Send + 'static {
    /// Deliver with an owned flag available to incremental codec writers.
    /// # Errors
    /// Returns output or cancellation errors.
    fn deliver_owned(
        &mut self,
        bytes: &[u8],
        cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> std::io::Result<()> {
        self.deliver(bytes, &cancelled)
    }
    /// Write and flush one transaction, without retaining bytes after return.
    /// # Errors
    /// Returns output or cancellation errors. Partial writes are permitted on failure.
    fn deliver(
        &mut self,
        bytes: &[u8],
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> std::io::Result<()>;
}

#[cfg(target_os = "linux")]
// Independent O_NONBLOCK descriptor, bounded chunks/poll waits, no tcdrain.
impl CancellableOutput for bmux_tui::terminal::LinuxTerminalOutput {
    fn deliver(
        &mut self,
        bytes: &[u8],
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> std::io::Result<()> {
        Self::deliver(self, bytes, cancelled)
    }
}

#[cfg(any(feature = "images", test))]
pub(crate) struct Delivery<O> {
    worker: Option<std::thread::JoinHandle<O>>,
    receiver: tokio::sync::oneshot::Receiver<std::io::Result<()>>,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(any(feature = "images", test))]
impl<O: CancellableOutput> Delivery<O> {
    pub(crate) fn start(output: &mut Option<O>, bytes: Vec<u8>) -> std::io::Result<Self> {
        Self::start_with(output, bytes, |job| {
            std::thread::Builder::new()
                .name("tui-output".into())
                .spawn(job)
        })
    }

    fn start_with<F>(output: &mut Option<O>, bytes: Vec<u8>, spawn: F) -> std::io::Result<Self>
    where
        F: FnOnce(Box<dyn FnOnce() -> O + Send>) -> std::io::Result<std::thread::JoinHandle<O>>,
    {
        let owner = output
            .take()
            .ok_or_else(|| std::io::Error::other("output owner unavailable"))?;
        let handoff = std::sync::Arc::new(std::sync::Mutex::new(Some(owner)));
        let worker_handoff = std::sync::Arc::clone(&handoff);
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_cancelled = std::sync::Arc::clone(&cancelled);
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let worker = match spawn(Box::new(move || {
            let mut output = worker_handoff
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                .expect("output handoff");
            let result = output.deliver_owned(&bytes, worker_cancelled);
            let _ = sender.send(result);
            output
        })) {
            Ok(worker) => worker,
            Err(error) => {
                *output = handoff
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                return Err(error);
            }
        };
        Ok(Self {
            worker: Some(worker),
            receiver,
            cancelled,
        })
    }

    pub(crate) fn poll(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use std::future::Future;
        std::pin::Pin::new(&mut self.receiver)
            .poll(cx)
            .map(|result| {
                result.unwrap_or_else(|_| Err(std::io::Error::other("output worker panicked")))
            })
    }

    pub(crate) fn reclaim(&mut self) -> std::io::Result<O> {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        self.worker
            .take()
            .expect("owned worker")
            .join()
            .map_err(|_| std::io::Error::other("output worker panicked"))
    }
}

#[cfg(any(feature = "images", test))]
impl<O> Drop for Delivery<O> {
    fn drop(&mut self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod delivery_fault_tests {
    use super::*;

    #[test]
    fn successful_delivery_can_be_polled_and_reclaimed() {
        struct Output(Vec<u8>);
        impl CancellableOutput for Output {
            fn deliver(
                &mut self,
                bytes: &[u8],
                _: &std::sync::atomic::AtomicBool,
            ) -> std::io::Result<()> {
                self.0.extend_from_slice(bytes);
                Ok(())
            }
        }
        let mut output = Some(Output(Vec::new()));
        let mut delivery = Delivery::start(&mut output, vec![1, 2]).expect("start");
        assert!(output.is_none());
        // Joining the owned worker makes completion deterministic before polling.
        let reclaimed = delivery.reclaim().expect("reclaim");
        assert_eq!(reclaimed.0, [1, 2]);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(matches!(
            delivery.poll(&mut cx),
            std::task::Poll::Ready(Ok(()))
        ));
    }
    #[test]
    fn dropping_delivery_cancels_and_joins_the_owned_worker() {
        struct Output(std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl CancellableOutput for Output {
            fn deliver(
                &mut self,
                _: &[u8],
                cancelled: &std::sync::atomic::AtomicBool,
            ) -> std::io::Result<()> {
                while !cancelled.load(std::sync::atomic::Ordering::Acquire) {
                    std::thread::yield_now();
                }
                self.0.store(true, std::sync::atomic::Ordering::Release);
                Ok(())
            }
        }
        let completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut output = Some(Output(std::sync::Arc::clone(&completed)));
        let delivery = Delivery::start(&mut output, Vec::new()).expect("start");
        drop(delivery);
        assert!(completed.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn thread_start_failure_retains_output_for_cleanup_without_writes() {
        struct Output;
        impl CancellableOutput for Output {
            fn deliver(
                &mut self,
                _: &[u8],
                _: &std::sync::atomic::AtomicBool,
            ) -> std::io::Result<()> {
                panic!("failed spawn must not write");
            }
        }
        let mut output = Some(Output);
        let result = Delivery::start_with(&mut output, vec![1], |_job| {
            Err(std::io::Error::other("injected spawn failure"))
        });
        assert!(result.is_err());
        assert!(
            output.is_some(),
            "cleanup retains the original output owner"
        );
    }
}

/// Presenter useful for tests and applications with no terminal output.
#[derive(Debug, Default)]
pub struct HeadlessPresenter {
    presentations: u64,
    resets: u64,
}

impl HeadlessPresenter {
    /// Number of successful presentation calls.
    #[must_use]
    pub const fn presentations(&self) -> u64 {
        self.presentations
    }

    /// Number of reset calls.
    #[must_use]
    pub const fn resets(&self) -> u64 {
        self.resets
    }
}

impl<P> Presenter<P> for HeadlessPresenter {
    type Error = std::convert::Infallible;

    fn reset(&mut self, _reason: ResetReason) {
        self.resets = self.resets.saturating_add(1);
    }

    fn present(&mut self, _program: &mut P) -> Result<PresentReport, Self::Error> {
        self.presentations = self.presentations.saturating_add(1);
        Ok(PresentReport::default())
    }
}
