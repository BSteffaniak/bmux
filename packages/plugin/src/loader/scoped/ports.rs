//! Host implementation of the SDK's negotiated opaque invocation ports.
//! Handles are monotonically allocated, never addresses or caller credentials.
#[path = "ports/callback_authority.rs"]
mod callback_authority;
#[cfg(test)]
#[path = "ports/tests.rs"]
mod tests;
use super::{
    Arc, AssertUnwindSafe, BTreeMap, InvocationScope, Mutex, Ordering, Result,
    ServiceEventSinkHandle, abi, catch_unwind, failure, scope_error,
};
use abi::{AbiStatus, NativeHostRequest, NativeInvocationPorts, NativeSlice};
use bmux_plugin_sdk::{BoundHostServicePort, HostServiceRequest, ServiceKind};
use std::sync::{OnceLock, atomic::AtomicU64};

static NEXT: AtomicU64 = AtomicU64::new(1);
static PORTS: OnceLock<Mutex<BTreeMap<u64, Arc<HostPorts>>>> = OnceLock::new();
fn registry() -> &'static Mutex<BTreeMap<u64, Arc<HostPorts>>> {
    PORTS.get_or_init(|| Mutex::new(BTreeMap::new()))
}
#[derive(Clone, Copy)]
pub(super) enum InvocationTable {
    V3Callback(abi::callback_authority::NativeInvocationPortsV3CallbackAuthorityV1),
    V2(NativeInvocationPorts),
    V3(abi::NativeInvocationPortsV3),
    V3Retained(abi::NativeInvocationPortsV3RetainedHostV2),
    V3Availability(abi::NativeInvocationPortsV3Availability),
}
impl std::ops::Deref for InvocationTable {
    type Target = NativeInvocationPorts;
    fn deref(&self) -> &Self::Target {
        match self {
            Self::V3Callback(table) => &table.base.base.base.base.base,
            Self::V2(base) => base,
            Self::V3(table) => &table.base,
            Self::V3Retained(table) => &table.base.base.base.base,
            Self::V3Availability(table) => &table.base.base,
        }
    }
}
impl std::ops::DerefMut for InvocationTable {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            Self::V3Callback(table) => &mut table.base.base.base.base.base,
            Self::V2(base) => base,
            Self::V3(table) => &mut table.base,
            Self::V3Retained(table) => &mut table.base.base.base.base,
            Self::V3Availability(table) => &mut table.base.base,
        }
    }
}
impl InvocationTable {
    pub(super) fn as_ptr(&self) -> *const NativeInvocationPorts {
        &raw const **self
    }
}

pub(super) struct HostPorts {
    callback: Option<super::callbacks::CallbackBinding>,
    owned_entry: Option<super::callbacks::CallbackEntryLease>,
    retained_execution: Option<Arc<super::LoadedExecution>>,
    async_activation: bool,
    activation: Option<bmux_plugin_runtime::ActivationScope>,
    activation_host: Option<bmux_plugin_sdk::PluginHostServices>,
    native_v3: std::sync::atomic::AtomicBool,
    invocation: InvocationScope,
    events: Option<ServiceEventSinkHandle>,
    host: Option<BoundHostServicePort>,
    kernel: Mutex<Option<crate::InvocationKernelPort>>,
    state: Mutex<PortState>,
    registration: Mutex<Option<crate::ActivationRegistrationPort>>,
}
enum ChildOutcome {
    Bytes(Vec<u8>),
    Failed(AbiStatus),
}
struct PortState {
    closed: bool,
    retained: bool,
    next_child: u64,
    children: BTreeMap<u64, Option<ChildOutcome>>,
    child_bytes: usize,
    admissions: BTreeMap<u64, bmux_plugin_runtime::AdmissionGuard>,
}
impl HostPorts {
    pub(super) fn new(
        invocation: InvocationScope,
        events: Option<ServiceEventSinkHandle>,
        host: Option<BoundHostServicePort>,
    ) -> Arc<Self> {
        Arc::new(Self {
            callback: None,
            owned_entry: None,
            retained_execution: None,
            async_activation: false,
            activation: None,
            activation_host: None,
            native_v3: std::sync::atomic::AtomicBool::new(false),
            invocation,
            events,
            host,
            kernel: Mutex::new(None),
            registration: Mutex::new(None),
            state: Mutex::new(PortState {
                closed: false,
                retained: false,
                next_child: 1,
                children: BTreeMap::new(),
                child_bytes: 0,
                admissions: BTreeMap::new(),
            }),
        })
    }
    pub(super) fn for_activation(
        invocation: InvocationScope,
        activation: bmux_plugin_runtime::ActivationScope,
        host: bmux_plugin_sdk::PluginHostServices,
    ) -> Arc<Self> {
        let mut port = Self::new(invocation, None, None);
        let owner = Arc::get_mut(&mut port).expect("new port has one owner");
        owner.activation = Some(activation);
        owner.activation_host = Some(host);
        port
    }

    pub(super) fn with_callback(
        mut self: Arc<Self>,
        callback: Option<super::callbacks::CallbackBinding>,
    ) -> Arc<Self> {
        Arc::get_mut(&mut self)
            .expect("unpublished port has one owner")
            .callback = callback;
        self
    }
    fn bind_entry(
        &self,
        entry: super::callbacks::CallbackEntryLease,
    ) -> Result<abi::callback_authority::NativeInvocationPortsV3CallbackAuthorityV1> {
        let binding = self
            .callback
            .as_ref()
            .ok_or_else(|| failure("native callback binding missing"))?;
        entry.validate_binding(binding)?;
        let mut port = Self::new(
            self.invocation.clone(),
            self.events.clone(),
            self.host.clone(),
        )
        .with_callback(self.callback.clone())
        .with_retained_execution(self.retained_execution.clone());
        let owned = Arc::get_mut(&mut port).expect("unpublished port has one owner");
        owned.activation.clone_from(&self.activation);
        owned.activation_host.clone_from(&self.activation_host);
        owned.async_activation = self.async_activation;
        owned.owned_entry = Some(entry);
        owned
            .state
            .lock()
            .map_err(|_| failure("native port state poisoned"))?
            .retained = true;
        owned
            .registration
            .lock()
            .map_err(|_| failure("registration port poisoned"))?
            .clone_from(
                &*self
                    .registration
                    .lock()
                    .map_err(|_| failure("registration port poisoned"))?,
            );
        let kernel = self
            .kernel
            .lock()
            .map_err(|_| failure("kernel port poisoned"))?
            .clone();
        match port.install_for(abi::ABI_VERSION, kernel)? {
            InvocationTable::V3Callback(table) => Ok(table),
            _ => Err(failure("callback entry did not produce negotiated ports")),
        }
    }
    pub(super) fn with_retained_execution(
        mut self: Arc<Self>,
        execution: Option<Arc<super::LoadedExecution>>,
    ) -> Arc<Self> {
        Arc::get_mut(&mut self)
            .expect("unpublished port has one owner")
            .retained_execution = execution;
        self
    }

    pub(super) fn with_async_activation(mut self: Arc<Self>) -> Arc<Self> {
        Arc::get_mut(&mut self)
            .expect("unpublished port has one owner")
            .async_activation = true;
        self
    }

    pub(super) fn install(self: &Arc<Self>) -> Result<NativeInvocationPorts> {
        let size = u32::try_from(std::mem::size_of::<NativeInvocationPorts>())
            .map_err(|_| failure("native invocation port size exceeds ABI range"))?;
        let handle = NEXT
            .try_update(Ordering::AcqRel, Ordering::Acquire, |id| id.checked_add(1))
            .map_err(|_| failure("native callback identity exhausted"))?;
        let mut registry = registry()
            .lock()
            .map_err(|_| failure("native port registry poisoned"))?;
        if registry.len() >= 4096 {
            return Err(failure("native port capacity exhausted"));
        }
        registry.insert(handle, self.clone());
        drop(registry);
        Ok(NativeInvocationPorts {
            size,
            version: abi::PORTS_VERSION,
            capabilities: self.capabilities(),
            handle,
            retain,
            release,
            emit_event,
            host_begin,
            host_read,
            host_ack,
            registration_begin,
        })
    }
    fn validate_installation(&self, version: u32, has_kernel: bool) -> Result<()> {
        if self
            .host
            .as_ref()
            .is_some_and(|host| host.invocation_scope().id() != self.invocation.id())
        {
            return Err(failure(
                "native host port belongs to a different invocation",
            ));
        }
        if self.retained_execution.as_ref().is_some_and(|execution| {
            execution.scope.id() != self.invocation.instance_id()
                || execution.scope.runtime_id() != self.invocation.runtime_id()
        }) {
            return Err(failure(
                "retained host belongs to a different execution scope",
            ));
        }
        if version == abi::ABI_VERSION_V2 && self.retained_execution.is_some() {
            return Err(failure("v2 cannot carry retained host authority"));
        }
        if version == abi::ABI_VERSION_V2 && has_kernel {
            return Err(failure(
                "v2 native invocation cannot carry kernel authority",
            ));
        }
        if !matches!(version, abi::ABI_VERSION_V2 | abi::ABI_VERSION) {
            return Err(failure("unsupported native invocation version"));
        }
        if self.callback.is_some() && version != abi::ABI_VERSION {
            return Err(failure("callback authority requires v3 invocation ports"));
        }
        Ok(())
    }
    pub(super) fn install_for(
        self: &Arc<Self>,
        version: u32,
        kernel: Option<crate::InvocationKernelPort>,
    ) -> Result<InvocationTable> {
        self.validate_installation(version, kernel.is_some())?;
        let has_callback = self.callback.is_some();
        let has_availability = self.host.is_some() || self.activation_host.is_some();
        let v3_size =
            u32::try_from(if has_callback {
                std::mem::size_of::<
                    abi::callback_authority::NativeInvocationPortsV3CallbackAuthorityV1,
                >()
            } else if self.retained_execution.is_some() {
                std::mem::size_of::<abi::NativeInvocationPortsV3RetainedHostV2>()
            } else if has_availability {
                std::mem::size_of::<abi::NativeInvocationPortsV3Availability>()
            } else {
                std::mem::size_of::<abi::NativeInvocationPortsV3>()
            })
            .map_err(|_| failure("native v3 port size exceeds ABI range"))?;
        let has_kernel = kernel.is_some();
        *self
            .kernel
            .lock()
            .map_err(|_| failure("kernel port poisoned"))? = kernel;
        self.native_v3
            .store(version == abi::ABI_VERSION, Ordering::Release);
        let mut base = self.install()?;
        if version == abi::ABI_VERSION_V2 {
            return Ok(InvocationTable::V2(base));
        }
        base.size = v3_size;
        base.version = abi::PORTS_VERSION_V3;
        if self.async_activation {
            base.capabilities |= abi::CAP_ASYNC_ACTIVATION;
        }
        if self.activation.is_some() {
            base.capabilities |= abi::CAP_RETAINED_LIFECYCLE;
        }
        if has_kernel {
            base.capabilities |= abi::CAP_KERNEL_REQUESTS;
        }
        if has_callback {
            base.capabilities |= abi::callback_authority::CAP_CALLBACK_AUTHORITY_V1;
            if self.retained_execution.is_some() {
                base.capabilities |=
                    abi::CAP_RETAINED_ASYNC_HOST_V1 | abi::CAP_RETAINED_HOST_LOOKUP_V2;
            }
            if has_availability {
                base.capabilities |= abi::CAP_SELECTED_SERVICE_AVAILABILITY_V1;
            }
            return Ok(InvocationTable::V3Callback(
                abi::callback_authority::NativeInvocationPortsV3CallbackAuthorityV1 {
                    base: abi::NativeInvocationPortsV3RetainedHostV2 {
                        base: abi::NativeInvocationPortsV3RetainedHost {
                            base: abi::NativeInvocationPortsV3Availability {
                                base: abi::NativeInvocationPortsV3 { base, kernel_begin },
                                availability_begin,
                            },
                            retained_host_open,
                        },
                        retained_host_open_v2,
                    },
                    callback_authority_open: callback_authority::open,
                },
            ));
        }
        if self.retained_execution.is_some() {
            base.capabilities |= abi::CAP_RETAINED_ASYNC_HOST_V1 | abi::CAP_RETAINED_HOST_LOOKUP_V2;
            if has_availability {
                base.capabilities |= abi::CAP_SELECTED_SERVICE_AVAILABILITY_V1;
            }
            return Ok(InvocationTable::V3Retained(
                abi::NativeInvocationPortsV3RetainedHostV2 {
                    base: abi::NativeInvocationPortsV3RetainedHost {
                        base: abi::NativeInvocationPortsV3Availability {
                            base: abi::NativeInvocationPortsV3 { base, kernel_begin },
                            availability_begin,
                        },
                        retained_host_open,
                    },
                    retained_host_open_v2,
                },
            ));
        }
        if has_availability {
            base.capabilities |= abi::CAP_SELECTED_SERVICE_AVAILABILITY_V1;
            return Ok(InvocationTable::V3Availability(
                abi::NativeInvocationPortsV3Availability {
                    base: abi::NativeInvocationPortsV3 { base, kernel_begin },
                    availability_begin,
                },
            ));
        }
        Ok(InvocationTable::V3(abi::NativeInvocationPortsV3 {
            base,
            kernel_begin,
        }))
    }

    const fn capabilities(&self) -> u64 {
        (if self.events.is_some() {
            abi::CAP_STREAM_EVENTS
        } else {
            0
        }) | (if self.host.is_some() || self.activation_host.is_some() {
            abi::CAP_HOST_SERVICES
        } else {
            0
        })
    }
    pub(super) fn bind_registration(
        table: &abi::NativeInvocationPorts,
        registration: Option<crate::ActivationRegistrationPort>,
    ) -> Result<()> {
        *lookup(table.handle)?
            .registration
            .lock()
            .map_err(|_| failure("registration port poisoned"))? = registration;
        Ok(())
    }
    pub(super) fn retire(table: &NativeInvocationPorts) -> Result<()> {
        let port = lookup(table.handle)?;
        let mut state = port
            .state
            .lock()
            .map_err(|_| failure("native port state poisoned"))?;
        if state.retained {
            return Err(failure("plugin still retains native invocation port"));
        }
        state.closed = true;
        drop(state);
        registry()
            .lock()
            .map_err(|_| failure("native port registry poisoned"))?
            .remove(&table.handle);
        Ok(())
    }
}
/// The ABI carries only a status code, so record why a host callback failed
/// on the host side; otherwise a failed native host call leaves no trace.
fn log_callback_failure(error: &bmux_plugin_sdk::PluginError) {
    tracing::warn!(target: "bmux_plugin::native_ports", %error, "native host callback failed");
}
fn lookup(handle: u64) -> Result<Arc<HostPorts>> {
    registry()
        .lock()
        .map_err(|_| failure("native port registry poisoned"))?
        .get(&handle)
        .cloned()
        .ok_or_else(|| failure("native invocation capability expired"))
}
fn guarded(work: impl FnOnce() -> Result<()>) -> AbiStatus {
    match catch_unwind(AssertUnwindSafe(work)) {
        Ok(Ok(())) => abi::OK,
        Ok(Err(error)) => {
            log_callback_failure(&error);
            abi::FAILED
        }
        Err(_) => abi::PANICKED,
    }
}
unsafe extern "C" fn retain(handle: u64) -> AbiStatus {
    guarded(|| {
        let port = lookup(handle)?;
        let _admission = port.invocation.try_admit().map_err(scope_error)?;
        let mut state = port
            .state
            .lock()
            .map_err(|_| failure("native port state poisoned"))?;
        if state.closed || state.retained {
            return Err(failure("native invocation port closed or already retained"));
        }
        state.retained = true;
        drop(state);
        Ok(())
    })
}
unsafe extern "C" fn release(handle: u64) -> AbiStatus {
    guarded(|| {
        let port = lookup(handle)?;
        let mut state = port
            .state
            .lock()
            .map_err(|_| failure("native port state poisoned"))?;
        if !state.retained || !state.children.is_empty() {
            return Err(failure(
                "native port still owns child outcomes or was released",
            ));
        }
        state.closed = true;
        state.retained = false;
        drop(state);
        if port.owned_entry.is_some() {
            registry()
                .lock()
                .map_err(|_| failure("native port registry poisoned"))?
                .remove(&handle);
        }
        Ok(())
    })
}
unsafe fn borrowed<'a>(pointer: *const u8, length: usize) -> Result<&'a [u8]> {
    if length > abi::MAX_BUFFER_BYTES || (length != 0 && pointer.is_null()) {
        return Err(failure("invalid callback bytes"));
    }
    if length == 0 {
        Ok(&[])
    } else {
        Ok(unsafe { std::slice::from_raw_parts(pointer, length) })
    }
}
unsafe extern "C" fn emit_event(handle: u64, bytes: *const u8, length: usize) -> AbiStatus {
    let mut code = abi::OK;
    let result = guarded(|| {
        let port = lookup(handle)?;
        let _admission = match host_start_admission(&port)? {
            Ok(admission) => admission,
            Err(cancelled) => {
                code = cancelled;
                return Ok(());
            }
        };
        let state = port
            .state
            .lock()
            .map_err(|_| failure("native port state poisoned"))?;
        if state.closed || !state.retained {
            return Err(failure("native callback is closed"));
        }
        drop(state);
        let event = serde_json::from_slice(unsafe { borrowed(bytes, length) }?)
            .map_err(|e| failure(e.to_string()))?;
        // Once delivery is attempted, any error stays FAILED, even when
        // cancellation occurs concurrently. Never infer delivery from its text.
        port.events
            .as_ref()
            .ok_or_else(|| failure("stream events not negotiated"))?
            .emit_event(event)
    });
    if result == abi::OK { code } else { result }
}
unsafe fn text<'a>(slice: NativeSlice) -> Result<&'a str> {
    std::str::from_utf8(unsafe { borrowed(slice.pointer, slice.length) }?)
        .map_err(|e| failure(e.to_string()))
}
// Only cancellation verified before dispatch is recoverable cancellation.
// A dispatch error after effects may have started remains FAILED, never inferred
// from text or from a coincident later cancellation.
fn host_start_admission(
    port: &HostPorts,
) -> Result<std::result::Result<bmux_plugin_runtime::AdmissionGuard, AbiStatus>> {
    if let Some(entry) = &port.owned_entry {
        let binding = port
            .callback
            .as_ref()
            .ok_or_else(|| failure("native entry ports lost callback binding"))?;
        entry.validate_binding(binding)?;
    }
    let cancelled = || {
        if port.native_v3.load(Ordering::Acquire) {
            abi::CANCELLED
        } else {
            abi::FAILED
        }
    };
    if let Some(activation) = &port.activation
        && activation.state().map_err(scope_error)?
            != bmux_plugin_runtime::LifecycleState::Activating
    {
        return activation.try_admit().map(Ok).map_err(scope_error);
    }
    if port.invocation.is_cancelled().map_err(scope_error)? {
        return Ok(Err(cancelled()));
    }
    match port.invocation.try_admit() {
        Ok(admission) => Ok(Ok(admission)),
        Err(error) => {
            if port.invocation.is_cancelled().map_err(scope_error)? {
                Ok(Err(cancelled()))
            } else {
                Err(scope_error(error))
            }
        }
    }
}

// Validate the ABI header before reading request fields. The returned borrow
// remains callback-local; no dispatch or child reservation occurs here.
unsafe fn validated_host_request<'a>(
    request: *const NativeHostRequest,
    child: *mut u64,
) -> Result<&'a NativeHostRequest> {
    if request.is_null()
        || child.is_null()
        || !(request as usize).is_multiple_of(std::mem::align_of::<NativeHostRequest>())
    {
        return Err(failure("invalid native host request"));
    }
    let size = unsafe { std::ptr::addr_of!((*request).size).read() };
    if (size as usize) < std::mem::size_of::<NativeHostRequest>() {
        return Err(failure("unsupported host request size"));
    }
    Ok(unsafe { &*request })
}

unsafe fn decode_host_request(request: &NativeHostRequest) -> Result<HostServiceRequest<'_>> {
    let kind = match request.kind {
        0 => ServiceKind::Query,
        1 => ServiceKind::Command,
        2 => ServiceKind::Event,
        _ => return Err(failure("unsupported native host request kind")),
    };
    Ok(HostServiceRequest {
        capability: unsafe { text(request.capability) }?,
        kind,
        interface: unsafe { text(request.interface) }?,
        operation: unsafe { text(request.operation) }?,
        payload: unsafe { borrowed(request.payload.pointer, request.payload.length) }?,
    })
}

unsafe extern "C" fn host_begin(
    handle: u64,
    request: *const NativeHostRequest,
    child: *mut u64,
) -> AbiStatus {
    let mut code = abi::OK;
    let result = guarded(|| {
        let request = unsafe { validated_host_request(request, child) }?;
        let port = lookup(handle)?;
        let admission = match host_start_admission(&port)? {
            Ok(admission) => admission,
            Err(cancelled) => {
                code = cancelled;
                unsafe {
                    child.write(0);
                }
                return Ok(());
            }
        };
        let host = port
            .activation_host
            .clone()
            .or_else(|| {
                port.host
                    .clone()
                    .map(super::LoadedExecution::bound_host_services)
            })
            .ok_or_else(|| failure("host service port not negotiated"))?;
        let request = unsafe { decode_host_request(request) }?;
        let id = {
            let mut state = port
                .state
                .lock()
                .map_err(|_| failure("native port state poisoned"))?;
            if state.closed
                || !state.retained
                || state.children.len() >= 32
                || state.child_bytes
                    > abi::MAX_BUFFER_BYTES
                        .saturating_mul(32)
                        .saturating_sub(abi::MAX_BUFFER_BYTES)
            {
                return Err(failure("native host outcome budget exhausted or closed"));
            }
            let id = state.next_child;
            state.next_child = id
                .checked_add(1)
                .ok_or_else(|| failure("native child identity exhausted"))?;
            state.children.insert(id, None);
            state.admissions.insert(id, admission);
            // Reserve bytes before calling author code, even for concurrent children.
            state.child_bytes += abi::MAX_BUFFER_BYTES;
            id
        };
        // Publish the identity before effects. Read/ack never reexecute dispatch.
        unsafe {
            child.write(id);
        }
        let result = host.invoke(request);
        let bytes = match result {
            Ok(bytes) if bytes.len() <= abi::MAX_BUFFER_BYTES => bytes,
            Err(bmux_plugin_sdk::PluginError::InvocationCancelled)
                if port.native_v3.load(Ordering::Acquire) =>
            {
                // The selected dispatcher guarantees this typed rejection was
                // before effects. Never infer it from text or a later token.
                let mut state = port
                    .state
                    .lock()
                    .map_err(|_| failure("native port state poisoned"))?;
                state.children.remove(&id);
                state.admissions.remove(&id);
                state.child_bytes -= abi::MAX_BUFFER_BYTES;
                drop(state);
                unsafe {
                    child.write(0);
                }
                code = abi::CANCELLED;
                return Ok(());
            }
            other => {
                let mut state = port
                    .state
                    .lock()
                    .map_err(|_| failure("native port state poisoned"))?;
                state.children.remove(&id);
                state.admissions.remove(&id);
                state.child_bytes -= abi::MAX_BUFFER_BYTES;
                drop(state);
                // The native caller only sees a status code; keep the cause.
                match other {
                    Err(error) => tracing::warn!(%error, "native host invocation failed"),
                    Ok(bytes) => tracing::warn!(
                        len = bytes.len(),
                        "native host invocation exceeded its response budget"
                    ),
                }
                return Err(failure(
                    "host invocation failed or exceeded its response budget",
                ));
            }
        };
        port.state
            .lock()
            .map_err(|_| failure("native port state poisoned"))?
            .children
            .insert(id, Some(ChildOutcome::Bytes(bytes)));
        Ok(())
    });
    if result == abi::OK { code } else { result }
}
// Native invocation callbacks run on the host's blocking plugin worker. Drive
// the selected asynchronous lookup with wake/park, never a no-op-waker poll or
// synchronous dispatch fallback. The retained child owns admission throughout.
fn wait_for_availability<F: std::future::Future>(future: F) -> F::Output {
    struct WakeThread(std::thread::Thread);
    impl std::task::Wake for WakeThread {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = std::task::Waker::from(Arc::new(WakeThread(std::thread::current())));
    let mut context = std::task::Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            std::task::Poll::Ready(outcome) => return outcome,
            std::task::Poll::Pending => std::thread::park(),
        }
    }
}

unsafe extern "C" fn retained_host_open(
    handle: u64,
    version: u16,
    output: *mut abi::retained::NativeRetainedHostPortV1,
) -> AbiStatus {
    let mut code = abi::OK;
    let result = guarded(|| {
        if version != 1 {
            code = abi::UNSUPPORTED;
            return Ok(());
        }
        if output.is_null() {
            code = abi::INVALID_ARGUMENT;
            return Ok(());
        }
        let port = lookup(handle)?;
        if !port.native_v3.load(Ordering::Acquire) {
            code = abi::UNSUPPORTED;
            return Ok(());
        }
        let Some(execution) = &port.retained_execution else {
            code = abi::UNSUPPORTED;
            return Ok(());
        };
        let _admission = match host_start_admission(&port)? {
            Ok(admission) => admission,
            Err(status) => {
                code = status;
                return Ok(());
            }
        };
        let table = execution.open_retained_host_table()?;
        unsafe {
            output.write(table);
        }
        Ok(())
    });
    if result == abi::OK { code } else { result }
}

unsafe extern "C" fn retained_host_open_v2(
    handle: u64,
    version: u16,
    output: *mut abi::retained::NativeRetainedHostPortV2,
) -> AbiStatus {
    if version != 2 {
        return abi::UNSUPPORTED;
    }
    if output.is_null() {
        return abi::INVALID_ARGUMENT;
    }
    // Reuse the same owner admission and reference acquisition as v1, but never
    // retry a rejected open or transfer ownership through a second open.
    let mut base = std::mem::MaybeUninit::uninit();
    let status = unsafe { retained_host_open(handle, 1, base.as_mut_ptr()) };
    if status == abi::OK {
        unsafe {
            output.write(super::retained::table_v2(base.assume_init()));
        }
    }
    status
}

unsafe extern "C" fn availability_begin(
    handle: u64,
    version: u16,
    input: *const u8,
    length: usize,
    child: *mut u64,
) -> AbiStatus {
    use bmux_plugin_sdk::typed_dispatch::{
        SELECTED_SERVICE_AVAILABILITY_VERSION, ServiceAvailabilityKey,
    };
    let mut code = abi::OK;
    let result = guarded(|| {
        if child.is_null() || length > abi::MAX_BUFFER_BYTES || (length != 0 && input.is_null()) {
            code = abi::INVALID_ARGUMENT;
            return Ok(());
        }
        unsafe {
            child.write(0);
        }
        if version != SELECTED_SERVICE_AVAILABILITY_VERSION {
            code = abi::UNSUPPORTED;
            return Ok(());
        }
        let port = lookup(handle)?;
        if !port.native_v3.load(Ordering::Acquire)
            || (port.host.is_none() && port.activation_host.is_none())
        {
            code = abi::UNSUPPORTED;
            return Ok(());
        }
        let bytes = if length == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(input, length) }
        };
        // New contract: typed-stable only, no positional or JSON fallback.
        let key: ServiceAvailabilityKey =
            bmux_codec::from_typed_bytes(bytes).map_err(|error| failure(error.to_string()))?;
        key.validate()?;
        let admission = match host_start_admission(&port)? {
            Ok(admission) => admission,
            Err(status) => {
                code = status;
                return Ok(());
            }
        };
        let id = reserve_availability_child(&port, admission)?;
        unsafe {
            child.write(id);
        }
        let outcome = lookup_availability(&port, key);
        port.state
            .lock()
            .map_err(|_| failure("native port state poisoned"))?
            .children
            .insert(id, Some(outcome));
        Ok(())
    });
    if result == abi::OK { code } else { result }
}

fn lookup_availability(
    port: &HostPorts,
    key: bmux_plugin_sdk::typed_dispatch::ServiceAvailabilityKey,
) -> ChildOutcome {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let future = if let Some(host) = &port.activation_host {
            host.selected_service_availability(key)
        } else {
            port.host
                .as_ref()
                .ok_or_else(|| failure("availability port missing"))?
                .selected_service_availability(key)
        };
        let availability = wait_for_availability(future)?;
        bmux_plugin_sdk::encode_service_message(&availability)
    }));
    match outcome {
        Ok(Ok(bytes)) if bytes.len() <= abi::MAX_BUFFER_BYTES => ChildOutcome::Bytes(bytes),
        Ok(Ok(_)) => ChildOutcome::Failed(abi::LIMIT_EXCEEDED),
        Ok(Err(error)) => {
            log_callback_failure(&error);
            ChildOutcome::Failed(abi::FAILED)
        }
        Err(_) => ChildOutcome::Failed(abi::PANICKED),
    }
}

fn reserve_availability_child(
    port: &HostPorts,
    admission: bmux_plugin_runtime::AdmissionGuard,
) -> Result<u64> {
    let mut state = port
        .state
        .lock()
        .map_err(|_| failure("native port state poisoned"))?;
    if state.closed || !state.retained || state.children.len() >= 32 {
        return Err(failure("availability child capacity exhausted"));
    }
    let id = state.next_child;
    state.next_child = id
        .checked_add(1)
        .ok_or_else(|| failure("native child identity exhausted"))?;
    state.children.insert(id, None);
    state.admissions.insert(id, admission);
    state.child_bytes += abi::MAX_BUFFER_BYTES;
    drop(state);
    Ok(id)
}

unsafe extern "C" fn host_read(
    handle: u64,
    child: u64,
    output: *mut u8,
    capacity: usize,
    length: *mut usize,
) -> AbiStatus {
    let mut code = abi::OK;
    let result = guarded(|| {
        if length.is_null()
            || capacity > abi::MAX_BUFFER_BYTES
            || (capacity != 0 && output.is_null())
        {
            return Err(failure("invalid native host output"));
        }
        let port = lookup(handle)?;
        // Settlement of retained bytes is not new effect admission. The registry
        // owner and state lock fence retirement even after scope cancellation.
        let state = port
            .state
            .lock()
            .map_err(|_| failure("native port state poisoned"))?;
        let outcome = state
            .children
            .get(&child)
            .and_then(Option::as_ref)
            .ok_or_else(|| failure("native child unavailable"))?;
        let bytes = match outcome {
            ChildOutcome::Bytes(bytes) => bytes.clone(),
            ChildOutcome::Failed(status) => {
                code = *status;
                unsafe {
                    length.write(0);
                }
                return Ok(());
            }
        };
        drop(state);
        unsafe {
            length.write(bytes.len());
        }
        if capacity < bytes.len() {
            code = abi::BUFFER_TOO_SMALL;
            return Ok(());
        }
        if !bytes.is_empty() {
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), output, bytes.len());
            }
        }
        Ok(())
    });
    if result == abi::OK { code } else { result }
}
unsafe extern "C" fn kernel_begin(
    handle: u64,
    bytes: *const u8,
    length: usize,
    child: *mut u64,
) -> AbiStatus {
    // Even absent grants provide a valid callback, but never an ambient executor.
    let Ok(port) = lookup(handle) else {
        return abi::NOT_FOUND;
    };
    let kernel = match port.kernel.lock() {
        Ok(kernel) => kernel.clone(),
        Err(_) => return abi::FAILED,
    };
    let Some(kernel) = kernel else {
        return abi::UNSUPPORTED;
    };
    let mut code = abi::OK;
    let result = guarded(|| {
        if child.is_null() {
            return Err(failure("missing kernel child output"));
        }
        let request = unsafe { borrowed(bytes, length) }?;
        let admission = match host_start_admission(&port)? {
            Ok(admission) => admission,
            Err(cancelled) => {
                code = cancelled;
                unsafe {
                    child.write(0);
                }
                return Ok(());
            }
        };
        let id = {
            let mut state = port
                .state
                .lock()
                .map_err(|_| failure("native port state poisoned"))?;
            if state.closed || !state.retained || state.children.len() >= 32 {
                return Err(failure("kernel child capacity exhausted"));
            }
            let id = state.next_child;
            state.next_child = id
                .checked_add(1)
                .ok_or_else(|| failure("native child identity exhausted"))?;
            state.children.insert(id, None);
            state.admissions.insert(id, admission);
            state.child_bytes += abi::MAX_BUFFER_BYTES;
            id
        };
        // OK means child admission, not successful kernel execution. Publish
        // its identity before effects and retain a terminal read disposition.
        unsafe {
            child.write(id);
        }
        let outcome = match catch_unwind(AssertUnwindSafe(|| {
            port.activation_host.as_ref().map_or_else(
                || kernel.execute_encoded(request),
                |host| host.execute_kernel_encoded(request),
            )
        })) {
            Ok(Ok(bytes)) if bytes.len() <= abi::MAX_BUFFER_BYTES => ChildOutcome::Bytes(bytes),
            Ok(Ok(_)) => ChildOutcome::Failed(abi::LIMIT_EXCEEDED),
            Ok(Err(error)) => {
                log_callback_failure(&error);
                ChildOutcome::Failed(abi::FAILED)
            }
            Err(_) => ChildOutcome::Failed(abi::PANICKED),
        };
        port.state
            .lock()
            .map_err(|_| failure("native port state poisoned"))?
            .children
            .insert(id, Some(outcome));
        Ok(())
    });
    if result == abi::OK { code } else { result }
}

unsafe extern "C" fn registration_begin(
    handle: u64,
    bytes: *const u8,
    length: usize,
    child: *mut u64,
) -> AbiStatus {
    guarded(|| {
        if child.is_null() {
            return Err(failure("missing registration child output"));
        }
        let port = lookup(handle)?;
        let admission = port.invocation.try_admit().map_err(scope_error)?;
        let registration = port
            .registration
            .lock()
            .map_err(|_| failure("registration port poisoned"))?
            .clone()
            .ok_or_else(|| failure("activation registration port not bound"))?;
        let mutation = serde_json::from_slice(unsafe { borrowed(bytes, length) }?)
            .map_err(|e| failure(e.to_string()))?;
        let id = {
            let mut state = port
                .state
                .lock()
                .map_err(|_| failure("native port state poisoned"))?;
            if state.closed || !state.retained || state.children.len() >= 32 {
                return Err(failure("registration child capacity exhausted"));
            }
            let id = state.next_child;
            state.next_child = id
                .checked_add(1)
                .ok_or_else(|| failure("native child identity exhausted"))?;
            state.children.insert(id, None);
            state.admissions.insert(id, admission);
            state.child_bytes += abi::MAX_BUFFER_BYTES;
            id
        };
        unsafe {
            child.write(id);
        }
        match super::apply_registration(&registration, mutation) {
            Ok(bytes) => {
                port.state
                    .lock()
                    .map_err(|_| failure("native port state poisoned"))?
                    .children
                    .insert(id, Some(ChildOutcome::Bytes(bytes)));
                Ok(())
            }
            Err(error) => {
                let mut state = port
                    .state
                    .lock()
                    .map_err(|_| failure("native port state poisoned"))?;
                state.children.remove(&id);
                state.admissions.remove(&id);
                state.child_bytes -= abi::MAX_BUFFER_BYTES;
                drop(state);
                Err(error)
            }
        }
    })
}

unsafe extern "C" fn host_ack(handle: u64, child: u64) -> AbiStatus {
    guarded(|| {
        let port = lookup(handle)?;
        // Acknowledgement retires an existing outcome, including after cancel.
        // It cannot dispatch or revive the invocation.
        let mut state = port
            .state
            .lock()
            .map_err(|_| failure("native port state poisoned"))?;
        if !state.children.get(&child).is_some_and(Option::is_some) {
            return Err(failure("native child not terminal"));
        }
        state.children.remove(&child);
        state.admissions.remove(&child);
        state.child_bytes -= abi::MAX_BUFFER_BYTES;
        drop(state);
        Ok(())
    })
}
