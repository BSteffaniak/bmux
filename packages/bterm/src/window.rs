//! Single-inflight synchronous presentation handoff. The worker can acknowledge
//! only an actual, timely main-thread output, never admission into a queue.

use anyhow::{Result, anyhow, bail};
use bmux_frontend_runtime::presentation::{PreparedDelivery, PresentationIdentity};
use bterm_native_platform::PlatformProxy;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

const OUTPUT_TIMEOUT: Duration = Duration::from_secs(10);
const TELEMETRY_WINDOW: usize = 128;

/// One UI-serviced output attempt, not an input-to-display latency measurement.
/// Retained only with `BTERM_RENDER_TELEMETRY=1`; the history is capped at 128.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputTelemetrySample {
    pub identity: PresentationIdentity,
    /// Producer gate contention before construction of the queued transaction.
    pub producer_gate_ns: u64,
    /// From just before queue admission until UI service begins, including wake delay.
    pub queue_ns: u64,
    /// UI synchronization and complete output callback, before receipt transmission.
    pub output_ns: u64,
    pub renderer: Option<bterm_native_renderer::FrameTelemetry>,
    pub succeeded: bool,
}

#[derive(Default)]
struct OutputTelemetry {
    attempts: u64,
    samples: std::collections::VecDeque<OutputTelemetrySample>,
}
impl OutputTelemetry {
    fn record(&mut self, sample: OutputTelemetrySample) {
        self.attempts = self.attempts.saturating_add(1);
        if self.samples.len() == TELEMETRY_WINDOW {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }
}
type SharedTelemetry = Option<Arc<Mutex<OutputTelemetry>>>;

fn elapsed_ns(start: Option<Instant>) -> u64 {
    start.map_or(0, |start| {
        u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
    })
}

#[derive(Default)]
struct Lifecycle {
    closed: bool,
}
#[derive(Default)]
struct Transaction {
    cancelled: bool,
    completed: bool,
}

impl Transaction {
    fn output(
        &mut self,
        closed: bool,
        deadline: Instant,
        output: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        let blocker = if closed {
            Some("native output receiver is closed")
        } else if self.cancelled {
            Some("native presentation transaction was cancelled")
        } else if Instant::now() >= deadline {
            Some("native presentation deadline expired before output")
        } else {
            None
        };
        if let Some(blocker) = blocker {
            self.cancelled = true;
            Err(blocker.into())
        } else {
            let result = output();
            self.completed = true;
            result
        }
    }
}

/// One complete output transaction. A timed-out transaction must not reach GPU
/// output. Use `present` rather than accessing the delivery independently.
pub struct PendingOutput {
    delivery: PreparedDelivery,
    receipt: mpsc::SyncSender<Result<(), String>>,
    transaction: Arc<Mutex<Transaction>>,
    lifecycle: Arc<Mutex<Lifecycle>>,
    deadline: Instant,
    telemetry: SharedTelemetry,
    producer_gate_ns: u64,
    queued_at: Option<Instant>,
}

impl PendingOutput {
    /// Serialize cancellation/closure with actual output. Holds the transaction
    /// gate through presentation, so a timeout cannot race a late GPU submit.
    pub fn present(
        self,
        output: impl FnOnce(&PreparedDelivery) -> Result<(), String>,
    ) -> Result<(), String> {
        self.present_with_telemetry(|delivery, _| output(delivery))
    }

    /// Associate an optional renderer attempt with this exact transaction. Diagnostics
    /// never affect acceptance; callers must still return actual synchronous output.
    pub fn present_with_telemetry(
        self,
        output: impl FnOnce(
            &PreparedDelivery,
            &mut Option<bterm_native_renderer::FrameTelemetry>,
        ) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut renderer = None;
        let queue_ns = elapsed_ns(self.queued_at);
        let output_start = self.telemetry.as_ref().map(|_| Instant::now());
        let result = match (self.lifecycle.lock(), self.transaction.lock()) {
            (Ok(lifecycle), Ok(mut transaction)) => {
                transaction.output(lifecycle.closed, self.deadline, || {
                    output(&self.delivery, &mut renderer)
                })
            }
            _ => Err("native presentation synchronization failed".into()),
        };
        let result = result.map_err(|error| {
            format!(
                "presentation {:?} rejected: {error}",
                self.delivery.identity
            )
        });
        let sample = OutputTelemetrySample {
            identity: self.delivery.identity,
            producer_gate_ns: self.producer_gate_ns,
            queue_ns,
            output_ns: elapsed_ns(output_start),
            renderer,
            succeeded: result.is_ok(),
        };
        let _ = self.receipt.send(result.clone());
        // Receipt/authority are independent of diagnostic retention or export.
        if let Some(telemetry) = &self.telemetry
            && let Ok(mut telemetry) = telemetry.lock()
        {
            telemetry.record(sample);
        }
        result
    }
}

struct PendingClipboard {
    request: bmux_cli::attach::NativeClipboardRequest,
    receipt: mpsc::SyncSender<Result<Option<String>, String>>,
    transaction: Arc<Mutex<Transaction>>,
    deadline: Instant,
}

/// Cloneable observer adapter. A shared producer gate enforces one outstanding
/// publication across clones; the bounded deadline includes gate contention.
#[derive(Clone)]
pub struct NativeOutput {
    sender: mpsc::SyncSender<PendingOutput>,
    clipboard: mpsc::SyncSender<PendingClipboard>,
    lifecycle: Arc<Mutex<Lifecycle>>,
    producer: Arc<Mutex<()>>,
    telemetry: SharedTelemetry,
    wake: Arc<dyn Fn() -> Result<()> + Send + Sync>,
}

/// Main-thread receiving half. Dropping it rejects pending output, not runtime
/// execution. No caller can turn its queue admission into output success.
pub struct NativeOutputReceiver {
    receiver: mpsc::Receiver<PendingOutput>,
    clipboard: mpsc::Receiver<PendingClipboard>,
    lifecycle: Arc<Mutex<Lifecycle>>,
    telemetry: SharedTelemetry,
}

#[must_use]
pub fn output_channel(wake: PlatformProxy) -> (NativeOutput, NativeOutputReceiver) {
    output_channel_with_wake(Arc::new(move || {
        wake.wake().map_err(|error| anyhow!(error))
    }))
}

fn output_channel_with_wake(
    wake: Arc<dyn Fn() -> Result<()> + Send + Sync>,
) -> (NativeOutput, NativeOutputReceiver) {
    let (sender, receiver) = mpsc::sync_channel(1);
    let (clipboard_tx, clipboard_rx) = mpsc::sync_channel(1);
    let lifecycle = Arc::new(Mutex::new(Lifecycle::default()));
    let telemetry = std::env::var_os("BTERM_RENDER_TELEMETRY")
        .is_some_and(|value| value == "1")
        .then(|| Arc::new(Mutex::new(OutputTelemetry::default())));
    (
        NativeOutput {
            sender,
            clipboard: clipboard_tx,
            lifecycle: Arc::clone(&lifecycle),
            producer: Arc::new(Mutex::new(())),
            telemetry: telemetry.clone(),
            wake,
        },
        NativeOutputReceiver {
            receiver,
            clipboard: clipboard_rx,
            lifecycle,
            telemetry,
        },
    )
}

impl NativeOutput {
    /// Wake the main thread to observe worker completion or failure.
    pub fn wake(&self) -> Result<()> {
        (self.wake)()
    }
    /// Block off the event-loop thread until actual output or bounded rejection.
    /// On timeout the transaction is cancelled before this method returns.
    pub fn present(&self, delivery: PreparedDelivery) -> Result<()> {
        self.present_until(delivery, Instant::now() + OUTPUT_TIMEOUT)
    }

    fn present_until(&self, delivery: PreparedDelivery, deadline: Instant) -> Result<()> {
        let gate_start = self.telemetry.as_ref().map(|_| Instant::now());
        let _producer = loop {
            self.check(deadline)?;
            match self.producer.try_lock() {
                Ok(guard) => break guard,
                Err(std::sync::TryLockError::WouldBlock) => {
                    std::thread::sleep(Duration::from_millis(2))
                }
                Err(std::sync::TryLockError::Poisoned(_)) => bail!("native output producer failed"),
            }
        };
        let producer_gate_ns = elapsed_ns(gate_start);
        let (receipt, response) = mpsc::sync_channel(1);
        let transaction = Arc::new(Mutex::new(Transaction::default()));
        let pending = PendingOutput {
            delivery,
            receipt,
            transaction: Arc::clone(&transaction),
            lifecycle: Arc::clone(&self.lifecycle),
            deadline,
            telemetry: self.telemetry.clone(),
            producer_gate_ns,
            queued_at: self.telemetry.as_ref().map(|_| Instant::now()),
        };
        self.sender
            .try_send(pending)
            .map_err(|_| anyhow!("native output queue is unavailable"))?;
        if let Err(error) = (self.wake)() {
            if cancel(&transaction) {
                return Err(anyhow!(error));
            }
            // An existing redraw may have completed output before wake failed.
            // As with timeout, completed authority is decided by its receipt.
            return response
                .recv()
                .map_err(|_| anyhow!("native presentation receipt lost"))?
                .map_err(|error| anyhow!(error));
        }
        loop {
            if let Err(error) = self.check(deadline) {
                if cancel(&transaction) {
                    return Err(error);
                }
                // Output began before the deadline and completed while the
                // cancellation gate was held. Preserve its actual result;
                // never report failure after advancing displayed authority.
                return response
                    .recv()
                    .map_err(|_| anyhow!("native presentation receipt lost"))?
                    .map_err(|error| anyhow!(error));
            }
            match response.recv_timeout(Duration::from_millis(10)) {
                Ok(result) => return result.map_err(|error| anyhow!(error)),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("native presentation was not acknowledged")
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    }

    fn clipboard(
        &self,
        request: bmux_cli::attach::NativeClipboardRequest,
    ) -> Result<Option<String>> {
        let deadline = Instant::now() + OUTPUT_TIMEOUT;
        self.check(deadline)?;
        if let bmux_cli::attach::NativeClipboardRequest::WriteText(text) = &request
            && text.len() > bterm_native_platform::clipboard::MAX_CLIPBOARD_BYTES
        {
            bail!("native clipboard payload exceeds transfer budget");
        }
        let (receipt, response) = mpsc::sync_channel(1);
        let transaction = Arc::new(Mutex::new(Transaction::default()));
        self.clipboard
            .try_send(PendingClipboard {
                request,
                receipt,
                transaction: Arc::clone(&transaction),
                deadline,
            })
            .map_err(|_| anyhow!("native clipboard request queue is unavailable"))?;
        if let Err(error) = self.wake() {
            if cancel(&transaction) {
                return Err(error);
            }
            // Clipboard service may complete through an already-dispatched UI
            // event; a subsequent wake failure cannot replace its actual result.
            return response
                .recv()
                .map_err(|_| anyhow!("native clipboard receipt lost"))?
                .map_err(|error| anyhow!(error));
        }
        loop {
            if let Err(error) = self.check(deadline) {
                if cancel(&transaction) {
                    return Err(error);
                }
                return response
                    .recv()
                    .map_err(|_| anyhow!("native clipboard receipt lost"))?
                    .map_err(|error| anyhow!(error));
            }
            match response.recv_timeout(Duration::from_millis(10)) {
                Ok(result) => return result.map_err(|error| anyhow!(error)),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("native clipboard request rejected")
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    }

    pub(crate) fn clipboard_request(
        &self,
        request: bmux_cli::attach::NativeClipboardRequest,
    ) -> Result<Option<String>> {
        self.clipboard(request)
    }

    fn check(&self, deadline: Instant) -> Result<()> {
        if self
            .lifecycle
            .lock()
            .map_err(|_| anyhow!("native output lifecycle failed"))?
            .closed
        {
            bail!("native window detached");
        }
        if Instant::now() >= deadline {
            bail!("native presentation timed out");
        }
        Ok(())
    }
}

fn cancel(transaction: &Mutex<Transaction>) -> bool {
    if let Ok(mut transaction) = transaction.lock() {
        if transaction.completed {
            return false;
        }
        transaction.cancelled = true;
    }
    true
}

impl NativeOutputReceiver {
    /// Bounded, oldest-first UI-serviced attempts. Disabled diagnostics return empty.
    /// Admission failures and dropped/unserviced deliveries are not output attempts.
    pub fn telemetry_samples(&self) -> Vec<OutputTelemetrySample> {
        self.telemetry
            .as_ref()
            .and_then(|telemetry| {
                telemetry
                    .lock()
                    .ok()
                    .map(|telemetry| telemetry.samples.iter().copied().collect())
            })
            .unwrap_or_default()
    }

    /// Lifetime count and latest UI-serviced attempt, without allocating an export.
    pub fn latest_telemetry(&self) -> Option<(u64, OutputTelemetrySample)> {
        let telemetry = self.telemetry.as_ref()?.lock().ok()?;
        Some((telemetry.attempts, *telemetry.samples.back()?))
    }
    /// Service explicit reducer clipboard requests on the event-loop thread.
    /// Cancellation is checked before OS IO; clipboard text is never logged.
    pub(crate) fn service_clipboard(
        &self,
        clipboard: &mut Result<
            bterm_native_platform::clipboard::Clipboard,
            bterm_native_platform::clipboard::ClipboardError,
        >,
    ) {
        if let Ok(pending) = self.clipboard.try_recv() {
            let result = match (self.lifecycle.lock(), pending.transaction.lock()) {
                (Ok(lifecycle), Ok(mut transaction)) => {
                    if lifecycle.closed
                        || transaction.cancelled
                        || Instant::now() >= pending.deadline
                    {
                        transaction.cancelled = true;
                        Err("native clipboard request expired or detached".into())
                    } else {
                        let result = match clipboard {
                            Ok(clipboard) => match pending.request {
                                bmux_cli::attach::NativeClipboardRequest::ReadText => {
                                    clipboard.read_text().map(Some)
                                }
                                bmux_cli::attach::NativeClipboardRequest::WriteText(text) => {
                                    clipboard.write_text(text).map(|()| None)
                                }
                            }
                            .map_err(|error| format!("native clipboard: {error:?}")),
                            Err(error) => Err(format!("native clipboard: {error:?}")),
                        };
                        transaction.completed = true;
                        result
                    }
                }
                _ => Err("native clipboard synchronization failed".into()),
            };
            let _ = pending.receipt.send(result);
        }
    }
    pub fn try_recv(&self) -> Option<PendingOutput> {
        self.receiver.try_recv().ok()
    }
    pub fn close(&self) {
        if let Ok(mut lifecycle) = self.lifecycle.lock() {
            lifecycle.closed = true;
        }
    }
}
impl Drop for NativeOutputReceiver {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Delivery construction belongs to the retained compositor. Cancellation
    // gates themselves can be checked without inventing a second scene model.
    fn delivery() -> PreparedDelivery {
        use bmux_frontend_runtime::presentation::PresentationIdentity;
        use bmux_tui::{damage::Damage, geometry::Rect, presentation::PresentationState};
        PreparedDelivery {
            identity: PresentationIdentity {
                epoch: Default::default(),
                attached_id: Default::default(),
                revision: 1,
                viewport_generation: 1,
            },
            pointer_shape: None,
            prepared: Arc::new(
                PresentationState::new(Rect::new(0, 0, 1, 1))
                    .prepare(Damage::Full, |_| {})
                    .unwrap(),
            ),
        }
    }

    fn queued() -> (NativeOutput, NativeOutputReceiver, mpsc::Receiver<()>) {
        let (notify, notification) = mpsc::sync_channel(1);
        let (output, receiver) = output_channel_with_wake(Arc::new(move || {
            notify.send(()).unwrap();
            Ok(())
        }));
        (output, receiver, notification)
    }

    #[test]
    fn output_telemetry_is_bounded_and_preserves_exact_identity_and_outcome() {
        let mut telemetry = OutputTelemetry::default();
        let mut identity = delivery().identity;
        for revision in 1..=256 {
            identity.revision = revision;
            identity.viewport_generation = revision + 1;
            telemetry.record(OutputTelemetrySample {
                identity,
                producer_gate_ns: 2,
                queue_ns: 3,
                output_ns: 5,
                renderer: None,
                succeeded: revision % 2 == 0,
            });
        }
        assert_eq!(telemetry.attempts, 256);
        assert_eq!(telemetry.samples.len(), TELEMETRY_WINDOW);
        assert_eq!(telemetry.samples.front().unwrap().identity.revision, 129);
        let last = telemetry.samples.back().unwrap();
        assert_eq!(last.identity.revision, 256);
        assert_eq!(last.identity.viewport_generation, 257);
        assert!(last.succeeded);
        assert_eq!(
            (last.producer_gate_ns, last.queue_ns, last.output_ns),
            (2, 3, 5),
        );
    }

    #[test]
    fn renderer_failure_is_correlated_without_changing_receipt() {
        let (mut output, mut receiver, notification) = queued();
        let telemetry = Arc::new(Mutex::new(OutputTelemetry::default()));
        output.telemetry = Some(Arc::clone(&telemetry));
        receiver.telemetry = Some(telemetry);
        let identity = delivery().identity;
        let worker = std::thread::spawn(move || output.present(delivery()));
        notification.recv_timeout(Duration::from_secs(1)).unwrap();
        let frame = bterm_native_renderer::FrameTelemetry {
            surface_acquire_ns: 17,
            gpu_wait_ns: 23,
            ..Default::default()
        };
        let mut pending = receiver.try_recv().unwrap();
        pending.queued_at = Some(Instant::now() - Duration::from_millis(2));
        let result = pending.present_with_telemetry(|_, sample| {
            *sample = Some(frame);
            Err("test failure".into())
        });
        assert_eq!(
            worker.join().unwrap().unwrap_err().to_string(),
            result.unwrap_err(),
        );
        let samples = receiver.telemetry_samples();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].identity, identity);
        assert_eq!(samples[0].renderer, Some(frame));
        assert!(samples[0].queue_ns >= 2_000_000);
        assert!(!samples[0].succeeded);
        assert_eq!(receiver.latest_telemetry(), Some((1, samples[0])));
    }

    #[test]
    fn rejected_transaction_never_inherits_renderer_sample() {
        let (mut output, mut receiver, notification) = queued();
        let telemetry = Arc::new(Mutex::new(OutputTelemetry::default()));
        output.telemetry = Some(Arc::clone(&telemetry));
        receiver.telemetry = Some(telemetry);
        let worker = std::thread::spawn(move || output.present(delivery()));
        notification.recv_timeout(Duration::from_secs(1)).unwrap();
        let pending = receiver.try_recv().unwrap();
        pending.transaction.lock().unwrap().cancelled = true;
        assert!(
            pending
                .present_with_telemetry(|_, _| panic!("cancelled backend"))
                .is_err()
        );
        assert!(worker.join().unwrap().is_err());
        let samples = receiver.telemetry_samples();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].renderer, None);
        assert!(!samples[0].succeeded);
    }

    #[test]
    fn disabled_diagnostics_do_not_retain_output_samples() {
        let (mut output, mut receiver, notification) = queued();
        output.telemetry = None;
        receiver.telemetry = None;
        let worker = std::thread::spawn(move || output.present(delivery()));
        notification.recv_timeout(Duration::from_secs(1)).unwrap();
        receiver.try_recv().unwrap().present(|_| Ok(())).unwrap();
        worker.join().unwrap().unwrap();
        assert_eq!(receiver.telemetry_samples().len(), 0);
        assert_eq!(receiver.latest_telemetry(), None);
        assert_eq!(elapsed_ns(None), 0);
    }

    #[test]
    fn backend_failure_reaches_both_sides_with_frame_identity() {
        let (output, receiver, notification) = queued();
        let worker = std::thread::spawn(move || output.present(delivery()));
        notification.recv_timeout(Duration::from_secs(1)).unwrap();
        let error = receiver
            .try_recv()
            .unwrap()
            .present(|_| Err("GPU device lost: test backend failure".into()))
            .unwrap_err();
        assert!(error.contains("GPU device lost: test backend failure"));
        assert!(error.contains("viewport_generation"));
        assert!(error.contains("revision"));
        assert_eq!(worker.join().unwrap().unwrap_err().to_string(), error);
    }

    #[test]
    fn transaction_blockers_are_distinct_and_never_reach_backend() {
        let future = Instant::now() + Duration::from_secs(1);
        let mut closed = Transaction::default();
        assert_eq!(
            closed.output(true, future, || panic!("closed output")),
            Err("native output receiver is closed".into())
        );
        let mut cancelled = Transaction {
            cancelled: true,
            completed: false,
        };
        assert_eq!(
            cancelled.output(false, future, || panic!("cancelled output")),
            Err("native presentation transaction was cancelled".into())
        );
        let mut expired = Transaction::default();
        assert_eq!(
            expired.output(false, Instant::now(), || panic!("expired output")),
            Err("native presentation deadline expired before output".into())
        );
    }

    #[test]
    fn dropping_pending_rejects_publication_without_waiting_for_timeout() {
        let (output, receiver, notification) = queued();
        let thread = std::thread::spawn(move || output.present(delivery()));
        notification.recv_timeout(Duration::from_secs(1)).unwrap();
        drop(receiver.try_recv().unwrap());
        assert!(thread.join().unwrap().is_err());
    }

    #[test]
    fn closing_after_queue_prevents_backend_and_releases_worker() {
        let (output, receiver, notification) = queued();
        let thread = std::thread::spawn(move || output.present(delivery()));
        notification.recv_timeout(Duration::from_secs(1)).unwrap();
        receiver.close();
        assert!(
            receiver
                .try_recv()
                .unwrap()
                .present(|_| panic!("closed output reached backend"))
                .is_err()
        );
        assert!(thread.join().unwrap().is_err());
    }

    #[test]
    fn timeout_cancels_queued_output_before_late_render() {
        let (output, receiver, notification) = queued();
        let thread = std::thread::spawn(move || {
            output.present_until(delivery(), Instant::now() + Duration::from_millis(50))
        });
        notification.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(thread.join().unwrap().is_err());
        assert!(
            receiver
                .try_recv()
                .unwrap()
                .present(|_| panic!("timed-out output reached backend"))
                .is_err()
        );
    }

    #[test]
    fn stale_viewport_receipt_is_failure_and_next_delivery_can_succeed() {
        let (output, receiver, notification) = queued();
        let producer = output.clone();
        let thread = std::thread::spawn(move || producer.present(delivery()));
        notification.recv_timeout(Duration::from_secs(1)).unwrap();
        let rejected = receiver
            .try_recv()
            .unwrap()
            .present(|delivery| {
                assert_ne!(delivery.identity.viewport_generation, 2);
                Err("stale viewport".into())
            })
            .unwrap_err();
        assert!(rejected.contains("stale viewport"));
        assert!(rejected.contains("viewport_generation"));
        assert_eq!(thread.join().unwrap().unwrap_err().to_string(), rejected);
        let thread = std::thread::spawn(move || output.present(delivery()));
        notification.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(receiver.try_recv().unwrap().present(|_| Ok(())).is_ok());
        assert!(thread.join().unwrap().is_ok());
    }

    #[test]
    fn completed_output_spanning_deadline_returns_actual_success() {
        // Hold the producer in its wake callback until output is complete. This
        // forces its first receipt-loop check to observe an expired deadline.
        let (notified, notification) = mpsc::sync_channel(1);
        let (release, released) = mpsc::sync_channel(1);
        let released = Mutex::new(released);
        let (output, receiver) = output_channel_with_wake(Arc::new(move || {
            notified.send(()).unwrap();
            released.lock().unwrap().recv().unwrap();
            Ok(())
        }));
        let deadline = Instant::now() + Duration::from_secs(2);
        let worker = std::thread::spawn(move || output.present_until(delivery(), deadline));
        notification.recv_timeout(Duration::from_secs(1)).unwrap();
        let accepted = receiver.try_recv().unwrap().present(|_| {
            // Presentation has started under the cancellation gate. Complete
            // only after the worker's deadline, as a slow GPU may do.
            std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
            Ok(())
        });
        assert!(accepted.is_ok());
        release.send(()).unwrap();
        assert!(worker.join().unwrap().is_ok());
    }

    #[test]
    fn close_after_completed_output_before_receipt_consumption_keeps_success() {
        let (notified, notification) = mpsc::sync_channel(1);
        let (release, released) = mpsc::sync_channel(1);
        let released = Mutex::new(released);
        let (output, receiver) = output_channel_with_wake(Arc::new(move || {
            notified.send(()).unwrap();
            released.lock().unwrap().recv().unwrap();
            Ok(())
        }));
        let worker = std::thread::spawn(move || output.present(delivery()));
        notification.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(receiver.try_recv().unwrap().present(|_| Ok(())).is_ok());
        receiver.close();
        release.send(()).unwrap();
        assert!(worker.join().unwrap().is_ok());
    }

    #[test]
    fn completed_output_cannot_be_reclassified_as_timeout() {
        let transaction = Mutex::new(Transaction::default());
        transaction
            .lock()
            .unwrap()
            .output(false, Instant::now() + Duration::from_secs(1), || Ok(()))
            .unwrap();
        assert!(!cancel(&transaction));
        assert!(!transaction.lock().unwrap().cancelled);
    }

    #[test]
    fn cloned_producers_cannot_admit_second_output_while_first_is_inflight() {
        let (output, receiver, notification) = queued();
        let producer = output.clone();
        let first = std::thread::spawn(move || producer.present(delivery()));
        notification.recv_timeout(Duration::from_secs(1)).unwrap();
        let pending = receiver.try_recv().unwrap();
        let second = std::thread::spawn(move || {
            output.present_until(delivery(), Instant::now() + Duration::from_millis(50))
        });
        assert!(second.join().unwrap().is_err());
        assert!(receiver.try_recv().is_none());
        assert!(pending.present(|_| Ok(())).is_ok());
        assert!(first.join().unwrap().is_ok());
    }

    #[test]
    fn wake_failure_after_output_completion_preserves_actual_receipt() {
        for backend in [Ok(()), Err("completed backend failure".to_string())] {
            let receiver_slot = Arc::new(Mutex::new(None::<NativeOutputReceiver>));
            let observed = Arc::new(Mutex::new(None));
            let wake_receiver = Arc::clone(&receiver_slot);
            let wake_observed = Arc::clone(&observed);
            let (output, receiver) = output_channel_with_wake(Arc::new(move || {
                // Deterministically complete through the existing receiver before
                // returning the wake failure; no scheduling delays or sleeps.
                let guard = wake_receiver.lock().unwrap();
                let pending = guard.as_ref().unwrap().try_recv().unwrap();
                *wake_observed.lock().unwrap() = Some(pending.present(|_| backend.clone()));
                bail!("wake failed after output");
            }));
            *receiver_slot.lock().unwrap() = Some(receiver);
            let actual = output
                .present(delivery())
                .map_err(|error| error.to_string());
            assert_eq!(actual, observed.lock().unwrap().take().unwrap());
        }
    }

    #[test]
    fn wake_failure_after_clipboard_completion_preserves_actual_receipt() {
        for backend in [
            Ok(Some("read result".to_string())),
            Err("clipboard failure".to_string()),
        ] {
            let receiver_slot = Arc::new(Mutex::new(None::<NativeOutputReceiver>));
            let wake_receiver = Arc::clone(&receiver_slot);
            let expected = backend.clone();
            let (output, receiver) = output_channel_with_wake(Arc::new(move || {
                let guard = wake_receiver.lock().unwrap();
                let receiver = guard.as_ref().unwrap();
                let pending = receiver.clipboard.try_recv().unwrap();
                // Mock only OS clipboard IO, not queueing or the completion gate.
                // Service and send the actual result before wake reports failure.
                let result = {
                    let lifecycle = receiver.lifecycle.lock().unwrap();
                    let mut transaction = pending.transaction.lock().unwrap();
                    transaction
                        .output(lifecycle.closed, pending.deadline, || {
                            backend.clone().map(|_| ())
                        })
                        .map(|()| backend.clone().unwrap())
                };
                pending.receipt.send(result).unwrap();
                bail!("wake failed after clipboard service");
            }));
            *receiver_slot.lock().unwrap() = Some(receiver);
            let actual = output
                .clipboard_request(bmux_cli::attach::NativeClipboardRequest::ReadText)
                .map_err(|error| error.to_string());
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn wake_failure_before_service_cancels_output_and_clipboard() {
        let (output, receiver) = output_channel_with_wake(Arc::new(|| bail!("wake failed")));
        assert_eq!(
            output.present(delivery()).unwrap_err().to_string(),
            "wake failed",
        );
        assert!(
            receiver
                .try_recv()
                .unwrap()
                .present(|_| panic!("cancelled output"))
                .is_err()
        );
        assert_eq!(
            output
                .clipboard_request(bmux_cli::attach::NativeClipboardRequest::ReadText)
                .unwrap_err()
                .to_string(),
            "wake failed",
        );
        let pending = receiver.clipboard.try_recv().unwrap();
        assert!(pending.transaction.lock().unwrap().cancelled);
        assert!(!pending.transaction.lock().unwrap().completed);
    }

    #[test]
    fn clipboard_queue_admission_is_not_service_success() {
        let (output, receiver, notification) = queued();
        let worker = std::thread::spawn(move || {
            output.clipboard_request(bmux_cli::attach::NativeClipboardRequest::ReadText)
        });
        notification.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(!worker.is_finished());
        receiver.service_clipboard(&mut Err(
            bterm_native_platform::clipboard::ClipboardError::Unavailable,
        ));
        assert!(worker.join().unwrap().is_err());
    }

    #[test]
    fn close_releases_queued_clipboard_request_without_os_io() {
        let (output, receiver, notification) = queued();
        let worker = std::thread::spawn(move || {
            output.clipboard_request(bmux_cli::attach::NativeClipboardRequest::WriteText(
                "private".into(),
            ))
        });
        notification.recv_timeout(Duration::from_secs(1)).unwrap();
        receiver.close();
        assert!(worker.join().unwrap().is_err());
        receiver.service_clipboard(&mut Err(
            bterm_native_platform::clipboard::ClipboardError::Unavailable,
        ));
    }

    #[test]
    fn cancellation_is_sticky() {
        let transaction = Mutex::new(Transaction::default());
        cancel(&transaction);
        assert!(transaction.lock().unwrap().cancelled);
        cancel(&transaction);
        assert!(transaction.lock().unwrap().cancelled);
    }

    #[test]
    fn closed_or_expired_output_never_invokes_backend() {
        let mut transaction = Transaction::default();
        let mut called = false;
        assert!(
            transaction
                .output(true, Instant::now() + Duration::from_secs(1), || {
                    called = true;
                    Ok(())
                })
                .is_err()
        );
        assert!(!called);
        let mut transaction = Transaction::default();
        assert!(
            transaction
                .output(false, Instant::now(), || {
                    called = true;
                    Ok(())
                })
                .is_err()
        );
        assert!(!called);
        assert!(
            transaction
                .output(false, Instant::now() + Duration::from_secs(1), || {
                    called = true;
                    Ok(())
                })
                .is_err()
        );
        assert!(!called);
    }

    #[test]
    fn successful_and_failed_backend_results_are_not_conflated() {
        let mut transaction = Transaction::default();
        assert!(
            transaction
                .output(false, Instant::now() + Duration::from_secs(1), || Err(
                    "GPU lost".into()
                ))
                .is_err()
        );
        assert!(
            transaction
                .output(false, Instant::now() + Duration::from_secs(1), || Ok(()))
                .is_ok()
        );
    }
}
