//! Import-free Mermaid guest execution with bounded, per-store interruption.

use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use wasmtime::{Engine, Instance, Module, Store, StoreLimits, StoreLimitsBuilder, UpdateDeadline};

use crate::{
    MermaidCancellationToken, MermaidRenderError, MermaidRenderRequest, MermaidRendered,
    MermaidRenderedOutput, RENDER_PERMITS, RenderPermit, svg_dimensions, validate_request,
};

#[path = "../wasm_config.rs"]
mod wasm_config;

const MEMORY_BYTES: usize = 512 * 1024 * 1024;
const SOURCE_BYTES: usize = 64 * 1024;
const OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const MODULE_BYTES: usize = 16 * 1024 * 1024;
const EPOCH_INTERVAL: Duration = Duration::from_millis(5);
const MAX_ASYNC_JOBS: usize = 8;
static ASYNC_JOBS: AtomicUsize = AtomicUsize::new(0);

/// Embedded Mermaid renderer. Clones share compiled code, never guest state.
///
/// Each call creates a fresh bounded store. Cancellation and the end-to-end
/// deadline belong to that store; advancing the engine epoch only prompts stores
/// to check their own state. A concurrency permit is held until the store drops.
#[derive(Clone)]
pub struct WasmMermaidRenderer(Arc<Runtime>);

struct Runtime {
    engine: Engine,
    module: Module,
    fuel: Option<u64>,
    #[cfg(test)]
    probe: Arc<ExecutionProbe>,
    _ticker: EpochTicker,
}

struct Execution {
    limits: StoreLimits,
    cancellation: MermaidCancellationToken,
    job_cancellation: MermaidCancellationToken,
    deadline: Instant,
    #[cfg(test)]
    probe: Arc<ExecutionProbe>,
}

impl Execution {
    fn check(&self) -> Result<(), MermaidRenderError> {
        if self.cancellation.is_cancelled() || self.job_cancellation.is_cancelled() {
            Err(MermaidRenderError::Cancelled)
        } else if Instant::now() >= self.deadline {
            Err(MermaidRenderError::TimedOut)
        } else {
            Ok(())
        }
    }
}

impl WasmMermaidRenderer {
    /// Load the trusted build-produced AOT guest without filesystem access.
    /// A configuration/target mismatch is an error, never a JIT fallback.
    ///
    /// # Errors
    /// Returns an error if the embedded artifact or runtime cannot be initialized.
    pub fn new() -> Result<Self, MermaidRenderError> {
        Self::load_embedded(None)
    }

    /// Compile the retained portable WASM with the shared destination-baseline config.
    /// Each call compiles anew; use clones to share compiled code on warm paths.
    ///
    /// # Errors
    /// Returns guest compilation or runtime initialization errors.
    pub fn new_jit() -> Result<Self, MermaidRenderError> {
        Self::from_module_bytes(include_bytes!(concat!(env!("OUT_DIR"), "/mermaid.wasm")))
    }

    /// Compile an explicitly supplied binary guest with the same sandbox bounds.
    ///
    /// Intended for ABI validation and synthetic trap/cancellation tests. No WASI
    /// or other host imports are accepted. Compilation is outside render deadlines;
    /// callers should initialize once, not compile on an interactive render path.
    ///
    /// # Errors
    /// Rejects oversized/invalid modules, imports, and runtime initialization failures.
    pub fn from_module_bytes(bytes: &[u8]) -> Result<Self, MermaidRenderError> {
        Self::compile(bytes, None)
    }

    /// Load the embedded fuel-instrumented AOT guest with a per-call budget.
    /// Epoch cancellation/deadlines remain active. Fuel exhaustion is a guest trap.
    /// This loads a distinct renderer; clones retain the same fuel budget.
    ///
    /// # Errors
    /// Returns initialization errors or rejects a zero fuel budget.
    pub fn new_with_fuel(fuel: u64) -> Result<Self, MermaidRenderError> {
        if fuel == 0 {
            return Err(MermaidRenderError::InvalidExecutionLimits);
        }
        Self::load_embedded(Some(fuel))
    }

    /// Compile the portable guest with fuel instrumentation and a per-call budget.
    ///
    /// # Errors
    /// Returns initialization errors or rejects a zero fuel budget.
    pub fn new_jit_with_fuel(fuel: u64) -> Result<Self, MermaidRenderError> {
        Self::from_module_bytes_with_fuel(
            include_bytes!(concat!(env!("OUT_DIR"), "/mermaid.wasm")),
            fuel,
        )
    }

    /// Compile an import-free binary test guest with optional benchmark fuel.
    ///
    /// # Errors
    /// Returns initialization errors or rejects a zero fuel budget.
    pub fn from_module_bytes_with_fuel(
        bytes: &[u8],
        fuel: u64,
    ) -> Result<Self, MermaidRenderError> {
        if fuel == 0 {
            return Err(MermaidRenderError::InvalidExecutionLimits);
        }
        Self::compile(bytes, Some(fuel))
    }

    fn compile(bytes: &[u8], fuel: Option<u64>) -> Result<Self, MermaidRenderError> {
        if bytes.len() > MODULE_BYTES {
            return Err(unavailable(
                "WASM module exceeds the fixed compilation size bound",
            ));
        }
        let engine = Self::engine(fuel.is_some())?;
        let module =
            Module::from_binary(&engine, bytes).map_err(|error| unavailable(&error.to_string()))?;
        Self::initialize(engine, module, fuel)
    }

    fn engine(fuel: bool) -> Result<Engine, MermaidRenderError> {
        let config = wasm_config::config(env!("BMUX_MERMAID_WASM_TARGET"), fuel)
            .map_err(|error| unavailable(&error.to_string()))?;
        Engine::new(&config).map_err(|error| unavailable(&error.to_string()))
    }

    fn load_embedded(fuel: Option<u64>) -> Result<Self, MermaidRenderError> {
        let engine = Self::engine(fuel.is_some())?;
        let module = deserialize_embedded(&engine, fuel.is_some()).map_err(|error| {
            unavailable(&format!("embedded Mermaid WASM AOT load failed: {error}"))
        })?;
        Self::initialize(engine, module, fuel)
    }

    fn initialize(
        engine: Engine,
        module: Module,
        fuel: Option<u64>,
    ) -> Result<Self, MermaidRenderError> {
        if module.imports().next().is_some() {
            return Err(unavailable(
                "Mermaid WASM guest must not import host services",
            ));
        }
        let ticker = EpochTicker::new(engine.clone())?;
        Ok(Self(Arc::new(Runtime {
            engine,
            module,
            fuel,
            #[cfg(test)]
            probe: Arc::new(ExecutionProbe::default()),
            _ticker: ticker,
        })))
    }

    /// Render synchronously, settling guest execution before returning.
    ///
    /// No detached work survives return, including cancellation, traps, and ABI
    /// failures. The guest receives UTF-8 source only; dimensions are validated on
    /// the host and cannot be passed to this version of the guest ABI. Cache keys
    /// preserve the shared render contract; migration must gate output parity
    /// before mixing process and WASM results in the same cache.
    ///
    /// # Errors
    /// Returns request validation, cancellation, timeout, guest, or ABI errors.
    pub fn render(
        &self,
        request: &MermaidRenderRequest,
        cancellation: &MermaidCancellationToken,
    ) -> Result<MermaidRendered, MermaidRenderError> {
        self.render_job(
            request,
            cancellation,
            &MermaidCancellationToken::default(),
            Instant::now(),
        )
    }

    /// Render off the executor thread, with cancellation-on-drop and settlement.
    ///
    /// Drop only signals the private job token; a Tokio supervisor awaits the
    /// blocking worker through actual settlement. The worker retains its store,
    /// render permit and admission slot, including after caller drop. A running
    /// Tokio runtime is required. At most eight async jobs (including permit
    /// waiters) exist across renderers; excess calls fail explicitly.
    ///
    /// # Errors
    /// Returns sync render errors, worker failures, or admission backpressure.
    pub async fn render_async(
        &self,
        request: &MermaidRenderRequest,
        cancellation: &MermaidCancellationToken,
    ) -> Result<MermaidRendered, MermaidRenderError> {
        let started = Instant::now();
        validate_request(request, cancellation)?;
        if request.source.as_str().len() > SOURCE_BYTES {
            return Err(MermaidRenderError::SourceTooLarge {
                actual: request.source.as_str().len(),
                maximum: SOURCE_BYTES,
            });
        }
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| unavailable("render_async requires a running Tokio runtime"))?;
        let admission = AsyncAdmission::acquire()?;
        let renderer = self.clone();
        let request = request.clone();
        let cancellation = cancellation.clone();
        let job_cancellation = MermaidCancellationToken::default();
        let worker_cancellation = job_cancellation.clone();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let guard = AsyncJob {
            cancellation: job_cancellation,
        };
        let worker = runtime.spawn_blocking(move || {
            let _admission = admission;
            renderer.render_job(&request, &cancellation, &worker_cancellation, started)
        });
        let _supervisor = runtime.spawn(async move {
            let result = worker
                .await
                .unwrap_or(Err(MermaidRenderError::BackendPanicked));
            let _ = sender.send(result);
        });
        let result = receiver
            .await
            .map_err(|_| MermaidRenderError::BackendPanicked)?;
        drop(guard);
        result
    }

    fn render_job(
        &self,
        request: &MermaidRenderRequest,
        cancellation: &MermaidCancellationToken,
        job_cancellation: &MermaidCancellationToken,
        started: Instant,
    ) -> Result<MermaidRendered, MermaidRenderError> {
        validate_request(request, cancellation)?;
        let deadline = started
            .checked_add(request.limits.timeout)
            .ok_or(MermaidRenderError::InvalidExecutionLimits)?;
        let source = request.source.as_str().as_bytes();
        let maximum = request.limits.max_source_bytes.min(SOURCE_BYTES);
        if source.len() > maximum {
            return Err(MermaidRenderError::SourceTooLarge {
                actual: source.len(),
                maximum,
            });
        }
        let _permit = acquire_permit(
            request.limits.max_concurrent_renders.min(2),
            deadline,
            cancellation,
            job_cancellation,
        )?;
        let execution = Execution {
            limits: StoreLimitsBuilder::new()
                .memory_size(MEMORY_BYTES)
                .table_elements(10_000)
                .instances(1)
                .memories(1)
                .tables(4)
                .trap_on_grow_failure(true)
                .build(),
            cancellation: cancellation.clone(),
            job_cancellation: job_cancellation.clone(),
            deadline,
            #[cfg(test)]
            probe: self.0.probe.clone(),
        };
        execution.check()?;
        let mut store = Store::new(&self.0.engine, execution);
        store.limiter(|execution| &mut execution.limits);
        if let Some(fuel) = self.0.fuel {
            store
                .set_fuel(fuel)
                .map_err(|error| unavailable(&error.to_string()))?;
        }
        store.set_epoch_deadline(1);
        store.epoch_deadline_callback(|context| {
            #[cfg(test)]
            context
                .data()
                .probe
                .epochs
                .lock()
                .unwrap()
                .insert(thread::current().id());
            if context.data().check().is_err() {
                // Use Wasmtime's epoch interrupt trap; the store's own state
                // classifies it as cancellation or deadline expiry on return.
                Ok(UpdateDeadline::Interrupt)
            } else {
                Ok(UpdateDeadline::Continue(1))
            }
        });
        let result = self.execute(&mut store, request);
        store.data().check()?;
        result
    }

    fn execute(
        &self,
        store: &mut Store<Execution>,
        request: &MermaidRenderRequest,
    ) -> Result<MermaidRendered, MermaidRenderError> {
        let instance = Instance::new(&mut *store, &self.0.module, &[])
            .map_err(|error| guest_error(store, &error.to_string()))?;
        let memory = instance
            .get_memory(&mut *store, "memory")
            .ok_or_else(|| invalid_abi("missing memory export"))?;
        let memory_type = memory.ty(&*store);
        if memory_type.is_64() || memory_type.is_shared() {
            return Err(invalid_abi("guest memory must be unshared wasm32 memory"));
        }
        let alloc = instance
            .get_typed_func::<u32, u32>(&mut *store, "alloc")
            .map_err(|_| invalid_abi("alloc must have signature (i32) -> i32"))?;
        let dealloc = instance
            .get_typed_func::<(u32, u32), ()>(&mut *store, "dealloc")
            .map_err(|_| invalid_abi("dealloc must have signature (i32, i32) -> ()"))?;
        let render = instance
            .get_typed_func::<(u32, u32), u64>(&mut *store, "render")
            .map_err(|_| invalid_abi("render must have signature (i32, i32) -> i64"))?;
        let source = request.source.as_str().as_bytes();
        let source_len =
            u32::try_from(source.len()).map_err(|_| invalid_abi("source length exceeds wasm32"))?;
        let input_ptr = alloc
            .call(&mut *store, source_len)
            .map_err(|error| guest_error(store, &error.to_string()))?;
        memory
            .write(&mut *store, input_ptr as usize, source)
            .map_err(|_| invalid_abi("allocated input range is outside guest memory"))?;
        let packed = render
            .call(&mut *store, (input_ptr, source_len))
            .map_err(|error| guest_error(store, &error.to_string()))?;
        let output_ptr = u32::try_from(packed >> 32)
            .map_err(|_| invalid_abi("output pointer exceeds wasm32"))?;
        let output_len = u32::try_from(packed & u64::from(u32::MAX))
            .map_err(|_| invalid_abi("output length exceeds wasm32"))?;
        let maximum = request.limits.max_output_bytes.min(OUTPUT_BYTES);
        let payload_len = (output_len as usize)
            .checked_sub(1)
            .ok_or_else(|| invalid_abi("guest returned an empty response"))?;
        if payload_len > maximum {
            return Err(MermaidRenderError::OutputTooLarge {
                actual: payload_len,
                maximum,
            });
        }
        let output_start = output_ptr as usize;
        let output_end = output_start
            .checked_add(output_len as usize)
            .ok_or_else(|| invalid_abi("output range overflows"))?;
        let input_start = input_ptr as usize;
        let input_end = input_start
            .checked_add(source.len())
            .ok_or_else(|| invalid_abi("input range overflows"))?;
        if output_start < input_end && input_start < output_end {
            return Err(invalid_abi(
                "output must not alias the host-owned input buffer",
            ));
        }
        let response = memory
            .data(&*store)
            .get(output_start..output_end)
            .ok_or_else(|| invalid_abi("output range is outside guest memory"))?;
        let result = decode_response(response, request);
        // Free separately-owned buffers only after validating their ranges. On a
        // malformed response or trap, dropping the fresh store reclaims all memory.
        dealloc
            .call(&mut *store, (output_ptr, output_len))
            .map_err(|error| guest_error(store, &error.to_string()))?;
        dealloc
            .call(&mut *store, (input_ptr, source_len))
            .map_err(|error| guest_error(store, &error.to_string()))?;
        result
    }
}

fn deserialize_embedded(engine: &Engine, fuel: bool) -> wasmtime::Result<Module> {
    let bytes: &[u8] = if fuel {
        include_bytes!(concat!(env!("OUT_DIR"), "/mermaid-fuel.cwasm"))
    } else {
        include_bytes!(concat!(env!("OUT_DIR"), "/mermaid.cwasm"))
    };
    // SAFETY: These immutable bytes are produced exclusively by this package's
    // trusted build script using Engine::precompile_module, the same locked
    // Wasmtime version, shared configuration and explicit destination target.
    // No runtime paths, public byte arguments or externally supplied native
    // artifacts can reach this operation. Wasmtime rejects incompatible engines;
    // callers propagate that error and never fall back to compilation.
    unsafe { Module::deserialize(engine, bytes) }
}

#[cfg(test)]
#[derive(Default)]
struct ExecutionProbe {
    epochs: Mutex<std::collections::HashSet<thread::ThreadId>>,
    settled: AtomicUsize,
}

#[cfg(test)]
impl Drop for Execution {
    fn drop(&mut self) {
        self.probe.settled.fetch_add(1, Ordering::AcqRel);
    }
}

struct AsyncAdmission;

impl AsyncAdmission {
    fn acquire() -> Result<Self, MermaidRenderError> {
        ASYNC_JOBS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_ASYNC_JOBS).then_some(active + 1)
            })
            .map_err(|_| unavailable("Mermaid WASM async job capacity exhausted"))?;
        Ok(Self)
    }
}

impl Drop for AsyncAdmission {
    fn drop(&mut self) {
        ASYNC_JOBS.fetch_sub(1, Ordering::AcqRel);
    }
}

struct AsyncJob {
    cancellation: MermaidCancellationToken,
}

impl Drop for AsyncJob {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

fn acquire_permit(
    limit: usize,
    deadline: Instant,
    cancellation: &MermaidCancellationToken,
    job_cancellation: &MermaidCancellationToken,
) -> Result<RenderPermit, MermaidRenderError> {
    let (permits, available) = RENDER_PERMITS.get_or_init(|| (Mutex::new(0), Condvar::new()));
    let mut active = permits
        .lock()
        .map_err(|_| MermaidRenderError::BackendPanicked)?;
    loop {
        if cancellation.is_cancelled() || job_cancellation.is_cancelled() {
            return Err(MermaidRenderError::Cancelled);
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(MermaidRenderError::TimedOut)?;
        if *active < limit {
            *active += 1;
            return Ok(RenderPermit);
        }
        active = available
            .wait_timeout(active, remaining.min(EPOCH_INTERVAL))
            .map_err(|_| MermaidRenderError::BackendPanicked)?
            .0;
    }
}

fn decode_response(
    response: &[u8],
    request: &MermaidRenderRequest,
) -> Result<MermaidRendered, MermaidRenderError> {
    let status = response[0];
    if status > 1 {
        return Err(invalid_abi("unknown guest response status"));
    }
    let text = std::str::from_utf8(&response[1..])
        .map_err(|_| invalid_abi("guest response is not UTF-8"))?;
    if status == 1 {
        return Err(MermaidRenderError::InvalidDiagram {
            message: bounded_message(text),
        });
    }
    if !text.trim_start().starts_with("<svg") {
        return Err(invalid_abi("successful guest response is not SVG"));
    }
    if let Some((width, height)) = svg_dimensions(text)
        && (!width.is_finite()
            || !height.is_finite()
            || width < 0.0
            || height < 0.0
            || width > f64::from(request.width)
            || height > f64::from(request.height))
    {
        return Err(MermaidRenderError::OutputDimensionsExceeded);
    }
    Ok(MermaidRendered {
        output: MermaidRenderedOutput::Svg(text.as_bytes().to_vec()),
        cache_key: request.cache_key(),
        diagnostics: Vec::new(),
    })
}

fn bounded_message(message: &str) -> String {
    message.chars().take(1024).collect()
}

fn unavailable(message: &str) -> MermaidRenderError {
    MermaidRenderError::RendererUnavailable {
        message: bounded_message(message),
    }
}

fn invalid_abi(message: &str) -> MermaidRenderError {
    MermaidRenderError::InvalidGuestResponse {
        message: bounded_message(message),
    }
}

fn guest_error(store: &Store<Execution>, message: &str) -> MermaidRenderError {
    store
        .data()
        .check()
        .err()
        .unwrap_or_else(|| MermaidRenderError::InvalidDiagram {
            message: bounded_message(message),
        })
}

struct EpochTicker {
    stop: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

impl EpochTicker {
    fn new(engine: Engine) -> Result<Self, MermaidRenderError> {
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let signal = stop.clone();
        let thread = thread::Builder::new()
            .name("mermaid-wasm-epoch".into())
            .spawn(move || {
                let (lock, wake) = &*signal;
                let Ok(mut stopped) = lock.lock() else { return };
                while !*stopped {
                    let Ok(waited) = wake.wait_timeout(stopped, EPOCH_INTERVAL) else {
                        return;
                    };
                    stopped = waited.0;
                    if !*stopped {
                        engine.increment_epoch();
                    }
                }
            })
            .map_err(|error| unavailable(&error.to_string()))?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for EpochTicker {
    fn drop(&mut self) {
        let (lock, wake) = &*self.stop;
        if let Ok(mut stopped) = lock.lock() {
            *stopped = true;
            wake.notify_one();
        }
        if let Some(thread) = self.thread.take() {
            // No execution uses this ticker once the last renderer clone drops.
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod async_tests {
    use super::*;
    use std::{
        future::{Future, poll_fn},
        task::Poll,
    };
    use wasmtime::Config;

    async fn wait_until(mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !ready() {
            assert!(
                Instant::now() < deadline,
                "guest readiness/settlement deadline exceeded"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    #[test]
    fn embedded_aot_rejects_deliberate_fuel_config_mismatch() {
        let plain = WasmMermaidRenderer::engine(false).unwrap();
        let fuel = WasmMermaidRenderer::engine(true).unwrap();
        assert!(deserialize_embedded(&plain, true).is_err());
        assert!(deserialize_embedded(&fuel, false).is_err());
        assert!(deserialize_embedded(&plain, false).is_ok());
        assert!(deserialize_embedded(&fuel, true).is_ok());
    }

    #[test]
    fn epoch_deadline_produces_wasmtime_interrupt_trap() {
        let mut config = Config::new();
        config.epoch_interruption(true);
        let engine = Engine::new(&config).unwrap();
        let module = Module::from_binary(
            &engine,
            &wat::parse_str("(module (func (export \"run\") (loop $forever br $forever)))")
                .unwrap(),
        )
        .unwrap();
        let mut store = Store::new(&engine, ());
        store.set_epoch_deadline(1);
        store.epoch_deadline_callback(|_| Ok(UpdateDeadline::Interrupt));
        let instance = Instance::new(&mut store, &module, &[]).unwrap();
        let run = instance
            .get_typed_func::<(), ()>(&mut store, "run")
            .unwrap();
        engine.increment_epoch();
        let error = run.call(&mut store, ()).unwrap_err();
        assert_eq!(
            error.downcast_ref::<wasmtime::Trap>(),
            Some(&wasmtime::Trap::Interrupt)
        );
    }

    #[test]
    fn async_drop_is_nonblocking_isolated_and_eventually_settles() {
        let module = wat::parse_str(
            r#"(module
            (memory (export "memory") 1)
            (func (export "alloc") (param i32) (result i32) i32.const 64)
            (func (export "dealloc") (param i32 i32))
            (func (export "render") (param i32 i32) (result i64)
                (loop $forever br $forever) i64.const 0))"#,
        )
        .unwrap();
        let renderer = WasmMermaidRenderer::from_module_bytes(&module).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut request = MermaidRenderRequest::svg("flowchart LR\nA --> B", 800, 600);
            request.limits.timeout = Duration::from_secs(10);
            let shared = MermaidCancellationToken::default();
            let mut first = Box::pin(renderer.render_async(&request, &shared));
            poll_fn(|context| {
                assert!(first.as_mut().poll(context).is_pending());
                Poll::Ready(())
            })
            .await;
            // Epoch callbacks prove actual guest execution, not just host launch.
            wait_until(|| renderer.0.probe.epochs.lock().unwrap().len() == 1).await;
            let mut second = Box::pin(renderer.render_async(&request, &shared));
            poll_fn(|context| {
                assert!(second.as_mut().poll(context).is_pending());
                Poll::Ready(())
            })
            .await;
            wait_until(|| renderer.0.probe.epochs.lock().unwrap().len() == 2).await;
            // Fill the six remaining admitted slots with permit waiters. A ninth
            // call fails explicitly; dropping waiters doesn't cancel the guests.
            let mut queued = Vec::new();
            for _ in 2..MAX_ASYNC_JOBS {
                let mut job = Box::pin(renderer.render_async(&request, &shared));
                poll_fn(|context| {
                    assert!(job.as_mut().poll(context).is_pending());
                    Poll::Ready(())
                })
                .await;
                queued.push(job);
            }
            assert!(matches!(
                renderer.render_async(&request, &shared).await,
                Err(MermaidRenderError::RendererUnavailable { .. })
            ));
            drop(queued);
            wait_until(|| ASYNC_JOBS.load(Ordering::Acquire) == 2).await;
            assert_eq!(renderer.0.probe.settled.load(Ordering::Acquire), 0);
            let before = Instant::now();
            drop(first);
            assert!(before.elapsed() < Duration::from_millis(100));
            assert!(!shared.is_cancelled());
            wait_until(|| {
                renderer.0.probe.settled.load(Ordering::Acquire) == 1
                    && ASYNC_JOBS.load(Ordering::Acquire) == 1
            })
            .await;
            assert_eq!(renderer.0.probe.settled.load(Ordering::Acquire), 1);
            shared.cancel();
            assert_eq!(second.await, Err(MermaidRenderError::Cancelled));
            assert_eq!(renderer.0.probe.settled.load(Ordering::Acquire), 2);

            request.limits.timeout = Duration::from_millis(40);
            let token = MermaidCancellationToken::default();
            assert_eq!(
                renderer.render_async(&request, &token).await,
                Err(MermaidRenderError::TimedOut)
            );
            assert_eq!(renderer.0.probe.settled.load(Ordering::Acquire), 3);
        });
        runtime.shutdown_timeout(Duration::from_secs(3));
    }
}
