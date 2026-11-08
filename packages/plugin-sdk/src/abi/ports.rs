//! Scoped native callback transport. No callback pointer is serialized.
//!
//! The host supplies one retained opaque invocation handle. All callbacks must
//! catch their own panics and be thread-safe/reentrant; callbacks run without
//! adapter locks. `retain` acquires one ownership reference; `release` relinquishes
//! it only after admission closes AND all admitted callbacks return. Retained
//! author-side clones are closed capabilities, not host references after final.
//! Host begin is synchronous (like `BoundHostServicePort::invoke)`: it returns a
//! retained terminal child outcome, never PENDING. Host read/ack cannot execute
//! effects. Child IDs and bound caller/deadline authority are minted by the host.

use super::{
    AbiStatus, BUFFER_TOO_SMALL, BUSY, FAILED, INVALID_ARGUMENT, LIMIT_EXCEEDED, MAX_BUFFER_BYTES,
    OK, PANICKED, UNSUPPORTED, encode_bounded,
};
use crate::{HostServiceRequest, PluginError, ServiceEvent, ServiceEventSink};
use std::sync::{Arc, Mutex};

pub const PORTS_VERSION: u32 = 2;
pub const PORTS_VERSION_V3: u32 = 3;
pub const CAP_KERNEL_REQUESTS: u64 = 16;
pub const CAP_STREAM_EVENTS: u64 = 1;
pub const CAP_HOST_SERVICES: u64 = 2;
pub const CAP_LIVE_CANCELLATION: u64 = 4;
pub const CAP_ACTIVATION_REGISTRATIONS: u64 = 8;
pub const SCOPED_CAPABILITIES_V2: u64 =
    CAP_STREAM_EVENTS | CAP_HOST_SERVICES | CAP_LIVE_CANCELLATION | CAP_ACTIVATION_REGISTRATIONS;
pub const CAP_ASYNC_ACTIVATION: u64 = 64;
pub const CAP_RETAINED_LIFECYCLE: u64 = 32;
/// Frozen capabilities understood by pre-availability v3 peers.
pub const SCOPED_CAPABILITIES_V3_LEGACY: u64 =
    SCOPED_CAPABILITIES_V2 | CAP_KERNEL_REQUESTS | CAP_RETAINED_LIFECYCLE | CAP_ASYNC_ACTIVATION;
pub const CAP_SELECTED_SERVICE_AVAILABILITY_V1: u64 = 128;
pub const SCOPED_CAPABILITIES_V3_AVAILABILITY: u64 =
    SCOPED_CAPABILITIES_V3_LEGACY | CAP_SELECTED_SERVICE_AVAILABILITY_V1;
pub const CAP_RETAINED_ASYNC_HOST_V1: u64 = 256;
pub const SCOPED_CAPABILITIES_V3_RETAINED_V1: u64 =
    SCOPED_CAPABILITIES_V3_AVAILABILITY | CAP_RETAINED_ASYNC_HOST_V1;
pub const CAP_RETAINED_HOST_LOOKUP_V2: u64 = 512;
pub const SCOPED_CAPABILITIES_V3_RECOVERY_V2: u64 =
    SCOPED_CAPABILITIES_V3_RETAINED_V1 | CAP_RETAINED_HOST_LOOKUP_V2;
pub const SCOPED_CAPABILITIES: u64 =
    SCOPED_CAPABILITIES_V3_RECOVERY_V2 | super::CAP_CALLBACK_AUTHORITY_V1;
pub const MAX_CALLBACKS: usize = 32;

/// V3 child calls may return `CANCELLED` as an explicit terminal child error.
///
/// Begin cancellation must leave no retained child (the host cleans admission);
/// read cancellation still requires a successful ack. Neither author-visible
/// cancellation poisons a subsequently settled parent report. Other failures,
/// including ack/release uncertainty, remain sticky. V2 semantics are unchanged.
///
/// Borrowed slices are valid only until callback return; recipients copy before
/// returning. No owner may deallocate another side's buffers.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NativeSlice {
    pub pointer: *const u8,
    pub length: usize,
}
impl NativeSlice {
    const fn new(bytes: &[u8]) -> Self {
        Self {
            pointer: bytes.as_ptr(),
            length: bytes.len(),
        }
    }
}
#[repr(C)]
pub struct NativeHostRequest {
    pub size: u32,
    /// Query=0, command=1, event=2. Unsupported discriminants are errors.
    pub kind: u32,
    pub capability: NativeSlice,
    pub interface: NativeSlice,
    pub operation: NativeSlice,
    pub payload: NativeSlice,
}

/// Copied fixed-layout table; the memory holding the table is borrowed only
///
/// during `start_scoped`. The callbacks and handle remain valid through release.
/// Every callback is required even when its capability is not selected; unused
/// entries may return UNSUPPORTED. `emit_event` returns backpressure explicitly.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NativeInvocationPorts {
    pub size: u32,
    pub version: u32,
    pub capabilities: u64,
    pub handle: u64,
    pub retain: unsafe extern "C" fn(u64) -> AbiStatus,
    pub release: unsafe extern "C" fn(u64) -> AbiStatus,
    pub emit_event: unsafe extern "C" fn(u64, *const u8, usize) -> AbiStatus,
    pub host_begin: unsafe extern "C" fn(u64, *const NativeHostRequest, *mut u64) -> AbiStatus,
    pub host_read: unsafe extern "C" fn(u64, u64, *mut u8, usize, *mut usize) -> AbiStatus,
    pub host_ack: unsafe extern "C" fn(u64, u64) -> AbiStatus,
    /// Bounded encoded `RegistrationMutation`; returns retained terminal child
    /// outcome retrieved/acknowledged through `host_read/host_ack`. Host validates
    /// against its bound M6 `ActivationRegistrationPort`, never payload ownership.
    pub registration_begin: unsafe extern "C" fn(u64, *const u8, usize, *mut u64) -> AbiStatus,
}
/// Explicit v3 extension; v2's layout and callback semantics remain frozen.
///
/// Pass the address of `base` to `start_scoped`, with version 3 and the size of
/// this complete table. Kernel payloads are opaque `bmux_ipc::Request` bytes;
/// terminal child bytes are `bmux_ipc::ResponsePayload`. Read/ack share the base
/// table's invocation-local child namespace and never repeat the mutation.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NativeInvocationPortsV3 {
    pub base: NativeInvocationPorts,
    pub kernel_begin: unsafe extern "C" fn(u64, *const u8, usize, *mut u64) -> AbiStatus,
}
/// Size-checked v3 tail extension for selected-service availability-v1.
///
/// Pass the address of `base.base`, version 3 and this complete table's size.
/// The capability bit AND full size are required before accessing the tail.
/// `availability_begin` receives contract version 1 and a canonical typed-stable
/// encoded, validated `ServiceAvailabilityKey`. It uses the base retained child
/// namespace; successful read bytes encode `SelectedServiceAvailability` using
/// the same codec. No positional/JSON fallback is part of this new contract.
///
/// The host must validate the admitted caller scope and access against the actual
/// selected registry. Only authoritative missing selection is `Absent`; denied,
/// unsupported, unavailable and failed lookups remain errors. All buffers are
/// bounded by `MAX_BUFFER_BYTES`. Begin/read/ack status semantics are unchanged.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NativeInvocationPortsV3Availability {
    pub base: NativeInvocationPortsV3,
    pub availability_begin: unsafe extern "C" fn(u64, u16, *const u8, usize, *mut u64) -> AbiStatus,
}

/// Independent retained-host open extension, preserving every prior prefix.
///
/// The full tail is required with `CAP_RETAINED_ASYNC_HOST_V1`. Its availability
/// prefix need not be granted; unused callbacks still require valid pointers.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NativeInvocationPortsV3RetainedHost {
    pub base: NativeInvocationPortsV3Availability,
    pub retained_host_open: super::retained::NativeRetainedHostOpenV1,
}

/// Explicit recovery-v2 open tail; all earlier allocations remain frozen.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NativeInvocationPortsV3RetainedHostV2 {
    pub base: NativeInvocationPortsV3RetainedHost,
    pub retained_host_open_v2: super::retained::NativeRetainedHostOpenV2,
}

#[derive(Default)]
struct Gate {
    closed: bool,
    admitted: usize,
    releasing: bool,
    released: bool,
    failure: Option<AbiStatus>,
}
pub struct InvocationPorts {
    table: NativeInvocationPorts,
    callback_authority_open:
        Option<unsafe extern "C" fn(u64, u16, *mut super::NativeCallbackAuthorityV1) -> AbiStatus>,
    retained_host_open_v2: Option<super::retained::NativeRetainedHostOpenV2>,
    retained_host_open: Option<super::retained::NativeRetainedHostOpenV1>,
    availability_begin:
        Option<unsafe extern "C" fn(u64, u16, *const u8, usize, *mut u64) -> AbiStatus>,
    kernel_begin: Option<unsafe extern "C" fn(u64, *const u8, usize, *mut u64) -> AbiStatus>,
    cancellation: Mutex<Option<crate::ServiceCancellation>>,
    gate: Mutex<Gate>,
    events: Mutex<()>,
}
struct Admission(Arc<InvocationPorts>);
impl Drop for Admission {
    fn drop(&mut self) {
        if let Ok(mut gate) = self.0.gate.lock() {
            gate.admitted -= 1;
        }
        // Release is observed by close_and_drain on the next host poll. No
        // wakeup protocol exists: the descriptor explicitly remains poll driven.
    }
}
fn protocol(status: AbiStatus) -> PluginError {
    PluginError::ServiceProtocol {
        details: format!("native invocation callback status {status}"),
    }
}
impl InvocationPorts {
    /// # Safety
    /// Pointer is aligned/readable for a truthful header, then its advertised
    /// size. Callback/handle lifetime obeys retain/release, including reentrancy.
    pub(crate) unsafe fn acquire(
        pointer: *const NativeInvocationPorts,
    ) -> Result<Arc<Self>, AbiStatus> {
        unsafe { Self::from_native(pointer, true) }
    }

    /// Take ownership of a table whose single reference the host already
    /// transferred (callback entry ports): no further retain is issued, and
    /// closing these ports releases exactly that reference. On error nothing
    /// was taken and the caller still owns the transferred reference.
    ///
    /// # Safety
    /// Same requirements as [`Self::acquire`], and the table must carry one
    /// host-transferred reference.
    pub(crate) unsafe fn adopt(
        pointer: *const NativeInvocationPorts,
    ) -> Result<Arc<Self>, AbiStatus> {
        unsafe { Self::from_native(pointer, false) }
    }

    unsafe fn from_native(
        pointer: *const NativeInvocationPorts,
        retain: bool,
    ) -> Result<Arc<Self>, AbiStatus> {
        if pointer.is_null() {
            return Err(INVALID_ARGUMENT);
        }
        let header = unsafe { pointer.cast::<super::NativeDescriptorHeader>().read() };
        let (supported, kernel_begin) = match header.version {
            PORTS_VERSION
                if (header.size as usize) >= std::mem::size_of::<NativeInvocationPorts>() =>
            {
                (SCOPED_CAPABILITIES_V2, None)
            }
            PORTS_VERSION_V3
                if (header.size as usize) >= std::mem::size_of::<NativeInvocationPortsV3>() =>
            {
                let extension = unsafe { pointer.cast::<NativeInvocationPortsV3>().read() };
                (
                    SCOPED_CAPABILITIES | super::CAP_CALLBACK_AUTHORITY_V1,
                    Some(extension.kernel_begin),
                )
            }
            _ => return Err(UNSUPPORTED),
        };
        let table = unsafe { pointer.read() };
        if table.handle == 0 || table.capabilities & !supported != 0 {
            return Err(UNSUPPORTED);
        }
        let availability_begin = if table.capabilities & CAP_SELECTED_SERVICE_AVAILABILITY_V1 != 0 {
            if header.version != PORTS_VERSION_V3
                || (header.size as usize)
                    < std::mem::size_of::<NativeInvocationPortsV3Availability>()
            {
                return Err(UNSUPPORTED);
            }
            let extension = unsafe { pointer.cast::<NativeInvocationPortsV3Availability>().read() };
            Some(extension.availability_begin)
        } else {
            None
        };
        let retained_host_open = if table.capabilities & CAP_RETAINED_ASYNC_HOST_V1 != 0 {
            if header.version != PORTS_VERSION_V3
                || (header.size as usize)
                    < std::mem::size_of::<NativeInvocationPortsV3RetainedHost>()
            {
                return Err(UNSUPPORTED);
            }
            Some(
                unsafe { pointer.cast::<NativeInvocationPortsV3RetainedHost>().read() }
                    .retained_host_open,
            )
        } else {
            None
        };
        let retained_host_open_v2 = if table.capabilities & CAP_RETAINED_HOST_LOOKUP_V2 != 0 {
            if header.version != PORTS_VERSION_V3
                || (header.size as usize)
                    < std::mem::size_of::<NativeInvocationPortsV3RetainedHostV2>()
            {
                return Err(UNSUPPORTED);
            }
            Some(
                unsafe {
                    pointer
                        .cast::<NativeInvocationPortsV3RetainedHostV2>()
                        .read()
                }
                .retained_host_open_v2,
            )
        } else {
            None
        };
        let callback_authority_open =
            unsafe { Self::callback_open(pointer, header, table.capabilities) }?;
        if retain {
            let status = unsafe { (table.retain)(table.handle) };
            if status != OK {
                return Err(status);
            }
        }
        Ok(Arc::new(Self {
            table,
            kernel_begin,
            availability_begin,
            retained_host_open,
            callback_authority_open,
            retained_host_open_v2,
            cancellation: Mutex::new(None),
            gate: Mutex::new(Gate::default()),
            events: Mutex::new(()),
        }))
    }
    unsafe fn callback_open(
        pointer: *const NativeInvocationPorts,
        header: super::NativeDescriptorHeader,
        capabilities: u64,
    ) -> Result<Option<super::callback_authority::NativeCallbackAuthorityOpenV1>, AbiStatus> {
        if capabilities & super::CAP_CALLBACK_AUTHORITY_V1 == 0 {
            return Ok(None);
        }
        if header.version != PORTS_VERSION_V3
            || (header.size as usize)
                < std::mem::size_of::<super::NativeInvocationPortsV3CallbackAuthorityV1>()
        {
            return Err(UNSUPPORTED);
        }
        Ok(Some(
            unsafe {
                pointer
                    .cast::<super::NativeInvocationPortsV3CallbackAuthorityV1>()
                    .read()
            }
            .callback_authority_open,
        ))
    }
    pub(crate) fn open_callback_authority(
        self: &Arc<Self>,
    ) -> Result<super::NativeCallbackAuthorityV1, AbiStatus> {
        let open = self.callback_authority_open.ok_or(UNSUPPORTED)?;
        let _admitted = self
            .admit(super::CAP_CALLBACK_AUTHORITY_V1)
            .map_err(|_| FAILED)?;
        let mut output = std::mem::MaybeUninit::<super::NativeCallbackAuthorityV1>::uninit();
        let status = unsafe { open(self.table.handle, 1, output.as_mut_ptr()) };
        if status != OK {
            return Err(status);
        }
        let header = unsafe {
            output
                .as_ptr()
                .cast::<super::NativeDescriptorHeader>()
                .read()
        };
        if header.size as usize != std::mem::size_of::<super::NativeCallbackAuthorityV1>()
            || header.version != 1
        {
            return Err(INVALID_ARGUMENT);
        }
        let table = unsafe { output.assume_init() };
        if table.handle == 0 {
            // Invalid transferred authority remains host-pinned; no guessed release.
            return Err(INVALID_ARGUMENT);
        }
        Ok(table)
    }
    pub(crate) fn open_retained_host(
        self: &Arc<Self>,
    ) -> crate::Result<super::retained::RetainedHostPort> {
        let open = self
            .retained_host_open
            .ok_or(PluginError::UnsupportedHostOperation {
                operation: "retained_host_open",
            })?;
        let _admitted = self.admit(CAP_RETAINED_ASYNC_HOST_V1)?;
        let mut output = std::mem::MaybeUninit::uninit();
        let status = unsafe { open(self.table.handle, 1, output.as_mut_ptr()) };
        if status != OK {
            return Err(protocol(status));
        }
        // OK transferred one independently pinned reference, not this gate.
        unsafe { super::retained::RetainedHostPort::from_native_owned(output.assume_init()) }
    }
    pub(crate) fn open_retained_host_v2(
        self: &Arc<Self>,
    ) -> crate::Result<super::retained::RetainedHostPort> {
        let open = self
            .retained_host_open_v2
            .ok_or(PluginError::UnsupportedHostOperation {
                operation: "retained_host_open_v2",
            })?;
        let _admitted = self.admit(CAP_RETAINED_HOST_LOOKUP_V2)?;
        let mut output = std::mem::MaybeUninit::uninit();
        let status = unsafe { open(self.table.handle, 2, output.as_mut_ptr()) };
        if status != OK {
            return Err(protocol(status));
        }
        unsafe { super::retained::RetainedHostPort::from_native_owned_v2(output.assume_init()) }
    }
    pub(crate) const fn capabilities(&self) -> u64 {
        self.table.capabilities
    }
    pub(crate) fn bind_cancellation(
        &self,
        cancellation: crate::ServiceCancellation,
    ) -> Result<(), AbiStatus> {
        let mut bound = self.cancellation.lock().map_err(|_| PANICKED)?;
        if bound.is_some() {
            return Err(INVALID_ARGUMENT);
        }
        *bound = Some(cancellation);
        drop(bound);
        Ok(())
    }
    fn admit(self: &Arc<Self>, capability: u64) -> Result<Admission, PluginError> {
        let mut gate = self.gate.lock().map_err(|_| protocol(PANICKED))?;
        if gate.closed {
            return Err(protocol(FAILED));
        }
        if self.table.capabilities & capability == 0 {
            return Err(protocol(UNSUPPORTED));
        }
        if gate.admitted >= MAX_CALLBACKS {
            return Err(protocol(BUSY));
        }
        gate.admitted += 1;
        drop(gate);
        Ok(Admission(Arc::clone(self)))
    }
    fn begin_failed(&self, status: AbiStatus, child: u64) {
        if status == super::CANCELLED && child != 0 {
            // A rejected begin must not strand a published child identity.
            self.failed(INVALID_ARGUMENT);
        } else {
            self.child_failed(status);
        }
    }
    fn child_failed(&self, status: AbiStatus) {
        // Only an explicitly negotiated terminal child cancellation is a
        // recoverable author-visible error. Never infer this from the parent's
        // token: a real transport failure concurrent with cancel is still fatal.
        if self.table.version != PORTS_VERSION_V3 || status != super::CANCELLED {
            self.failed(status);
        }
    }
    fn failed(&self, status: AbiStatus) {
        if let Ok(mut gate) = self.gate.lock() {
            gate.failure.get_or_insert(status);
        }
    }
    pub(crate) fn settle_activation(&self) -> Result<(), AbiStatus> {
        let gate = self.gate.lock().map_err(|_| PANICKED)?;
        if gate.admitted != 0 {
            return Err(BUSY);
        }
        if let Some(status) = gate.failure {
            return Err(status);
        }
        drop(gate);
        *self.cancellation.lock().map_err(|_| PANICKED)? = None;
        Ok(())
    }

    /// Closes admission and returns BUSY until callbacks drain. Final publication
    /// is forbidden until this returns success or an explicit terminal failure.
    pub(crate) fn close_and_drain(&self) -> Result<(), AbiStatus> {
        let failure = {
            let mut gate = self.gate.lock().map_err(|_| PANICKED)?;
            gate.closed = true;
            if gate.admitted != 0 {
                return Err(BUSY);
            }
            if gate.released {
                return gate.failure.map_or(Ok(()), |status| {
                    Err(if status == BUSY { FAILED } else { status })
                });
            }
            if gate.releasing {
                return Err(BUSY);
            }
            gate.releasing = true;
            gate.failure
        };
        // Closed+zero linearizes before release. No later clone can admit work.
        let status = unsafe { (self.table.release)(self.table.handle) };
        let mut gate = self.gate.lock().map_err(|_| PANICKED)?;
        gate.releasing = false;
        gate.released = true;
        if status != OK {
            gate.failure.get_or_insert(status);
            drop(gate);
            return Err(if status == BUSY { FAILED } else { status });
        }
        drop(gate);
        failure.map_or(Ok(()), |status| {
            Err(if status == BUSY { FAILED } else { status })
        })
    }
    pub(crate) fn host_port(self: &Arc<Self>) -> crate::definition::PluginHostServices {
        let owner = Arc::clone(self);
        let registration_owner = Arc::clone(self);
        let mut port =
            crate::definition::PluginHostServices::new(move |request| owner.invoke(&request))
                .with_registrations(move |mutation| registration_owner.registration(&mutation));
        if self.table.capabilities & CAP_RETAINED_ASYNC_HOST_V1 != 0 {
            let owner = Arc::clone(self);
            port = port.with_retained_host(move || owner.open_retained_host());
        }
        if self.table.capabilities & CAP_RETAINED_HOST_LOOKUP_V2 != 0 {
            let owner = Arc::clone(self);
            port = port.with_retained_host_v2(move || owner.open_retained_host_v2());
        }
        if self.table.capabilities & CAP_SELECTED_SERVICE_AVAILABILITY_V1 != 0 {
            let owner = Arc::clone(self);
            port = port.with_service_availability(move |key| {
                let owner = Arc::clone(&owner);
                Box::pin(async move { owner.availability(&key) })
            });
        }
        if self.table.capabilities & CAP_KERNEL_REQUESTS == 0 {
            return port;
        }
        let owner = Arc::clone(self);
        port.with_kernel(move |request| owner.kernel(request))
    }
    fn availability(
        self: &Arc<Self>,
        key: &crate::typed_dispatch::ServiceAvailabilityKey,
    ) -> crate::Result<crate::typed_dispatch::SelectedServiceAvailability> {
        key.validate()?;
        let _admitted = self.admit(CAP_SELECTED_SERVICE_AVAILABILITY_V1)?;
        let bound = self.cancellation.lock().map_err(|_| protocol(PANICKED))?;
        let cancelled = bound
            .as_ref()
            .is_some_and(crate::ServiceCancellation::is_cancelled);
        drop(bound);
        // A retained activation port has no setup token after settlement; the
        // host callback mints fresh admission against its retained caller scope.
        if cancelled {
            return Err(PluginError::InvocationCancelled);
        }
        let input = crate::encode_service_message(key)?;
        if input.len() > MAX_BUFFER_BYTES {
            return Err(protocol(LIMIT_EXCEEDED));
        }
        let begin = self
            .availability_begin
            .ok_or(PluginError::UnsupportedHostOperation {
                operation: "selected_service_availability",
            })?;
        let mut child = 0;
        let status = unsafe {
            begin(
                self.table.handle,
                crate::typed_dispatch::SELECTED_SERVICE_AVAILABILITY_VERSION,
                input.as_ptr(),
                input.len(),
                &raw mut child,
            )
        };
        if status == super::CANCELLED && child == 0 {
            return Err(PluginError::InvocationCancelled);
        }
        if status != OK {
            self.begin_failed(status, child);
            return Err(protocol(status));
        }
        if child == 0 {
            self.failed(INVALID_ARGUMENT);
            return Err(protocol(INVALID_ARGUMENT));
        }
        let bytes = self.retrieve(child)?;
        bmux_codec::from_typed_bytes(&bytes).map_err(|error| PluginError::ServiceProtocol {
            details: format!("invalid availability-v1 result: {error}"),
        })
    }
    fn kernel(self: &Arc<Self>, request: &[u8]) -> crate::Result<Vec<u8>> {
        let _admitted = self.admit(CAP_KERNEL_REQUESTS)?;
        let bound = self.cancellation.lock().map_err(|_| protocol(PANICKED))?;
        if bound
            .as_ref()
            .is_none_or(crate::ServiceCancellation::is_cancelled)
        {
            return Err(protocol(FAILED));
        }
        drop(bound);
        if request.len() > MAX_BUFFER_BYTES {
            return Err(protocol(INVALID_ARGUMENT));
        }
        let begin = self.kernel_begin.ok_or_else(|| protocol(UNSUPPORTED))?;
        let mut child = 0;
        let status = unsafe {
            begin(
                self.table.handle,
                request.as_ptr(),
                request.len(),
                &raw mut child,
            )
        };
        if status != OK {
            self.begin_failed(status, child);
            return Err(protocol(status));
        }
        if child == 0 {
            self.failed(INVALID_ARGUMENT);
            return Err(protocol(INVALID_ARGUMENT));
        }
        self.retrieve(child)
    }
    fn registration(
        self: &Arc<Self>,
        mutation: &crate::definition::RegistrationMutation,
    ) -> crate::Result<Vec<u8>> {
        let _admitted = self.admit(CAP_ACTIVATION_REGISTRATIONS)?;
        mutation
            .validate()
            .map_err(|_| protocol(INVALID_ARGUMENT))?;
        let input = encode_bounded(&mutation).map_err(protocol)?;
        let mut child = 0;
        let status = unsafe {
            (self.table.registration_begin)(
                self.table.handle,
                input.as_ptr(),
                input.len(),
                &raw mut child,
            )
        };
        if status != OK {
            self.begin_failed(status, child);
            return Err(protocol(status));
        }
        if child == 0 {
            self.failed(INVALID_ARGUMENT);
            return Err(protocol(INVALID_ARGUMENT));
        }
        self.retrieve(child)
    }
    fn invoke(self: &Arc<Self>, request: &HostServiceRequest<'_>) -> crate::Result<Vec<u8>> {
        let _admitted = self.admit(CAP_HOST_SERVICES)?;
        if [
            request.capability.len(),
            request.interface.len(),
            request.operation.len(),
            request.payload.len(),
        ]
        .into_iter()
        .any(|length| length > MAX_BUFFER_BYTES)
        {
            return Err(protocol(INVALID_ARGUMENT));
        }
        let request = NativeHostRequest {
            size: u32::try_from(std::mem::size_of::<NativeHostRequest>())
                .map_err(|_| protocol(INVALID_ARGUMENT))?,
            kind: match request.kind {
                crate::ServiceKind::Query => 0,
                crate::ServiceKind::Command => 1,
                crate::ServiceKind::Event => 2,
            },
            capability: NativeSlice::new(request.capability.as_bytes()),
            interface: NativeSlice::new(request.interface.as_bytes()),
            operation: NativeSlice::new(request.operation.as_bytes()),
            payload: NativeSlice::new(request.payload),
        };
        let mut child = 0;
        let status = unsafe {
            (self.table.host_begin)(self.table.handle, &raw const request, &raw mut child)
        };
        if self.table.version == PORTS_VERSION_V3 && status == super::CANCELLED && child == 0 {
            // The host explicitly rejected admission before effects and retained
            // no child. Return the cancellation error to the author, without
            // poisoning the authoritative report it may subsequently produce.
            return Err(PluginError::InvocationCancelled);
        }
        if status != OK {
            self.begin_failed(status, child);
            return Err(protocol(status));
        }
        if child == 0 {
            self.failed(INVALID_ARGUMENT);
            return Err(protocol(INVALID_ARGUMENT));
        }
        self.retrieve(child)
    }
    fn retrieve(&self, child: u64) -> crate::Result<Vec<u8>> {
        let result = (|| {
            let mut length = 0;
            let status = unsafe {
                (self.table.host_read)(
                    self.table.handle,
                    child,
                    std::ptr::null_mut(),
                    0,
                    &raw mut length,
                )
            };
            if status != BUFFER_TOO_SMALL && status != OK {
                return Err(status);
            }
            if length > MAX_BUFFER_BYTES {
                return Err(INVALID_ARGUMENT);
            }
            let mut bytes = vec![0; length];
            let mut written = 0;
            let status = unsafe {
                (self.table.host_read)(
                    self.table.handle,
                    child,
                    bytes.as_mut_ptr(),
                    bytes.len(),
                    &raw mut written,
                )
            };
            if status != OK || written > bytes.len() {
                return Err(if status == OK {
                    INVALID_ARGUMENT
                } else {
                    status
                });
            }
            bytes.truncate(written);
            Ok(bytes)
        })();
        let status = unsafe { (self.table.host_ack)(self.table.handle, child) };
        if status != OK {
            self.failed(status);
            return Err(protocol(status));
        }
        if let Err(status) = &result {
            if self.table.version == PORTS_VERSION_V3 {
                self.child_failed(*status);
            } else {
                // Preserve v2's aggregate retrieval-failure status.
                self.failed(FAILED);
            }
        }
        result.map_err(protocol)
    }
}
/// Captured sink clones become closed after terminal producer return; they never
/// dereference a host handle after release. Serial event admission preserves
/// producer ordering; concurrent/reentrant emission receives explicit BUSY.
pub struct NativeEventSink {
    pub ports: Arc<InvocationPorts>,
    pub invocation_id: crate::PluginInvocationId,
}
impl ServiceEventSink for NativeEventSink {
    fn emit_event(&self, event: ServiceEvent) -> crate::Result<()> {
        if event.invocation_id != self.invocation_id {
            self.ports.failed(INVALID_ARGUMENT);
            return Err(protocol(INVALID_ARGUMENT));
        }
        let _admitted = self.ports.admit(CAP_STREAM_EVENTS)?;
        let _ordered = self.ports.events.try_lock().map_err(|_| {
            self.ports.failed(BUSY);
            protocol(BUSY)
        })?;
        let bytes = encode_bounded(&event).map_err(|status| {
            self.ports.failed(status);
            protocol(status)
        })?;
        let status = unsafe {
            (self.ports.table.emit_event)(self.ports.table.handle, bytes.as_ptr(), bytes.len())
        };
        if status != OK {
            self.ports.failed(status);
            return Err(protocol(status));
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests;

impl Drop for InvocationPorts {
    fn drop(&mut self) {
        // Start rejection must release a successful retain as well. Normal
        // terminal paths already drained; admission guards retain self if busy.
        let _ = self.close_and_drain();
    }
}
