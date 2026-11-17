//! Explicitly negotiated native ABI v2/v3. ABI v1/v2 layouts remain frozen.
//!
//! All pointers refer to caller-owned memory borrowed only for that call. The
//! plugin copies input and never returns an allocation to the host. Output is
//! copied into bounded host buffers; `BUFFER_TOO_SMALL` reports the required
//! length without repeating work. Handles are adapter-local monotonic integers,
//! never addresses. The library must stay loaded until all instances are drained
//! and destroyed. Metadata discovery is inert.
//!
//! Calls may be concurrent and reentrant. No adapter mutex is held while calling
//! author code or polling a future. `BUSY` means another poll owns that operation.
//! Pending operations are retained until final read and acknowledgement. Guests
//! advertising `CAP_HOST_WAKE_V1` export `read_wake`, which polls with a
//! host-provided waker and guarantees a wake once progress is possible (see
//! `host_wake`). Legacy guests without that bit poll with a no-op waker through
//! `read`; hosts MUST periodically poll those guests. Cancellation is cooperative
//! and never implies destruction.
//! Unwinding panics become `PANICKED`; aborts/native faults cannot be contained.
//! A panicked instance is retained as cleanup-required and cannot be destroyed
//! through this ABI. New host ports enter only through a separately retained
//! C table; executable bridge fields are rejected before JSON context decoding.

pub mod async_host;
pub mod callback_authority;
pub mod host_events;
pub use callback_authority::{
    CALLBACK_AUTHORITY_VERSION, CAP_CALLBACK_AUTHORITY_V1, MAX_AUTHORITY_CANCEL_SUBSCRIPTIONS,
    NativeCallbackAuthorityV1, NativeCallbackCancelNotifyV1,
    NativeInvocationPortsV3CallbackAuthorityV1,
};
mod callback_projection;
pub mod host_wake;
pub use host_wake::{
    CAP_HOST_WAKE_V1, HOST_WAKE_VERSION, HostWaker, NATIVE_DESCRIPTOR_SIZE_HOST_WAKE_V1,
    NativeHostWakerV1, NativePluginDescriptorV3HostWakeV1, NativeReadWakeV1,
};
mod ports;
pub mod retained;
pub use ports::{
    CAP_ACTIVATION_REGISTRATIONS, CAP_ASYNC_ACTIVATION, CAP_HOST_SERVICES, CAP_KERNEL_REQUESTS,
    CAP_LIVE_CANCELLATION, CAP_RETAINED_ASYNC_HOST_V1, CAP_RETAINED_HOST_LOOKUP_V2,
    CAP_RETAINED_LIFECYCLE, CAP_SELECTED_SERVICE_AVAILABILITY_V1, CAP_STREAM_EVENTS,
    NativeHostRequest, NativeInvocationPorts, NativeInvocationPortsV3,
    NativeInvocationPortsV3Availability, NativeInvocationPortsV3RetainedHost,
    NativeInvocationPortsV3RetainedHostV2, NativeSlice, PORTS_VERSION, PORTS_VERSION_V3,
    SCOPED_CAPABILITIES, SCOPED_CAPABILITIES_V2, SCOPED_CAPABILITIES_V3_AVAILABILITY,
    SCOPED_CAPABILITIES_V3_LEGACY, SCOPED_CAPABILITIES_V3_RECOVERY_V2,
    SCOPED_CAPABILITIES_V3_RETAINED_V1,
};

use crate::definition::{
    ActivationId, CommandOutcome, PluginCallContext, PluginCreateContext, PluginDefinition,
    PluginFuture, PluginInstance, PluginLifecycleContext,
};
use crate::{
    NativeCommandContext, NativeServiceContext, PluginEvent, PluginInvocationId,
    ServiceCancellation,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    task::{Context, Poll, Waker},
};

pub const ABI_VERSION_V2: u32 = 2;
pub const ABI_VERSION: u32 = 3;
/// Descriptor size with an explicit checked conversion usable by static tables.
pub const NATIVE_DESCRIPTOR_SIZE: u32 = {
    let size = std::mem::size_of::<NativePluginDescriptorV3>();
    assert!(size <= u32::MAX as usize);
    let bytes = size.to_le_bytes();
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
};
/// Descriptor-level capabilities: every scoped invocation capability plus
/// operation-level contracts that never appear in an invocation ports table.
pub const DESCRIPTOR_CAPABILITIES: u64 = SCOPED_CAPABILITIES | CAP_HOST_WAKE_V1;
pub const ENTRY_SYMBOL: &str = "bmux_plugin_entry_v2";
pub const MAX_BUFFER_BYTES: usize = 1024 * 1024;
pub const MAX_INSTANCES: usize = 256;
pub const MAX_OPERATIONS: usize = 128;

pub type AbiStatus = i32;
pub const OK: AbiStatus = 0;
pub const PENDING: AbiStatus = 1;
pub const INVALID_ARGUMENT: AbiStatus = 2;
pub const BUFFER_TOO_SMALL: AbiStatus = 3;
pub const NOT_FOUND: AbiStatus = 4;
pub const BUSY: AbiStatus = 5;
pub const UNSUPPORTED: AbiStatus = 6;
pub const LIMIT_EXCEEDED: AbiStatus = 7;
pub const PANICKED: AbiStatus = 8;
pub const FAILED: AbiStatus = 9;
/// V3 child-call cancellation: an explicit terminal error, not transport failure.
///
/// Available only with negotiated invocation ports version 3. Version 2 retains
/// its rejection/unknown-failure behavior; status 10 is not recoverable there.
/// This does not assert rollback or authorize retry. A successfully acknowledged
/// child cancellation must not replace an author's subsequently settled report.
pub const CANCELLED: AbiStatus = 10;

/// Function table size/version must be checked before reading later fields.
///
/// `start` consumes one unique operation ID. It never replays an existing ID.
/// `read` drives the operation and copies the same final outcome until `ack`.
/// `ack` is explicit disposal, not cancellation. Hosts must not reuse operation
/// IDs in an instance, including after ack. `destroy` rejects outstanding work.
#[repr(C)]
pub struct NativePluginDescriptor {
    pub size: u32,
    pub version: u32,
    /// Scoped callback features implemented by `start_scoped`; hosts must select
    /// the required bits explicitly in the per-invocation table.
    pub capabilities: u64,
    pub start_scoped:
        unsafe extern "C" fn(u64, u64, *const u8, usize, *const NativeInvocationPorts) -> AbiStatus,
    pub metadata: unsafe extern "C" fn(*mut u8, usize, *mut usize) -> AbiStatus,
    pub create: unsafe extern "C" fn(*const u8, usize, *mut u64) -> AbiStatus,
    pub destroy: unsafe extern "C" fn(u64) -> AbiStatus,
    pub start: unsafe extern "C" fn(u64, u64, *const u8, usize) -> AbiStatus,
    pub read: unsafe extern "C" fn(u64, u64, *mut u8, usize, *mut usize) -> AbiStatus,
    pub cancel: unsafe extern "C" fn(u64, u64) -> AbiStatus,
    pub ack: unsafe extern "C" fn(u64, u64) -> AbiStatus,
}

/// Negotiated v3 declaration export. The base layout is unchanged from v2.
/// The callback copies bounded JSON `PluginDeclarations`, without author effects.
#[repr(C)]
pub struct NativePluginDescriptorV3 {
    pub base: NativePluginDescriptor,
    pub declarations: unsafe extern "C" fn(*mut u8, usize, *mut usize) -> AbiStatus,
}

/// Minimal prefix a loader may inspect before accessing the operation table.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct NativeDescriptorHeader {
    pub size: u32,
    pub version: u32,
}

/// Validate a descriptor before invoking any operation. Unknown capability bits
/// are rejected, not silently reinterpreted as a supported transport.
///
/// # Safety
/// `pointer` must be aligned immutable library memory readable for the header.
/// A supported header's size must truthfully describe the readable allocation.
/// The caller must retain the library for the returned borrow's entire lifetime.
/// # Errors
/// Returns an error if the bound scope is unavailable, the request is invalid,
/// or the implementation rejects the operation.
pub const unsafe fn validate_descriptor<'library>(
    pointer: *const NativePluginDescriptor,
) -> Result<&'library NativePluginDescriptor, AbiStatus> {
    if pointer.is_null() {
        return Err(INVALID_ARGUMENT);
    }
    let header = unsafe { pointer.cast::<NativeDescriptorHeader>().read() };
    if !matches!(header.version, ABI_VERSION_V2 | ABI_VERSION)
        || (header.size as usize) < std::mem::size_of::<NativePluginDescriptor>()
    {
        return Err(UNSUPPORTED);
    }
    if header.version == ABI_VERSION
        && (header.size as usize) < std::mem::size_of::<NativePluginDescriptorV3>()
    {
        return Err(UNSUPPORTED);
    }
    let descriptor = unsafe { &*pointer };
    let supported = if header.version == ABI_VERSION_V2 {
        SCOPED_CAPABILITIES_V2
    } else {
        DESCRIPTOR_CAPABILITIES
    };
    if descriptor.capabilities & !supported != 0 {
        return Err(UNSUPPORTED);
    }
    // The wake bit is only meaningful with its complete descriptor tail.
    if descriptor.capabilities & CAP_HOST_WAKE_V1 != 0
        && (header.size as usize) < std::mem::size_of::<NativePluginDescriptorV3HostWakeV1>()
    {
        return Err(UNSUPPORTED);
    }
    Ok(descriptor)
}

/// Read the host-wake tail when, and only when, `CAP_HOST_WAKE_V1` is advertised.
/// `None` is an explicit legacy guest that hosts must poll periodically.
///
/// # Safety
/// Same truthful allocation and retained-library requirements as `validate_descriptor`.
/// # Errors
/// Rejects unsupported/malformed descriptors.
pub const unsafe fn validate_descriptor_host_wake<'library>(
    pointer: *const NativePluginDescriptor,
) -> Result<Option<&'library NativePluginDescriptorV3HostWakeV1>, AbiStatus> {
    let descriptor = match unsafe { validate_descriptor(pointer) } {
        Ok(descriptor) => descriptor,
        Err(status) => return Err(status),
    };
    if descriptor.capabilities & CAP_HOST_WAKE_V1 == 0 {
        return Ok(None);
    }
    Ok(Some(unsafe {
        &*pointer.cast::<NativePluginDescriptorV3HostWakeV1>()
    }))
}

/// Read the v3 extension only after validating its complete header and size.
///
/// # Safety
/// Same truthful allocation and retained-library requirements as `validate_descriptor`.
/// # Errors
/// Rejects v2 and unsupported/malformed descriptors.
pub const unsafe fn validate_descriptor_v3<'library>(
    pointer: *const NativePluginDescriptor,
) -> Result<&'library NativePluginDescriptorV3, AbiStatus> {
    let descriptor = match unsafe { validate_descriptor(pointer) } {
        Ok(descriptor) => descriptor,
        Err(status) => return Err(status),
    };
    if descriptor.version != ABI_VERSION {
        return Err(UNSUPPORTED);
    }
    Ok(unsafe { &*pointer.cast::<NativePluginDescriptorV3>() })
}

/// Transport data used only by the dynamic adapter, not a second author API.
/// Streaming event delivery uses separately negotiated scoped callback ports.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", content = "context", rename_all = "snake_case")]
pub enum NativeOperation {
    Activate(PluginLifecycleContext),
    Deactivate(PluginLifecycleContext),
    Command {
        invocation_id: PluginInvocationId,
        context: NativeCommandContext,
    },
    Invoke {
        invocation_id: PluginInvocationId,
        context: NativeServiceContext,
    },
    Streaming {
        invocation_id: PluginInvocationId,
        context: NativeServiceContext,
    },
    Event(PluginEvent),
    /// Collect portable contributions/contracts once; retained read/ack prevents
    /// repeated registration side effects on response buffer growth.
    Registrations,
    Extension {
        invocation_id: PluginInvocationId,
        contract: String,
        payload: Vec<u8>,
    },
}

/// Explicit error text is owned by the retained outcome, not a thread local.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "value", rename_all = "snake_case")]
pub enum NativeOutcome {
    Unit,
    Command(CommandOutcome),
    Service(crate::ServiceResponse),
    Bytes(Vec<u8>),
    Registrations(crate::definition::InstanceRegistrations),
    Error { code: i32, message: String },
}
impl NativeOutcome {
    fn result<T>(
        value: crate::definition::PluginResult<T>,
        success: impl FnOnce(T) -> Self,
    ) -> Self {
        match value {
            Ok(value) => success(value),
            Err(error) => Self::Error {
                code: error.code,
                message: error.message,
            },
        }
    }
}

enum OperationState {
    Pending(PluginFuture<'static, NativeOutcome>),
    Polling,
    Draining(Box<Self>),
    Ready(Vec<u8>),
    Failed(AbiStatus),
}
struct Operation {
    state: Mutex<OperationState>,
    cancellation: ServiceCancellation,
    lifecycle: Option<bool>,
    ports: Option<Arc<ports::InvocationPorts>>,
}
#[derive(Default)]
struct InstanceLifecycle {
    // 0 inactive, 1 activating, 2 active, 3 deactivating, 4 cleanup required.
    phase: u8,
    activation: Option<ActivationId>,
    ports: Option<Arc<ports::InvocationPorts>>,
}
struct Instance {
    tasks: Mutex<Option<bmux_plugin_runtime::background::BackgroundTasks>>,
    callback_executor: Arc<callback_projection::Executor>,
    implementation: Arc<dyn PluginInstance>,
    context: PluginCreateContext,
    lifecycle: Mutex<InstanceLifecycle>,
    operations: Mutex<BTreeMap<u64, Arc<Operation>>>,
    // Strict monotonic IDs avoid unbounded tombstones and accidental replay.
    last_operation: AtomicU64,
    cleanup_required: AtomicBool,
}

/// Shared adapter bookkeeping, never a singleton implementation. Every create
/// invokes the factory anew. Host lifecycle policy remains outside this adapter.
pub struct NativeAdapter {
    definition: PluginDefinition,
    instances: Mutex<BTreeMap<u64, Arc<Instance>>>,
    next_instance: AtomicU64,
    instance_slots: AtomicUsize,
    // Failed destructor evidence is retained without pretending partially
    // dropped author state is callable or safe to unload.
    retired_failures: Mutex<BTreeMap<u64, AbiStatus>>,
}
struct InstanceReservation<'a>(&'a AtomicUsize);
impl Drop for InstanceReservation<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
impl NativeAdapter {
    #[must_use]
    pub const fn new(definition: PluginDefinition) -> Self {
        Self {
            definition,
            instances: Mutex::new(BTreeMap::new()),
            next_instance: AtomicU64::new(1),
            instance_slots: AtomicUsize::new(0),
            retired_failures: Mutex::new(BTreeMap::new()),
        }
    }
    fn instance(&self, id: u64) -> Result<Arc<Instance>, AbiStatus> {
        self.instances
            .lock()
            .map_err(|_| PANICKED)?
            .get(&id)
            .cloned()
            .ok_or(NOT_FOUND)
    }
    fn operation(
        &self,
        instance: u64,
        operation: u64,
    ) -> Result<(Arc<Instance>, Arc<Operation>), AbiStatus> {
        let instance = self.instance(instance)?;
        let operation = instance
            .operations
            .lock()
            .map_err(|_| PANICKED)?
            .get(&operation)
            .cloned()
            .ok_or(NOT_FOUND)?;
        Ok((instance, operation))
    }
    /// # Safety
    /// Output pointers must be writable for capacity/one length respectively,
    /// nonoverlapping, and valid until return. A null byte pointer needs zero capacity.
    pub unsafe fn metadata(
        &self,
        output: *mut u8,
        capacity: usize,
        length: *mut usize,
    ) -> AbiStatus {
        match encode_bounded(&self.definition.metadata) {
            Ok(bytes) => unsafe { copy_output(&bytes, output, capacity, length) },
            Err(status) => status,
        }
    }
    /// # Safety
    /// Input must be readable for length bytes; result must be writable for one u64.
    pub unsafe fn create(&self, input: *const u8, length: usize, result: *mut u64) -> AbiStatus {
        if result.is_null() {
            return INVALID_ARGUMENT;
        }
        let Ok(context) = (unsafe { decode::<PluginCreateContext>(input, length) }) else {
            return INVALID_ARGUMENT;
        };
        if self
            .instance_slots
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_INSTANCES).then_some(count + 1)
            })
            .is_err()
        {
            return LIMIT_EXCEEDED;
        }
        let reservation = InstanceReservation(&self.instance_slots);
        // Handle allocation is monotonic and independent of product identity.
        // Factories must not activate work; lifecycle admission belongs to host.
        let Ok(id) = self
            .next_instance
            .try_update(Ordering::AcqRel, Ordering::Acquire, |id| id.checked_add(1))
        else {
            return LIMIT_EXCEEDED;
        };
        let Ok(implementation) = self.definition.create(context.clone()) else {
            return FAILED;
        };
        let Ok(mut instances) = self.instances.lock() else {
            return PANICKED;
        };
        // The reservation includes factories currently running without the map
        // lock, so concurrent creates cannot exceed capacity before admission.
        instances.insert(
            id,
            Arc::new(Instance {
                tasks: Mutex::new(None),
                callback_executor: Arc::default(),
                implementation,
                context,
                lifecycle: Mutex::new(InstanceLifecycle::default()),
                operations: Mutex::new(BTreeMap::new()),
                last_operation: AtomicU64::new(0),
                cleanup_required: AtomicBool::new(false),
            }),
        );
        std::mem::forget(reservation); // Retained instance now owns this slot.
        unsafe {
            result.write(id);
        }
        OK
    }
    pub fn destroy(&self, id: u64) -> AbiStatus {
        if let Ok(instance) = self.instance(id)
            && let Err(status) = instance.callback_executor.retired()
        {
            return status;
        }
        let Ok(mut instances) = self.instances.lock() else {
            return PANICKED;
        };
        let Some(instance) = instances.get(&id) else {
            return self.retired_failures.lock().map_or(PANICKED, |failures| {
                failures.get(&id).copied().unwrap_or(NOT_FOUND)
            });
        };
        if instance.cleanup_required.load(Ordering::Acquire) {
            return FAILED;
        }
        // Arc references include calls admitted before this map lock. Those
        // callers must finish before removal; never free under a stale caller.
        if Arc::strong_count(instance) != 1 {
            return BUSY;
        }
        let Ok(operations) = instance.operations.lock() else {
            return PANICKED;
        };
        if !operations.is_empty() {
            return BUSY;
        }
        let Ok(lifecycle) = instance.lifecycle.lock() else {
            return PANICKED;
        };
        if lifecycle.phase != 0 {
            return BUSY;
        }
        drop(lifecycle);
        drop(operations);
        // Drop author state outside the registry lock (destructors may reenter).
        // Mark retirement uncertain before releasing registry ownership; a
        // reentrant destroy cannot report success while Drop is still running.
        let Ok(mut failures) = self.retired_failures.lock() else {
            return PANICKED;
        };
        failures.insert(id, BUSY);
        drop(failures);
        let removed = instances.remove(&id);
        drop(instances);
        let dropped = catch_unwind(AssertUnwindSafe(|| drop(removed)));
        let Ok(mut failures) = self.retired_failures.lock() else {
            return PANICKED;
        };
        if dropped.is_err() {
            failures.insert(id, PANICKED);
            return PANICKED;
        }
        failures.remove(&id);
        self.instance_slots.fetch_sub(1, Ordering::AcqRel);
        OK
    }
    /// # Safety
    /// Input is borrowed readable memory for this call only.
    pub unsafe fn start(
        &self,
        id: u64,
        operation_id: u64,
        input: *const u8,
        length: usize,
    ) -> AbiStatus {
        unsafe { self.start_inner(id, operation_id, input, length, None) }
    }
    /// # Safety
    /// Input obeys start's borrowed-buffer contract; ports obey retain/release
    /// and point to a truthful versioned fixed-layout table.
    pub unsafe fn start_scoped(
        &self,
        id: u64,
        operation_id: u64,
        input: *const u8,
        length: usize,
        ports: *const NativeInvocationPorts,
    ) -> AbiStatus {
        let ports = match unsafe { ports::InvocationPorts::acquire(ports) } {
            Ok(ports) => ports,
            Err(status) => return status,
        };
        unsafe { self.start_inner(id, operation_id, input, length, Some(ports)) }
    }
    unsafe fn start_inner(
        &self,
        id: u64,
        operation_id: u64,
        input: *const u8,
        length: usize,
        ports: Option<Arc<ports::InvocationPorts>>,
    ) -> AbiStatus {
        // Reject executable context fields BEFORE invoking their Deserialize
        // implementations: legacy bridge decoding can itself acquire ownership.
        let wire: serde_json::Value = match unsafe { decode(input, length) } {
            Ok(value) => value,
            Err(status) => return status,
        };
        if executable_fields(&wire) {
            return INVALID_ARGUMENT;
        }
        let request: NativeOperation = match serde_json::from_value(wire) {
            Ok(request) => request,
            Err(_) => return INVALID_ARGUMENT,
        };
        if matches!(&request, NativeOperation::Streaming { .. })
            && ports
                .as_ref()
                .is_none_or(|ports| ports.capabilities() & CAP_STREAM_EVENTS == 0)
        {
            return UNSUPPORTED;
        }
        let instance = match self.instance(id) {
            Ok(instance) => instance,
            Err(status) => return status,
        };
        if instance.cleanup_required.load(Ordering::Acquire) {
            return FAILED;
        }
        if let NativeOperation::Activate(context) | NativeOperation::Deactivate(context) = &request
            && context.scope != instance.context
        {
            return INVALID_ARGUMENT;
        }
        let Ok(mut operations) = instance.operations.lock() else {
            return PANICKED;
        };
        if operations.len() >= MAX_OPERATIONS {
            return LIMIT_EXCEEDED;
        }
        // Host must allocate ascending operation IDs. Failed admission does not
        // consume an ID; accepted IDs can never execute twice, even after ack.
        let last = instance.last_operation.load(Ordering::Acquire);
        if operation_id <= last {
            return INVALID_ARGUMENT;
        }
        let Ok(mut lifecycle) = instance.lifecycle.lock() else {
            return PANICKED;
        };
        if matches!(&request, NativeOperation::Activate(_))
            && ports
                .as_ref()
                .is_some_and(|ports| ports.capabilities() & CAP_ASYNC_ACTIVATION != 0)
        {
            if lifecycle.phase != 0 || !operations.is_empty() {
                return BUSY;
            }
            if let Err(status) = initialize_activation_tasks(&instance) {
                return status;
            }
        }
        let lifecycle_operation = match admit_lifecycle(
            &mut lifecycle,
            &request,
            ports.as_ref(),
            operations.is_empty(),
        ) {
            Ok(operation) => operation,
            Err(status) => return status,
        };
        drop(lifecycle);
        let cancellation = ServiceCancellation::new();
        if let Some(ports) = &ports
            && let Err(status) = ports.bind_cancellation(cancellation.clone())
        {
            return status;
        }
        let tasks = match instance.tasks.lock() {
            Ok(tasks) => tasks.clone(),
            Err(_) => return PANICKED,
        };
        let future = operation_future(
            Arc::clone(&instance.implementation),
            request,
            cancellation.clone(),
            ports.clone(),
            tasks,
            Arc::clone(&instance.callback_executor),
        );
        operations.insert(
            operation_id,
            Arc::new(Operation {
                state: Mutex::new(OperationState::Pending(future)),
                cancellation,
                lifecycle: lifecycle_operation,
                ports,
            }),
        );
        instance
            .last_operation
            .store(operation_id, Ordering::Release);
        PENDING
    }
    /// Export validated inert declarations using the metadata output contract.
    ///
    /// # Safety
    /// Output pointers must satisfy `metadata`'s requirements.
    pub unsafe fn declarations(
        &self,
        output: *mut u8,
        capacity: usize,
        length: *mut usize,
    ) -> AbiStatus {
        let Ok(declarations) = self.definition.declarations() else {
            return INVALID_ARGUMENT;
        };
        match encode_bounded(&declarations) {
            Ok(bytes) => unsafe { copy_output(&bytes, output, capacity, length) },
            Err(status) => status,
        }
    }

    /// Legacy no-op-waker read for hosts or guests without `CAP_HOST_WAKE_V1`.
    ///
    /// # Safety
    /// Same output requirements as `metadata`. The caller must continue polling
    /// pending operations, including after cancellation, until a final outcome;
    /// no wake is promised, so that polling must be periodic.
    pub unsafe fn read(
        &self,
        id: u64,
        operation_id: u64,
        output: *mut u8,
        capacity: usize,
        length: *mut usize,
    ) -> AbiStatus {
        let (status, _) =
            unsafe { self.read_polled(id, operation_id, output, capacity, length, Waker::noop()) };
        // Wake-capable readers of the same slot may be waiting on this poll.
        host_wake::notify_progress();
        status
    }

    /// `CAP_HOST_WAKE_V1` read: polls with the host's waker and guarantees a
    /// later wake through it after `PENDING`/`BUSY` once progress is possible.
    ///
    /// # Safety
    /// Same output requirements as `read`; `waker` must satisfy the
    /// `NativeHostWakerV1` contract for the duration of this call.
    pub unsafe fn read_wake(
        &self,
        id: u64,
        operation_id: u64,
        output: *mut u8,
        capacity: usize,
        length: *mut usize,
        waker: *const NativeHostWakerV1,
    ) -> AbiStatus {
        let waker = match unsafe { host_wake::guest_waker(waker) } {
            Ok(waker) => waker,
            Err(status) => return status,
        };
        // Register before inspecting state so a concurrent release cannot be
        // missed between a BUSY/drain observation and this registration.
        let registration = host_wake::ProgressRegistration::new(&waker);
        let (status, awaits_progress) =
            unsafe { self.read_polled(id, operation_id, output, capacity, length, &waker) };
        if !awaits_progress {
            registration.withdraw();
        }
        // This poll may have released the operation or drained callbacks that
        // other readers observed as BUSY. Never wake our own registration.
        host_wake::notify_progress_except(registration.token());
        status
    }

    /// Returns the status and whether a PENDING/BUSY result awaits guest-side
    /// progress (callback drain or concurrent poll) rather than the author
    /// future, whose own waker registration covers the latter.
    #[allow(clippy::too_many_lines)] // One linear state machine over the operation slot.
    unsafe fn read_polled(
        &self,
        id: u64,
        operation_id: u64,
        output: *mut u8,
        capacity: usize,
        length: *mut usize,
        waker: &Waker,
    ) -> (AbiStatus, bool) {
        if length.is_null() || capacity > MAX_BUFFER_BYTES || (capacity > 0 && output.is_null()) {
            return (INVALID_ARGUMENT, false);
        }
        unsafe {
            length.write(0);
        }
        let (instance, operation) = match self.operation(id, operation_id) {
            Ok(value) => value,
            Err(status) => return (status, false),
        };
        let _ = instance.callback_executor.drain();
        // Claim terminal drain without holding the state mutex across release
        // callbacks. Reentrant polls observe Polling/BUSY instead of deadlocking.
        let draining = match claim_draining(&operation) {
            Ok(value) => value,
            Err(status) => return (status, false),
        };
        if let Some(terminal) = draining {
            let terminal = drain_terminal(&instance, &operation, terminal);
            if matches!(&terminal, OperationState::Failed(_)) {
                instance.cleanup_required.store(true, Ordering::Release);
            }
            let Ok(mut state) = operation.state.lock() else {
                return (PANICKED, false);
            };
            *state = terminal;
            match &*state {
                OperationState::Ready(bytes) => {
                    return (
                        unsafe { copy_output(bytes, output, capacity, length) },
                        false,
                    );
                }
                OperationState::Failed(status) => return (*status, false),
                OperationState::Draining(_) => return (PENDING, true),
                // A drained non-terminal future may have been woken while the
                // slot was draining; poll it now instead of losing that wake.
                OperationState::Pending(_) | OperationState::Polling => {}
            }
        }
        let mut future = {
            let Ok(mut state) = operation.state.lock() else {
                return (PANICKED, false);
            };
            match &*state {
                OperationState::Ready(bytes) => {
                    return (
                        unsafe { copy_output(bytes, output, capacity, length) },
                        false,
                    );
                }
                OperationState::Failed(status) => return (*status, false),
                OperationState::Polling | OperationState::Draining(_) => return (BUSY, true),
                OperationState::Pending(_) => {}
            }
            match std::mem::replace(&mut *state, OperationState::Polling) {
                OperationState::Pending(future) => future,
                _ => unreachable!(),
            }
        };
        // Work the author spawns through plug-rt joins this instance's
        // host-owned activation scope; no plugin-side runtime exists.
        let _scope = bmux_plugin_runtime::rt::enter_instance(id);
        let polled = catch_unwind(AssertUnwindSafe(|| {
            future.as_mut().poll(&mut Context::from_waker(waker))
        }));
        let next = match polled {
            Ok(Poll::Pending) => OperationState::Pending(future),
            Ok(Poll::Ready(outcome)) => {
                if let Some(activating) = operation.lifecycle
                    && let Err(status) = settle_lifecycle(&instance, activating, &outcome)
                {
                    return (status, false);
                }
                if catch_unwind(AssertUnwindSafe(|| drop(future))).is_err() {
                    instance.cleanup_required.store(true, Ordering::Release);
                    OperationState::Failed(PANICKED)
                } else {
                    match encode_bounded(&outcome) {
                        Ok(bytes) => OperationState::Ready(bytes),
                        Err(status) => OperationState::Failed(status),
                    }
                }
            }
            Err(_) => {
                // A panicking future may own partially closed callbacks. Keep
                // its captures (and library ownership) alive for recovery.
                std::mem::forget(future);
                instance.cleanup_required.store(true, Ordering::Release);
                OperationState::Failed(PANICKED)
            }
        };
        let next = drain_terminal(&instance, &operation, next);
        if operation.ports.is_some() && matches!(&next, OperationState::Failed(_)) {
            // Includes release uncertainty: never admit destruction/unload on
            // a terminal transport error without explicit cleanup evidence.
            instance.cleanup_required.store(true, Ordering::Release);
        }
        let Ok(mut state) = operation.state.lock() else {
            return (PANICKED, false);
        };
        *state = next;
        match &*state {
            OperationState::Ready(bytes) => (
                unsafe { copy_output(bytes, output, capacity, length) },
                false,
            ),
            OperationState::Failed(status) => (*status, false),
            OperationState::Draining(_) => (PENDING, true),
            _ => (PENDING, false),
        }
    }
    pub fn cancel(&self, id: u64, operation_id: u64) -> AbiStatus {
        match self.operation(id, operation_id) {
            Ok((_, operation)) => {
                operation.cancellation.cancel();
                OK
            }
            Err(status) => status,
        }
    }
    pub fn ack(&self, id: u64, operation_id: u64) -> AbiStatus {
        let instance = match self.instance(id) {
            Ok(instance) => instance,
            Err(status) => return status,
        };
        let Ok(mut operations) = instance.operations.lock() else {
            return PANICKED;
        };
        let Some(operation) = operations.get(&operation_id) else {
            return NOT_FOUND;
        };
        let Ok(state) = operation.state.lock() else {
            return PANICKED;
        };
        if !matches!(*state, OperationState::Ready(_) | OperationState::Failed(_)) {
            return BUSY;
        }
        drop(state);
        operations.remove(&operation_id);
        OK
    }
}

fn executable_fields(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(fields) => fields.iter().any(|(key, value)| {
            (key == "host_kernel_bridge" && !value.is_null()) || executable_fields(value)
        }),
        serde_json::Value::Array(items) => items.iter().any(executable_fields),
        _ => false,
    }
}

fn claim_draining(operation: &Operation) -> Result<Option<OperationState>, AbiStatus> {
    let mut state = operation.state.lock().map_err(|_| PANICKED)?;
    if !matches!(&*state, OperationState::Draining(_)) {
        return Ok(None);
    }
    let OperationState::Draining(outcome) = std::mem::replace(&mut *state, OperationState::Polling)
    else {
        unreachable!()
    };
    drop(state);
    Ok(Some(*outcome))
}

fn settle_lifecycle(
    instance: &Instance,
    activating: bool,
    outcome: &NativeOutcome,
) -> Result<(), AbiStatus> {
    let Ok(mut lifecycle) = instance.lifecycle.lock() else {
        return Err(PANICKED);
    };
    lifecycle.phase = if matches!(outcome, NativeOutcome::Unit) {
        if activating { 2 } else { 0 }
    } else {
        4
    };
    if lifecycle.phase == 0 {
        lifecycle.activation = None;
    }
    drop(lifecycle);
    Ok(())
}

// Install the negotiated activation task owner. Its tasks run on the host's
// runtime through plug-rt; the caller retains operation admission throughout.
fn initialize_activation_tasks(instance: &Instance) -> Result<(), AbiStatus> {
    let mut task_slot = instance.tasks.lock().map_err(|_| PANICKED)?;
    if task_slot.is_some() {
        return Err(BUSY);
    }
    *task_slot = Some(bmux_plugin_runtime::background::BackgroundTasks::new());
    drop(task_slot);
    Ok(())
}

fn admit_lifecycle(
    lifecycle: &mut InstanceLifecycle,
    request: &NativeOperation,
    ports: Option<&Arc<ports::InvocationPorts>>,
    operations_empty: bool,
) -> Result<Option<bool>, AbiStatus> {
    Ok(match request {
        NativeOperation::Activate(context) => {
            if lifecycle.phase != 0 || !operations_empty {
                return Err(BUSY);
            }
            lifecycle.activation = Some(context.activation);
            if let Some(ports) =
                ports.filter(|ports| ports.capabilities() & CAP_RETAINED_LIFECYCLE != 0)
            {
                lifecycle.ports = Some(Arc::clone(ports));
            }
            lifecycle.phase = 1;
            Some(true)
        }
        NativeOperation::Deactivate(context) => {
            if !matches!(lifecycle.phase, 2 | 4) || lifecycle.activation != Some(context.activation)
            {
                return Err(INVALID_ARGUMENT);
            }
            // Host closes admissions and drains admitted work before cleanup.
            if !operations_empty {
                return Err(BUSY);
            }
            lifecycle.phase = 3;
            Some(false)
        }
        _ => {
            if lifecycle.phase != 2 {
                return Err(BUSY);
            }
            None
        }
    })
}

fn drain_terminal(
    instance: &Instance,
    operation: &Operation,
    terminal: OperationState,
) -> OperationState {
    if let Err(status) = instance.callback_executor.drain() {
        return if status == BUSY {
            OperationState::Draining(Box::new(terminal))
        } else {
            OperationState::Failed(status)
        };
    }
    if !matches!(
        &terminal,
        OperationState::Ready(_) | OperationState::Failed(_)
    ) {
        return terminal;
    }
    if operation.lifecycle == Some(false) {
        let retained = match instance.lifecycle.lock() {
            Ok(lifecycle) => lifecycle.ports.clone(),
            Err(_) => return OperationState::Failed(PANICKED),
        };
        if let Some(ports) = retained {
            match ports.close_and_drain() {
                Ok(()) => match instance.lifecycle.lock() {
                    Ok(mut lifecycle) => {
                        lifecycle.ports = None;
                    }
                    Err(_) => return OperationState::Failed(PANICKED),
                },
                Err(BUSY) => return OperationState::Draining(Box::new(terminal)),
                Err(status) => return OperationState::Failed(status),
            }
        }
    }
    if operation.lifecycle == Some(true)
        && let Some(ports) = &operation.ports
        && ports.capabilities() & CAP_RETAINED_LIFECYCLE != 0
    {
        return match ports.settle_activation() {
            Ok(()) => terminal,
            Err(BUSY) => OperationState::Draining(Box::new(terminal)),
            Err(status) => OperationState::Failed(status),
        };
    }
    if !matches!(
        &terminal,
        OperationState::Ready(_) | OperationState::Failed(_)
    ) {
        return terminal;
    }
    match operation
        .ports
        .as_ref()
        .map(|ports| ports.close_and_drain())
    {
        None | Some(Ok(())) => terminal,
        Some(Err(BUSY)) => OperationState::Draining(Box::new(terminal)),
        Some(Err(status)) => OperationState::Failed(status),
    }
}

async fn activate_author(
    instance: &dyn PluginInstance,
    context: PluginLifecycleContext,
    host_services: Option<crate::PluginHostServices>,
    tasks: Option<&bmux_plugin_runtime::background::BackgroundTasks>,
) -> crate::definition::PluginResult<()> {
    if let Some(tasks) = tasks {
        instance
            .activate_scoped_async(
                context,
                host_services,
                crate::HostAsyncHandle::new(tasks.clone()),
            )
            .await
    } else {
        instance.activate_scoped(context, host_services).await
    }
}

async fn deactivate_author(
    instance: &dyn PluginInstance,
    context: PluginLifecycleContext,
    services: Option<crate::PluginHostServices>,
    tasks: Option<&bmux_plugin_runtime::background::BackgroundTasks>,
) -> NativeOutcome {
    if let Some(tasks) = tasks
        && let Err(error) = tasks.shutdown_retained(None).await
    {
        return NativeOutcome::result(Err(crate::PluginCommandError::failed(error)), |()| {
            NativeOutcome::Unit
        });
    }
    NativeOutcome::result(instance.deactivate_scoped(context, services).await, |()| {
        NativeOutcome::Unit
    })
}

fn ordinary_host_services(
    ports: Option<&Arc<ports::InvocationPorts>>,
) -> Option<crate::PluginHostServices> {
    ports
        .filter(|ports| {
            ports.capabilities()
                & (CAP_HOST_SERVICES
                    | CAP_ACTIVATION_REGISTRATIONS
                    | CAP_KERNEL_REQUESTS
                    | CAP_SELECTED_SERVICE_AVAILABILITY_V1
                    | CAP_RETAINED_HOST_LOOKUP_V2)
                != 0
        })
        .map(ports::InvocationPorts::host_port)
}

/// Services handed to `activate`.
///
/// A retained activation table carries the host's activation services, which
/// stay valid from `activate` until deactivation begins, so background work may
/// retain them. Projected callback services reach the host only through a live
/// author entry, so they are never the activation's handle.
fn activation_host_services(
    ports: Option<&Arc<ports::InvocationPorts>>,
    invocation_services: impl FnOnce() -> Option<crate::PluginHostServices>,
) -> Option<crate::PluginHostServices> {
    if ports.is_some_and(|ports| ports.capabilities() & CAP_RETAINED_LIFECYCLE != 0) {
        ordinary_host_services(ports)
    } else {
        invocation_services()
    }
}

fn selected_callback(
    executor: &Arc<callback_projection::Executor>,
    ports: Option<&Arc<ports::InvocationPorts>>,
    request: &NativeOperation,
    cancellation: ServiceCancellation,
) -> crate::PluginResult<Option<(crate::PluginCallbackContext, crate::PluginHostServices)>> {
    if matches!(request, NativeOperation::Registrations) {
        return Ok(None);
    }
    let Some(ports) = ports.filter(|ports| ports.capabilities() & CAP_CALLBACK_AUTHORITY_V1 != 0)
    else {
        return Ok(None);
    };
    executor.project(ports, cancellation).map(Some)
}

fn operation_future(
    instance: Arc<dyn PluginInstance>,
    request: NativeOperation,
    cancellation: ServiceCancellation,
    ports: Option<Arc<ports::InvocationPorts>>,
    tasks: Option<bmux_plugin_runtime::background::BackgroundTasks>,
    executor: Arc<callback_projection::Executor>,
) -> PluginFuture<'static, NativeOutcome> {
    Box::pin(async move {
        let selected =
            match selected_callback(&executor, ports.as_ref(), &request, cancellation.clone()) {
                Ok(selected) => selected,
                Err(error) => {
                    return NativeOutcome::Error {
                        code: error.code,
                        message: error.message,
                    };
                }
            };
        let callback = selected.as_ref().map(|(context, _)| context.clone());
        let host_services = || {
            selected
                .as_ref()
                .map(|(_, services)| services.clone())
                .or_else(|| ordinary_host_services(ports.as_ref()))
        };
        let call = |invocation_id| PluginCallContext {
            callback_execution: callback.clone(),
            invocation_id,
            cancellation: cancellation.clone(),
            host_services: host_services(),
        };
        match request {
            NativeOperation::Activate(mut context) => {
                context.callback_execution = callback.clone();
                let services = activation_host_services(ports.as_ref(), host_services);
                let result =
                    activate_author(instance.as_ref(), context, services, tasks.as_ref()).await;
                NativeOutcome::result(result, |()| NativeOutcome::Unit)
            }
            NativeOperation::Deactivate(mut context) => {
                context.callback_execution = callback.clone();
                deactivate_author(instance.as_ref(), context, host_services(), tasks.as_ref()).await
            }
            NativeOperation::Command {
                invocation_id,
                mut context,
            } => {
                context.host_service_port = host_services();
                NativeOutcome::Command(instance.command(call(invocation_id), context).await)
            }
            NativeOperation::Invoke {
                invocation_id,
                mut context,
            } => {
                context.host_service_port = host_services();
                NativeOutcome::result(
                    instance.invoke(call(invocation_id), context).await,
                    NativeOutcome::Service,
                )
            }
            NativeOperation::Streaming {
                invocation_id,
                mut context,
            } => {
                context.host_service_port = host_services();
                let Some(ports) = ports.as_ref() else {
                    return NativeOutcome::Error {
                        code: UNSUPPORTED,
                        message: "streaming requires scoped ports".into(),
                    };
                };
                let context = crate::NativeStreamingServiceContext {
                    service: context,
                    events: crate::ServiceEventSinkHandle::new(ports::NativeEventSink {
                        ports: Arc::clone(ports),
                        invocation_id: invocation_id.clone(),
                    }),
                };
                NativeOutcome::result(
                    instance
                        .invoke_streaming(call(invocation_id), context)
                        .await,
                    NativeOutcome::Service,
                )
            }
            NativeOperation::Registrations => {
                NativeOutcome::result(instance.registrations(), NativeOutcome::Registrations)
            }
            NativeOperation::Event(event) => NativeOutcome::result(
                instance
                    .event_scoped(call(crate::PluginInvocationId::new()), event)
                    .await,
                |()| NativeOutcome::Unit,
            ),
            NativeOperation::Extension {
                invocation_id,
                contract,
                payload,
            } => NativeOutcome::result(
                instance
                    .extension(call(invocation_id), contract, payload)
                    .await,
                NativeOutcome::Bytes,
            ),
        }
    })
}
struct BoundedWriter(Vec<u8>);
impl std::io::Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_BUFFER_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other(
                "native outcome exceeds retention bound",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn encode_bounded(value: &impl Serialize) -> Result<Vec<u8>, AbiStatus> {
    let mut output = BoundedWriter(Vec::new());
    serde_json::to_writer(&mut output, value).map_err(|_| LIMIT_EXCEEDED)?;
    Ok(output.0)
}

/// Guard every exported operation, including factory calls and destructors.
pub fn ffi_guard(operation: impl FnOnce() -> AbiStatus) -> AbiStatus {
    catch_unwind(AssertUnwindSafe(operation)).unwrap_or(PANICKED)
}
unsafe fn decode<T: serde::de::DeserializeOwned>(
    input: *const u8,
    length: usize,
) -> Result<T, AbiStatus> {
    if input.is_null() || length == 0 || length > MAX_BUFFER_BYTES {
        return Err(INVALID_ARGUMENT);
    }
    let bytes = unsafe { std::slice::from_raw_parts(input, length) };
    serde_json::from_slice(bytes).map_err(|_| INVALID_ARGUMENT)
}
const unsafe fn copy_output(
    bytes: &[u8],
    output: *mut u8,
    capacity: usize,
    length: *mut usize,
) -> AbiStatus {
    if length.is_null() || capacity > MAX_BUFFER_BYTES || (capacity > 0 && output.is_null()) {
        return INVALID_ARGUMENT;
    }
    if bytes.len() > MAX_BUFFER_BYTES {
        return LIMIT_EXCEEDED;
    }
    unsafe {
        length.write(bytes.len());
    }
    if capacity < bytes.len() {
        return BUFFER_TOO_SMALL;
    }
    if !bytes.is_empty() {
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), output, bytes.len());
        }
    }
    OK
}

/// Used only in a thin cdylib packaging target. Implementation crates use
///
/// `define_plugin!` and emit no global symbols. The returned descriptor is
/// borrowed immutable library memory, valid until the library is unloaded.
#[macro_export]
macro_rules! export_plugin_entrypoint {
    ($definition:path $(,)?) => {
        const _: () = {
            fn adapter() -> &'static $crate::abi::NativeAdapter {
                static ADAPTER: std::sync::OnceLock<$crate::abi::NativeAdapter> =
                    std::sync::OnceLock::new();
                ADAPTER.get_or_init(|| $crate::abi::NativeAdapter::new($definition()))
            }
            unsafe extern "C" fn declarations(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
                $crate::abi::ffi_guard(|| unsafe { adapter().declarations(out, cap, len) })
            }
            unsafe extern "C" fn metadata(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
                $crate::abi::ffi_guard(|| unsafe { adapter().metadata(out, cap, len) })
            }
            unsafe extern "C" fn create(input: *const u8, len: usize, out: *mut u64) -> i32 {
                $crate::abi::ffi_guard(|| unsafe { adapter().create(input, len, out) })
            }
            unsafe extern "C" fn destroy(id: u64) -> i32 {
                $crate::abi::ffi_guard(|| adapter().destroy(id))
            }
            unsafe extern "C" fn start(id: u64, op: u64, input: *const u8, len: usize) -> i32 {
                $crate::abi::ffi_guard(|| unsafe { adapter().start(id, op, input, len) })
            }
            unsafe extern "C" fn start_scoped(
                id: u64,
                op: u64,
                input: *const u8,
                len: usize,
                ports: *const $crate::abi::NativeInvocationPorts,
            ) -> i32 {
                $crate::abi::ffi_guard(|| unsafe {
                    adapter().start_scoped(id, op, input, len, ports)
                })
            }
            unsafe extern "C" fn read(
                id: u64,
                op: u64,
                out: *mut u8,
                cap: usize,
                len: *mut usize,
            ) -> i32 {
                $crate::abi::ffi_guard(|| unsafe { adapter().read(id, op, out, cap, len) })
            }
            unsafe extern "C" fn read_wake(
                id: u64,
                op: u64,
                out: *mut u8,
                cap: usize,
                len: *mut usize,
                waker: *const $crate::abi::NativeHostWakerV1,
            ) -> i32 {
                $crate::abi::ffi_guard(|| unsafe {
                    adapter().read_wake(id, op, out, cap, len, waker)
                })
            }
            unsafe extern "C" fn cancel(id: u64, op: u64) -> i32 {
                $crate::abi::ffi_guard(|| adapter().cancel(id, op))
            }
            /// plug-rt installation: the host hands this library its runtime
            /// table before creating any instance. Refused unless the library
            /// was built in dynamic mode (`--cfg bmux_plugin_dylib`).
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn bmux_plug_rt_install_v1(
                table: *const $crate::rt::abi::PlugRtV1,
                guest: *mut $crate::rt::abi::PlugRtGuestV1,
            ) -> i32 {
                unsafe { $crate::rt::guest::install(table, guest) }
            }
            unsafe extern "C" fn ack(id: u64, op: u64) -> i32 {
                $crate::abi::ffi_guard(|| adapter().ack(id, op))
            }
            static DESCRIPTOR: $crate::abi::NativePluginDescriptorV3HostWakeV1 =
                $crate::abi::NativePluginDescriptorV3HostWakeV1 {
                    base: $crate::abi::NativePluginDescriptorV3 {
                        base: $crate::abi::NativePluginDescriptor {
                            size: $crate::abi::NATIVE_DESCRIPTOR_SIZE_HOST_WAKE_V1,
                            version: $crate::abi::ABI_VERSION,
                            // Callback authority is advertised: the host binds a
                            // lifecycle callback root for every selected activation
                            // and fails closed when an artifact cannot project it.
                            // Host wake v1 lets hosts drive operations by waker.
                            capabilities: $crate::abi::DESCRIPTOR_CAPABILITIES,
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
                };
            #[unsafe(no_mangle)]
            pub extern "C" fn bmux_plugin_entry_v2() -> *const $crate::abi::NativePluginDescriptor {
                &DESCRIPTOR.base.base
            }
        };
    };
}

#[cfg(test)]
mod tests;
