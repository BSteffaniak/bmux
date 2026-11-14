//! Independently owned retained host operations. No product identities live here.
//!
//! The trusted host owns deduplication, reservations and recovery. Dropping an
//! observer never acknowledges an outcome or cancels a producer. Callbacks must
//! be bounded, nonblocking, thread-safe and panic-contained. Host registry pins
//! retain execution, image and callback code through uncertain cleanup.

use super::{
    AbiStatus, BUFFER_TOO_SMALL, BUSY, INVALID_ARGUMENT, LIMIT_EXCEEDED, MAX_BUFFER_BYTES, OK,
    PENDING, UNSUPPORTED,
};
use crate::{HostServiceRequest, PluginError};
use std::sync::{Arc, Mutex};

fn slice(bytes: &[u8]) -> crate::Result<NativeRetainedSliceV1> {
    Ok(NativeRetainedSliceV1 {
        pointer: bytes.as_ptr(),
        length: u32::try_from(bytes.len()).map_err(|_| error(LIMIT_EXCEEDED))?,
    })
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NativeRetainedSliceV1 {
    pub pointer: *const u8,
    pub length: u32,
}
/// Generic routing metadata; bytes do not carry caller/owner authority.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NativeRetainedRequestV1 {
    pub size: u32,
    pub version: u32,
    /// Query=0, command=1, event=2; other values reject.
    pub kind: u32,
    pub capability: NativeRetainedSliceV1,
    pub interface: NativeRetainedSliceV1,
    pub operation: NativeRetainedSliceV1,
    pub payload: NativeRetainedSliceV1,
}

pub const RETAINED_HOST_VERSION: u32 = 1;
pub const RETAINED_HOST_CONTROL_CANCEL: u32 = 1;
pub const RETAINED_HOST_CONTROL_CLOSE: u32 = 2;
pub const RETAINED_HOST_CONTROL_JOIN: u32 = 3;
pub const RETAINED_HOST_COMPLETED: u32 = 1;
pub const RETAINED_HOST_REJECTED: u32 = 2;
pub const RETAINED_HOST_EFFECTS_UNRESOLVED: u32 = 3;
pub const RETAINED_HOST_RESPONSE_LIMIT: u32 = 4;
/// Admissions closed before dispatch; never inferred from an expired observer.
pub const RETAINED_HOST_CLOSED: u32 = 5;
/// Wire record: version u32 LE, outcome u32 LE, `observed_bytes` u64 LE, then success payload.
pub const RETAINED_HOST_RECORD_HEADER_BYTES: u32 = 16;
pub const RETAINED_HOST_MAX_CALLBACKS: u32 = 32;

/// Includes the entire terminal record, not just its successful payload.
/// One begin consumes exactly one operation slot in v1 (`operation_slots` == 1).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct NativeRetainedReservationV1 {
    pub size: u32,
    pub version: u32,
    pub max_outcome_bytes: u64,
    pub operation_slots: u32,
    pub reserved: u32,
}
impl NativeRetainedReservationV1 {
    /// # Errors
    /// Rejects unsupported representations or capacity outside transport bounds.
    pub fn validate(&self) -> Result<(), AbiStatus> {
        if self.version != RETAINED_HOST_VERSION
            || self.size as usize != std::mem::size_of::<Self>()
        {
            return Err(UNSUPPORTED);
        }
        if self.reserved != 0
            || self.operation_slots != 1
            || self.max_outcome_bytes < u64::from(RETAINED_HOST_RECORD_HEADER_BYTES)
            || self.max_outcome_bytes > MAX_BUFFER_BYTES as u64
        {
            return Err(LIMIT_EXCEEDED);
        }
        Ok(())
    }
}

/// Open transfers exactly one owned reference on OK, none on any other status.
///
/// The version argument selects the exact writable output allocation: v1 writes
/// precisely `NativeRetainedHostPortV1`, never a larger future table. Output is
/// uninitialized on entry and must be fully initialized on OK. The host resolves
/// an already bound owner; no request parameters create or select a session.
pub type NativeRetainedHostOpenV1 =
    unsafe extern "C" fn(u64, u16, *mut NativeRetainedHostPortV1) -> AbiStatus;

/// Independently retained asynchronous host operation transport.
///
/// Begin returns OK + nonzero operation only after reservation and recoverable
/// request-ID publication, before effects.
/// An identical request ID returns the same operation; conflicting reuse rejects.
/// IDs/tombstones survive ack until owner release. Read is non-consuming, returns
/// PENDING or stable terminal bytes + nonzero receipt; `BUFFER_TOO_SMALL` reports
/// required size without consuming. All lengths are bounded fixed-width fields.
/// Control operation 0 targets owner, otherwise its exact child; cancel signals,
/// close rejects new admission, join returns PENDING until actual completion.
/// Ack accepts only the exact terminal receipt. Release returns BUSY without
/// consuming a reference until closed, joined and all outcomes acknowledged.
/// Failed/uncertain release retains the host registry cleanup pin. No callback
/// may block waiting for a producer or keep a runtime lock across producer awaits.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NativeRetainedHostPortV1 {
    pub size: u32,
    pub version: u32,
    pub handle: u64,
    pub retain: unsafe extern "C" fn(u64) -> AbiStatus,
    pub release: unsafe extern "C" fn(u64) -> AbiStatus,
    pub begin: unsafe extern "C" fn(
        u64,
        u64,
        *const NativeRetainedRequestV1,
        *const NativeRetainedReservationV1,
        *mut u64,
    ) -> AbiStatus,
    pub read: unsafe extern "C" fn(u64, u64, *mut u8, u32, *mut u32, *mut u64) -> AbiStatus,
    pub control: unsafe extern "C" fn(u64, u64, u32) -> AbiStatus,
    pub ack: unsafe extern "C" fn(u64, u64, u64) -> AbiStatus,
}

/// Explicit recovery table version; the v1 prefix layout and semantics are frozen.
pub const RETAINED_HOST_LOOKUP_VERSION: u32 = 2;
pub type NativeRetainedHostLookupV2 = unsafe extern "C" fn(
    u64,
    u64,
    *const NativeRetainedRequestV1,
    *const NativeRetainedReservationV1,
    *mut u64,
) -> AbiStatus;
/// Non-consuming owner-local recovery lookup.
///
/// OK returns an existing nonzero operation; `NOT_FOUND` returns zero and never
/// authorizes replay. Full original facts and
/// reservation must match. Post-close lookup is supported; ack tombstones remain.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NativeRetainedHostPortV2 {
    pub base: NativeRetainedHostPortV1,
    pub lookup: NativeRetainedHostLookupV2,
}
/// Version 2 selects exactly the v2 writable allocation.
///
/// One reference on OK, none on failure. No downgrade after a selected v2 open fails.
pub type NativeRetainedHostOpenV2 =
    unsafe extern "C" fn(u64, u16, *mut NativeRetainedHostPortV2) -> AbiStatus;

/// Closed transport outcome taxonomy. Failure records contain no arbitrary text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetainedHostOutcome {
    Completed(Vec<u8>),
    RejectedBeforeEffects,
    EffectsUnresolved { observed_bytes: u64 },
    ResponseLimitExceeded { observed_bytes: u64 },
    Closed,
}
impl RetainedHostOutcome {
    /// Stable v1 encoding, independent of Rust/serde enum layout.
    /// # Errors
    /// Rejects a record exceeding the global transport bound.
    pub fn encode(&self) -> Result<Vec<u8>, AbiStatus> {
        let (kind, observed_bytes, payload): (u32, u64, &[u8]) = match self {
            Self::Completed(bytes) => (RETAINED_HOST_COMPLETED, bytes.len() as u64, bytes),
            Self::RejectedBeforeEffects => (RETAINED_HOST_REJECTED, 0, &[]),
            Self::EffectsUnresolved { observed_bytes } => {
                (RETAINED_HOST_EFFECTS_UNRESOLVED, *observed_bytes, &[])
            }
            Self::ResponseLimitExceeded { observed_bytes } => {
                (RETAINED_HOST_RESPONSE_LIMIT, *observed_bytes, &[])
            }
            Self::Closed => (RETAINED_HOST_CLOSED, 0, &[]),
        };
        if payload.len() > MAX_BUFFER_BYTES - RETAINED_HOST_RECORD_HEADER_BYTES as usize {
            return Err(LIMIT_EXCEEDED);
        }
        let mut bytes = Vec::with_capacity(16 + payload.len());
        bytes.extend_from_slice(&RETAINED_HOST_VERSION.to_le_bytes());
        bytes.extend_from_slice(&kind.to_le_bytes());
        bytes.extend_from_slice(&observed_bytes.to_le_bytes());
        bytes.extend_from_slice(payload);
        Ok(bytes)
    }
    /// # Errors
    /// Rejects unknown versions/kinds, malformed headers and failure payloads.
    pub fn decode(bytes: &[u8]) -> Result<Self, AbiStatus> {
        if bytes.len() < 16 || bytes.len() > MAX_BUFFER_BYTES {
            return Err(INVALID_ARGUMENT);
        }
        let word = |offset| {
            u32::from_le_bytes([
                bytes[offset],
                bytes[offset + 1],
                bytes[offset + 2],
                bytes[offset + 3],
            ])
        };
        if word(0) != RETAINED_HOST_VERSION {
            return Err(UNSUPPORTED);
        }
        let observed_bytes = u64::from(word(8)) | (u64::from(word(12)) << 32);
        if word(4) == RETAINED_HOST_COMPLETED {
            if observed_bytes != (bytes.len() - 16) as u64 {
                return Err(INVALID_ARGUMENT);
            }
            return Ok(Self::Completed(bytes[16..].to_vec()));
        }
        if bytes.len() != 16 {
            return Err(INVALID_ARGUMENT);
        }
        match word(4) {
            RETAINED_HOST_REJECTED if observed_bytes == 0 => Ok(Self::RejectedBeforeEffects),
            RETAINED_HOST_EFFECTS_UNRESOLVED => Ok(Self::EffectsUnresolved { observed_bytes }),
            RETAINED_HOST_RESPONSE_LIMIT => Ok(Self::ResponseLimitExceeded { observed_bytes }),
            RETAINED_HOST_CLOSED if observed_bytes == 0 => Ok(Self::Closed),
            RETAINED_HOST_REJECTED | RETAINED_HOST_CLOSED => Err(INVALID_ARGUMENT),
            _ => Err(UNSUPPORTED),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetainedHostObservation {
    Pending,
    Terminal {
        receipt: u64,
        outcome: RetainedHostOutcome,
    },
}

/// Static hosts implement this same mechanism against their actual owner ledger.
///
/// Methods only admit/control/observe; producers execute asynchronously elsewhere.
/// Drop of the implementation must preserve host-owned cleanup responsibility.
pub trait RetainedHostDispatch: Send + Sync {
    /// # Errors
    /// Rejects invalid/reused identities, unavailable authority or exhausted capacity.
    fn begin(
        &self,
        request_id: u64,
        request: &HostServiceRequest<'_>,
        reservation: NativeRetainedReservationV1,
    ) -> crate::Result<u64>;
    /// Recover only existing owner-local work; never admit, reserve or dispatch.
    ///
    /// # Errors
    /// Unsupported by default. Rejects conflicting facts/reservation or authority.
    fn lookup(
        &self,
        _request_id: u64,
        _request: &HostServiceRequest<'_>,
        _reservation: NativeRetainedReservationV1,
    ) -> crate::Result<Option<u64>> {
        Err(PluginError::UnsupportedHostOperation {
            operation: "retained_host_lookup",
        })
    }
    /// # Errors
    /// Returns host observation failure without consuming or redispatching work.
    fn observe(&self, operation: u64, max_bytes: u64) -> crate::Result<RetainedHostObservation>;
    /// # Errors
    /// Rejects unknown controls or an operation outside this exact owner.
    fn control(&self, operation: u64, control: u32) -> crate::Result<bool>;
    /// # Errors
    /// Rejects any receipt not belonging to this exact terminal operation.
    fn acknowledge(&self, operation: u64, receipt: u64) -> crate::Result<()>;
    /// # Errors
    /// Cleanup errors retain authority. False means BUSY and consumes nothing.
    fn release(&self) -> crate::Result<bool>;
}

#[derive(Clone)]
pub struct RetainedHostPort(Arc<dyn RetainedHostDispatch>);
impl RetainedHostPort {
    #[must_use]
    pub fn new(dispatch: Arc<dyn RetainedHostDispatch>) -> Self {
        Self(dispatch)
    }
    /// Open only the actual retained capability bound to this invocation table.
    /// The independently owned result survives invocation retirement.
    /// # Safety
    /// Pointer and callback lifetimes must meet scoped-port acquisition rules.
    /// The source table must permit an independent retain/release pair; callers
    /// must not pass a borrowed owner whose release retires other references.
    /// # Errors
    /// Unsupported/short/unknown tables reject without fallback or host effects.
    pub unsafe fn open_native(pointer: *const super::NativeInvocationPorts) -> crate::Result<Self> {
        let ports = unsafe { super::ports::InvocationPorts::acquire(pointer) }.map_err(error)?;
        ports.open_retained_host()
    }
    /// Open explicitly negotiated recovery-v2, without legacy fallback.
    ///
    /// # Safety
    /// Same source-table ownership requirements as `Self::open_native`.
    /// # Errors
    /// Rejects unsupported negotiation and failed selected v2 opens.
    pub unsafe fn open_native_v2(
        pointer: *const super::NativeInvocationPorts,
    ) -> crate::Result<Self> {
        let ports = unsafe { super::ports::InvocationPorts::acquire(pointer) }.map_err(error)?;
        ports.open_retained_host_v2()
    }
    /// # Safety
    /// Transfers one independently pinned owned reference, as for v1.
    /// # Errors
    /// Rejects any size/version other than the exact recovery-v2 representation.
    pub unsafe fn from_native_owned_v2(table: NativeRetainedHostPortV2) -> crate::Result<Self> {
        if table.base.size as usize != std::mem::size_of::<NativeRetainedHostPortV2>()
            || table.base.version != RETAINED_HOST_LOOKUP_VERSION
            || table.base.handle == 0
        {
            return Err(error(UNSUPPORTED));
        }
        Ok(Self::new(Arc::new(NativeDispatch {
            lookup: Some(table.lookup),
            state: Mutex::new(NativeState {
                table: Some(table.base),
                admitted: 0,
                releasing: false,
            }),
        })))
    }
    /// # Safety
    /// Table transfers one live host reference, with valid callbacks/code pinned
    /// until successful release; host retains recovery authority on failed drop.
    /// # Errors
    /// Rejects unsupported table representations without invoking their callbacks.
    pub unsafe fn from_native_owned(table: NativeRetainedHostPortV1) -> crate::Result<Self> {
        if table.size as usize != std::mem::size_of::<NativeRetainedHostPortV1>()
            || table.version != RETAINED_HOST_VERSION
            || table.handle == 0
        {
            return Err(error(UNSUPPORTED));
        }
        Ok(Self::new(Arc::new(NativeDispatch {
            lookup: None,
            state: Mutex::new(NativeState {
                table: Some(table),
                admitted: 0,
                releasing: false,
            }),
        })))
    }
    /// # Errors
    /// Rejects invalid bounds/identity; host rejection never triggers fallback.
    pub fn begin(
        &self,
        request_id: u64,
        request: &HostServiceRequest<'_>,
        reservation: NativeRetainedReservationV1,
    ) -> crate::Result<RetainedHostOperation> {
        validate_request(request_id, request, reservation)?;
        let operation = self.0.begin(request_id, request, reservation)?;
        if operation == 0 {
            return Err(error(INVALID_ARGUMENT));
        }
        Ok(RetainedHostOperation {
            owner: self.clone(),
            operation,
            max_bytes: reservation.max_outcome_bytes,
        })
    }
    /// Recover existing work using its original identity, facts and bounds.
    /// None means unknown at observation time, never permission to retry begin.
    ///
    /// # Errors
    /// Rejects malformed/conflicting requests and unsupported recovery negotiation.
    pub fn lookup(
        &self,
        request_id: u64,
        request: &HostServiceRequest<'_>,
        reservation: NativeRetainedReservationV1,
    ) -> crate::Result<Option<RetainedHostOperation>> {
        validate_request(request_id, request, reservation)?;
        self.0
            .lookup(request_id, request, reservation)?
            .map(|operation| {
                if operation == 0 {
                    return Err(error(INVALID_ARGUMENT));
                }
                Ok(RetainedHostOperation {
                    owner: self.clone(),
                    operation,
                    max_bytes: reservation.max_outcome_bytes,
                })
            })
            .transpose()
    }
    /// # Errors
    /// Returns host control failure; false means pending, not settled.
    pub fn control(&self, control: u32) -> crate::Result<bool> {
        control_valid(control)?;
        self.0.control(0, control)
    }
    /// # Errors
    /// False means BUSY and releases nothing. No implicit close, join or ack.
    pub fn release(&self) -> crate::Result<bool> {
        self.0.release()
    }
}
#[derive(Clone)]
pub struct RetainedHostOperation {
    owner: RetainedHostPort,
    operation: u64,
    max_bytes: u64,
}
impl RetainedHostOperation {
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.operation
    }
    /// # Errors
    /// Returns observation/protocol failure, never retries begin.
    pub fn observe(&self) -> crate::Result<RetainedHostObservation> {
        let observed = self.owner.0.observe(self.operation, self.max_bytes)?;
        if let RetainedHostObservation::Terminal { receipt, outcome } = &observed {
            if *receipt == 0 {
                return Err(error(INVALID_ARGUMENT));
            }
            if outcome.encode().map_err(error)?.len() as u64 > self.max_bytes {
                return Err(error(LIMIT_EXCEEDED));
            }
        }
        Ok(observed)
    }
    /// # Errors
    /// Returns host control failure; cancellation is not join.
    pub fn control(&self, control: u32) -> crate::Result<bool> {
        control_valid(control)?;
        self.owner.0.control(self.operation, control)
    }
    /// # Errors
    /// Host validates the exact retained terminal receipt before consuming it.
    pub fn acknowledge(&self, receipt: u64) -> crate::Result<()> {
        if receipt == 0 {
            return Err(error(INVALID_ARGUMENT));
        }
        self.owner.0.acknowledge(self.operation, receipt)
    }
}
fn validate_request(
    request_id: u64,
    request: &HostServiceRequest<'_>,
    reservation: NativeRetainedReservationV1,
) -> crate::Result<()> {
    reservation.validate().map_err(error)?;
    if request_id == 0 {
        return Err(error(INVALID_ARGUMENT));
    }
    if [
        request.capability.len(),
        request.interface.len(),
        request.operation.len(),
        request.payload.len(),
    ]
    .into_iter()
    .any(|length| length > MAX_BUFFER_BYTES)
    {
        return Err(error(LIMIT_EXCEEDED));
    }
    Ok(())
}
fn error(status: AbiStatus) -> PluginError {
    PluginError::ServiceProtocol {
        details: format!("retained host status {status}"),
    }
}
fn control_valid(control: u32) -> crate::Result<()> {
    if matches!(
        control,
        RETAINED_HOST_CONTROL_CANCEL | RETAINED_HOST_CONTROL_CLOSE | RETAINED_HOST_CONTROL_JOIN
    ) {
        Ok(())
    } else {
        Err(error(UNSUPPORTED))
    }
}
struct NativeState {
    table: Option<NativeRetainedHostPortV1>,
    admitted: u32,
    releasing: bool,
}
struct NativeDispatch {
    lookup: Option<NativeRetainedHostLookupV2>,
    state: Mutex<NativeState>,
}
impl NativeDispatch {
    fn with_table<T>(
        &self,
        f: impl FnOnce(NativeRetainedHostPortV1) -> crate::Result<T>,
    ) -> crate::Result<T> {
        let table = {
            let mut state = self.state.lock().map_err(|_| error(super::PANICKED))?;
            if state.releasing || state.admitted >= RETAINED_HOST_MAX_CALLBACKS {
                return Err(error(BUSY));
            }
            let table = state.table.ok_or_else(|| error(INVALID_ARGUMENT))?;
            state.admitted += 1;
            table
        };
        // No adapter lock crosses a callback. The host owns producer execution.
        let result = f(table);
        let mut state = self.state.lock().map_err(|_| error(super::PANICKED))?;
        state.admitted -= 1;
        drop(state);
        result
    }
}
fn native_request(request: &HostServiceRequest<'_>) -> crate::Result<NativeRetainedRequestV1> {
    Ok(NativeRetainedRequestV1 {
        size: u32::try_from(std::mem::size_of::<NativeRetainedRequestV1>())
            .map_err(|_| error(INVALID_ARGUMENT))?,
        version: RETAINED_HOST_VERSION,
        kind: match request.kind {
            crate::ServiceKind::Query => 0,
            crate::ServiceKind::Command => 1,
            crate::ServiceKind::Event => 2,
        },
        capability: slice(request.capability.as_bytes())?,
        interface: slice(request.interface.as_bytes())?,
        operation: slice(request.operation.as_bytes())?,
        payload: slice(request.payload)?,
    })
}
impl RetainedHostDispatch for NativeDispatch {
    fn begin(
        &self,
        request_id: u64,
        request: &HostServiceRequest<'_>,
        reservation: NativeRetainedReservationV1,
    ) -> crate::Result<u64> {
        self.with_table(|table| {
            let request = native_request(request)?;
            let mut operation = 0;
            let status = unsafe {
                (table.begin)(
                    table.handle,
                    request_id,
                    &raw const request,
                    &raw const reservation,
                    &raw mut operation,
                )
            };
            if status == OK && operation != 0 {
                Ok(operation)
            } else {
                Err(error(if status == OK {
                    INVALID_ARGUMENT
                } else {
                    status
                }))
            }
        })
    }
    fn lookup(
        &self,
        request_id: u64,
        request: &HostServiceRequest<'_>,
        reservation: NativeRetainedReservationV1,
    ) -> crate::Result<Option<u64>> {
        let lookup = self.lookup.ok_or(PluginError::UnsupportedHostOperation {
            operation: "retained_host_lookup",
        })?;
        self.with_table(|table| {
            let request = native_request(request)?;
            let mut operation = 0;
            let status = unsafe {
                lookup(
                    table.handle,
                    request_id,
                    &raw const request,
                    &raw const reservation,
                    &raw mut operation,
                )
            };
            match (status, operation) {
                (OK, id) if id != 0 => Ok(Some(id)),
                (super::NOT_FOUND, 0) => Ok(None),
                (OK | super::NOT_FOUND, _) => Err(error(INVALID_ARGUMENT)),
                (status, _) => Err(error(status)),
            }
        })
    }
    fn observe(&self, operation: u64, max_bytes: u64) -> crate::Result<RetainedHostObservation> {
        self.with_table(|table| {
            let mut length = 0;
            let mut receipt = 0;
            let status = unsafe {
                (table.read)(
                    table.handle,
                    operation,
                    std::ptr::null_mut(),
                    0,
                    &raw mut length,
                    &raw mut receipt,
                )
            };
            if status == PENDING {
                return Ok(RetainedHostObservation::Pending);
            }
            if status != BUFFER_TOO_SMALL
                || length < RETAINED_HOST_RECORD_HEADER_BYTES
                || u64::from(length) > max_bytes
            {
                return Err(error(if status == BUFFER_TOO_SMALL {
                    LIMIT_EXCEEDED
                } else if status == OK {
                    INVALID_ARGUMENT
                } else {
                    status
                }));
            }
            let mut bytes = vec![0; length as usize];
            let capacity = length;
            let expected_receipt = receipt;
            let status = unsafe {
                (table.read)(
                    table.handle,
                    operation,
                    bytes.as_mut_ptr(),
                    capacity,
                    &raw mut length,
                    &raw mut receipt,
                )
            };
            if status != OK || length != capacity || receipt == 0 || receipt != expected_receipt {
                return Err(error(if status == OK {
                    INVALID_ARGUMENT
                } else {
                    status
                }));
            }
            let outcome = RetainedHostOutcome::decode(&bytes).map_err(error)?;
            Ok(RetainedHostObservation::Terminal { receipt, outcome })
        })
    }
    fn control(&self, operation: u64, control: u32) -> crate::Result<bool> {
        self.with_table(|table| {
            match unsafe { (table.control)(table.handle, operation, control) } {
                OK => Ok(true),
                PENDING => Ok(false),
                status => Err(error(status)),
            }
        })
    }
    fn acknowledge(&self, operation: u64, receipt: u64) -> crate::Result<()> {
        self.with_table(
            |table| match unsafe { (table.ack)(table.handle, operation, receipt) } {
                OK => Ok(()),
                status => Err(error(status)),
            },
        )
    }
    fn release(&self) -> crate::Result<bool> {
        let table = {
            let mut state = self.state.lock().map_err(|_| error(super::PANICKED))?;
            if state.releasing || state.admitted != 0 {
                return Ok(false);
            }
            let Some(table) = state.table else {
                return Ok(true);
            };
            state.releasing = true;
            table
        };
        let status = unsafe { (table.release)(table.handle) };
        let mut state = self.state.lock().map_err(|_| error(super::PANICKED))?;
        if status == OK {
            state.table = None;
        }
        state.releasing = false;
        drop(state);
        match status {
            OK => Ok(true),
            BUSY => Ok(false),
            status => Err(error(status)),
        }
    }
}
impl Drop for NativeDispatch {
    fn drop(&mut self) {
        // BUSY/error leaves the authoritative registry pin intact. Never infer
        // settlement, cancel, consume an outcome, or unload callback code here.
        let _ = self.release();
    }
}

#[cfg(test)]
mod tests;
