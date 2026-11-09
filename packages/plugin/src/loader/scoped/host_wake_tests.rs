//! Operations are driven by real wakers: static futures and native guests that
//! negotiated `CAP_HOST_WAKE_V1` complete from their own wake, with no
//! observer polling interval and no observer deadline. Legacy native guests
//! keep their ABI-mandated periodic repoll; unknown bits stay rejected.
use super::activation_parity_tests::{Recorder, lifecycle_context, metadata, selected_execution};
use super::*;
use bmux_plugin_sdk::definition::{
    PluginDefinition, PluginFuture, PluginInstance, PluginLifecycleContext, PluginResult,
};
use std::time::{Duration, Instant};

/// Completes after `delay` on a foreign thread, waking only the registered waker.
async fn completed_elsewhere(delay: Duration) {
    let state = Arc::new(Mutex::new((false, None::<Waker>)));
    let worker = Arc::clone(&state);
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        let waker = {
            let mut state = worker.lock().unwrap();
            state.0 = true;
            state.1.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    });
    std::future::poll_fn(|cx| {
        let mut state = state.lock().unwrap();
        if state.0 {
            Poll::Ready(())
        } else {
            state.1 = Some(cx.waker().clone());
            Poll::Pending
        }
    })
    .await;
}

const DELAY: Duration = Duration::from_millis(60);

struct Delayed;
impl PluginInstance for Delayed {
    fn activate(&self, _: PluginLifecycleContext) -> PluginFuture<'_, PluginResult<()>> {
        Box::pin(async {
            completed_elsewhere(DELAY).await;
            Ok(())
        })
    }
    fn extension(
        &self,
        call: PluginCallContext,
        contract: String,
        _: Vec<u8>,
    ) -> PluginFuture<'_, PluginResult<Vec<u8>>> {
        Box::pin(async move {
            match contract.as_str() {
                // Paused-clock long operation: far beyond any former cap.
                "long" => tokio::time::sleep(Duration::from_secs(45)).await,
                // Cooperative cancellation that registers no waker of its own.
                "cooperative" => {
                    std::future::poll_fn(|_| {
                        if call.cancellation.is_cancelled() {
                            Poll::Ready(())
                        } else {
                            Poll::Pending
                        }
                    })
                    .await;
                }
                _ => completed_elsewhere(DELAY).await,
            }
            Ok(contract.into_bytes())
        })
    }
}

fn definition() -> PluginDefinition {
    PluginDefinition::new(metadata(), |_| {
        Ok(Arc::new(Delayed) as Arc<dyn PluginInstance>)
    })
}

mod native {
    use super::abi;
    use std::sync::OnceLock;

    fn adapter() -> &'static abi::NativeAdapter {
        static ADAPTER: OnceLock<abi::NativeAdapter> = OnceLock::new();
        ADAPTER.get_or_init(|| abi::NativeAdapter::new(super::definition()))
    }
    unsafe extern "C" fn declarations(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
        abi::ffi_guard(|| unsafe { adapter().declarations(out, cap, len) })
    }
    unsafe extern "C" fn metadata(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
        abi::ffi_guard(|| unsafe { adapter().metadata(out, cap, len) })
    }
    unsafe extern "C" fn create(input: *const u8, len: usize, out: *mut u64) -> i32 {
        abi::ffi_guard(|| unsafe { adapter().create(input, len, out) })
    }
    unsafe extern "C" fn destroy(id: u64) -> i32 {
        abi::ffi_guard(|| adapter().destroy(id))
    }
    unsafe extern "C" fn start(id: u64, op: u64, input: *const u8, len: usize) -> i32 {
        abi::ffi_guard(|| unsafe { adapter().start(id, op, input, len) })
    }
    unsafe extern "C" fn start_scoped(
        id: u64,
        op: u64,
        input: *const u8,
        len: usize,
        ports: *const abi::NativeInvocationPorts,
    ) -> i32 {
        abi::ffi_guard(|| unsafe { adapter().start_scoped(id, op, input, len, ports) })
    }
    unsafe extern "C" fn read(id: u64, op: u64, out: *mut u8, cap: usize, len: *mut usize) -> i32 {
        abi::ffi_guard(|| unsafe { adapter().read(id, op, out, cap, len) })
    }
    unsafe extern "C" fn read_wake(
        id: u64,
        op: u64,
        out: *mut u8,
        cap: usize,
        len: *mut usize,
        waker: *const abi::NativeHostWakerV1,
    ) -> i32 {
        abi::ffi_guard(|| unsafe { adapter().read_wake(id, op, out, cap, len, waker) })
    }
    unsafe extern "C" fn cancel(id: u64, op: u64) -> i32 {
        abi::ffi_guard(|| adapter().cancel(id, op))
    }
    unsafe extern "C" fn ack(id: u64, op: u64) -> i32 {
        abi::ffi_guard(|| adapter().ack(id, op))
    }
    const fn descriptor(size: u32, capabilities: u64) -> abi::NativePluginDescriptorV3HostWakeV1 {
        abi::NativePluginDescriptorV3HostWakeV1 {
            base: abi::NativePluginDescriptorV3 {
                base: abi::NativePluginDescriptor {
                    size,
                    version: abi::ABI_VERSION,
                    capabilities,
                    start_scoped,
                    metadata,
                    create,
                    destroy,
                    start,
                    read,
                    cancel,
                    ack,
                },
                declarations,
            },
            read_wake,
        }
    }
    pub(super) static WAKE: abi::NativePluginDescriptorV3HostWakeV1 = descriptor(
        abi::NATIVE_DESCRIPTOR_SIZE_HOST_WAKE_V1,
        abi::DESCRIPTOR_CAPABILITIES,
    );
    /// Legacy guest: same code, no wake bit, frozen v3 size.
    pub(super) static LEGACY: abi::NativePluginDescriptorV3HostWakeV1 =
        descriptor(abi::NATIVE_DESCRIPTOR_SIZE, abi::SCOPED_CAPABILITIES);
    /// Advertises the wake bit without the tail it requires.
    pub(super) static WAKE_WITHOUT_TAIL: abi::NativePluginDescriptorV3HostWakeV1 =
        descriptor(abi::NATIVE_DESCRIPTOR_SIZE, abi::DESCRIPTOR_CAPABILITIES);
    /// Advertises a bit beyond host wake that this host does not understand.
    pub(super) static UNKNOWN: abi::NativePluginDescriptorV3HostWakeV1 = descriptor(
        abi::NATIVE_DESCRIPTOR_SIZE_HOST_WAKE_V1,
        abi::DESCRIPTOR_CAPABILITIES | (abi::CAP_HOST_WAKE_V1 << 1),
    );
}

/// Observe one operation through its waker only, counting polls.
async fn observe(execution: &LoadedExecution, id: u64) -> (NativeOutcome, usize) {
    let signal = OperationSignal::new();
    let waker = signal.waker();
    let mut polls = 0;
    loop {
        polls += 1;
        if let Poll::Ready(outcome) = execution
            .poll(id, &mut Context::from_waker(&waker))
            .unwrap()
        {
            return (outcome, polls);
        }
        signal.wait().await.unwrap();
    }
}

async fn activate(execution: &Arc<LoadedExecution>) -> usize {
    let started = Instant::now();
    let activation = execution
        .begin_activation(lifecycle_context(), PluginConcurrencyConfig::Concurrent)
        .unwrap();
    // Test guard only: a lost wake fails the test instead of hanging it.
    let (outcome, polls) =
        tokio::time::timeout(Duration::from_secs(10), observe(execution, activation))
            .await
            .expect("activation is woken");
    assert!(matches!(outcome, NativeOutcome::Unit));
    assert!(started.elapsed() >= DELAY);
    execution.acknowledge(activation).await.unwrap();
    polls
}

async fn retire(execution: Arc<LoadedExecution>) {
    let deactivation = execution
        .begin_deactivation(lifecycle_context())
        .await
        .unwrap();
    let (outcome, _) =
        tokio::time::timeout(Duration::from_secs(10), observe(&execution, deactivation))
            .await
            .expect("deactivation is woken");
    assert!(matches!(outcome, NativeOutcome::Unit));
    execution.acknowledge(deactivation).await.unwrap();
    execution.destroy().await.unwrap();
}

fn load(
    descriptor: &'static abi::NativePluginDescriptorV3HostWakeV1,
) -> Result<DefinitionArtifact> {
    // SAFETY: the descriptor is this test binary's own in-process adapter,
    // owned by the process handle for the artifact's whole lifetime.
    unsafe { DefinitionArtifact::from_process_descriptor(&raw const descriptor.base.base) }
}

fn negotiated_wake(artifact: &DefinitionArtifact) -> bool {
    let ArtifactBackend::Dynamic(dynamic) = &artifact.backend else {
        panic!("dynamic artifact expected");
    };
    dynamic.read_wake.is_some()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn negotiated_guest_completes_from_its_own_wake_without_repolling() {
    let artifact = load(&native::WAKE).unwrap();
    assert!(negotiated_wake(&artifact));
    let execution = selected_execution(&artifact, &Arc::new(Recorder::default()));
    let polls = activate(&execution).await;
    // A 2ms repoll would need ~30 polls over the 60ms delay. Waker-driven
    // observation polls once before the guest wake and a bounded few after
    // (settlement and terminal drain), independent of elapsed time.
    assert!(polls <= 6, "observed {polls} polls");
    retire(execution).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_guest_without_wake_bit_keeps_explicit_periodic_repoll() {
    let artifact = load(&native::LEGACY).unwrap();
    assert!(!negotiated_wake(&artifact));
    let execution = selected_execution(&artifact, &Arc::new(Recorder::default()));
    let polls = activate(&execution).await;
    // The guest polls its future with a no-op waker; only the documented
    // legacy repoll drives it, so completion takes many timer-driven polls.
    assert!(polls > 6, "observed {polls} polls");
    retire(execution).await;
}

#[test]
fn wake_bit_without_tail_and_unknown_bits_are_rejected_at_load() {
    for descriptor in [&native::WAKE_WITHOUT_TAIL, &native::UNKNOWN] {
        let error = load(descriptor)
            .err()
            .expect("malformed wake negotiation is rejected")
            .to_string();
        assert!(error.contains("unsupported native descriptor"), "{error}");
    }
}

fn static_execution() -> Arc<LoadedExecution> {
    let execution = DefinitionArtifact::from_definition(definition())
        .create(PluginRuntimeScopeId::new(), ComponentRole::Backend, &[])
        .unwrap();
    let activation = execution
        .scope
        .begin_activation(PluginConcurrencyConfig::Concurrent)
        .unwrap();
    activation.mark_active().unwrap();
    execution.state.lock().unwrap().activation = Some(activation);
    execution
}

fn extension(execution: &LoadedExecution, contract: &str) -> u64 {
    execution
        .begin(NativeOperation::Extension {
            invocation_id: PluginInvocationId::new(),
            contract: contract.into(),
            payload: vec![],
        })
        .unwrap()
}

async fn retire_static(execution: Arc<LoadedExecution>) {
    let activation = execution.state.lock().unwrap().activation.take().unwrap();
    activation.close().unwrap();
    activation.drain().await.unwrap();
    activation.finish().unwrap();
    execution.destroy().await.unwrap();
}

#[tokio::test]
async fn static_operation_completes_from_its_own_wake_without_repolling() {
    let execution = static_execution();
    let id = extension(&execution, "delayed");
    let started = Instant::now();
    let (outcome, polls) = tokio::time::timeout(Duration::from_secs(10), observe(&execution, id))
        .await
        .expect("operation is woken");
    assert!(started.elapsed() >= DELAY);
    assert!(matches!(outcome, NativeOutcome::Bytes(bytes) if bytes == b"delayed"));
    // Pending, woken once by the author future, then settlement: no interval.
    assert!(polls <= 4, "observed {polls} polls");
    execution.acknowledge(id).await.unwrap();
    retire_static(execution).await;
}

#[tokio::test(start_paused = true)]
async fn static_operation_far_beyond_former_cap_completes() {
    let execution = static_execution();
    let id = extension(&execution, "long");
    let started = tokio::time::Instant::now();
    // No timeout wrapper: the paused clock auto-advances only while every task
    // waits, so this completes exactly when the author's 45s timer fires.
    let (outcome, polls) = observe(&execution, id).await;
    assert!(started.elapsed() >= Duration::from_secs(45));
    assert!(matches!(outcome, NativeOutcome::Bytes(bytes) if bytes == b"long"));
    assert!(polls <= 4, "observed {polls} polls");
    execution.acknowledge(id).await.unwrap();
    retire_static(execution).await;
}

#[tokio::test]
async fn cancellation_wakes_observer_of_cooperative_operation() {
    let execution = static_execution();
    let id = extension(&execution, "cooperative");
    let observer = {
        let execution = Arc::clone(&execution);
        tokio::spawn(async move { observe(&execution, id).await })
    };
    // Let the observer reach its waker-driven wait before cancelling.
    while execution
        .call(id)
        .unwrap()
        .observer
        .lock()
        .unwrap()
        .is_none()
    {
        tokio::task::yield_now().await;
    }
    execution.cancel(id).unwrap();
    let (outcome, _) = tokio::time::timeout(Duration::from_secs(10), observer)
        .await
        .expect("cancellation wakes the observer")
        .unwrap();
    assert!(matches!(outcome, NativeOutcome::Bytes(bytes) if bytes == b"cooperative"));
    execution.acknowledge(id).await.unwrap();
    retire_static(execution).await;
}

#[tokio::test]
async fn observer_racing_a_poll_is_woken_when_the_slot_is_released() {
    let execution = static_execution();
    let id = extension(&execution, "delayed");
    let call = execution.call(id).unwrap();
    let held = std::mem::replace(&mut *call.state.lock().unwrap(), CallState::Polling);
    let signal = OperationSignal::new();
    let waker = signal.waker();
    assert!(
        execution
            .poll(id, &mut Context::from_waker(&waker))
            .unwrap()
            .is_pending()
    );
    // The racing poller restores the slot, then releases contenders.
    *call.state.lock().unwrap() = held;
    drop(ReleaseContended(&call));
    tokio::time::timeout(Duration::from_secs(10), signal.wait())
        .await
        .expect("contended observer is woken")
        .unwrap();
    let (outcome, _) = observe(&execution, id).await;
    assert!(matches!(outcome, NativeOutcome::Bytes(_)));
    execution.acknowledge(id).await.unwrap();
    retire_static(execution).await;
}
