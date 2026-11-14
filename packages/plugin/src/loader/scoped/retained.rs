//! Independent native transport ownership; product outcomes stay in the bound dispatch.
#[cfg(test)]
#[path = "retained_tests.rs"]
mod tests;
use super::{Arc, LoadedExecution, Mutex, Result, failure};
use bmux_plugin_sdk::abi::retained::{
    NativeRetainedHostPortV1, NativeRetainedRequestV1, NativeRetainedReservationV1,
    NativeRetainedSliceV1, RetainedHostDispatch, RetainedHostObservation,
};
use bmux_plugin_sdk::abi::{self, AbiStatus};
use std::{
    collections::BTreeMap,
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

static NEXT: AtomicU64 = AtomicU64::new(1);
static OWNERS: OnceLock<Mutex<BTreeMap<u64, Arc<Owner>>>> = OnceLock::new();
fn owners() -> &'static Mutex<BTreeMap<u64, Arc<Owner>>> {
    OWNERS.get_or_init(Mutex::default)
}
struct Gate {
    references: usize,
    active: usize,
    releasing: bool,
}
struct Owner {
    execution: Arc<LoadedExecution>,
    dispatch: Arc<dyn RetainedHostDispatch>,
    gate: Mutex<Gate>,
}
fn lookup(handle: u64) -> Result<Arc<Owner>> {
    owners()
        .lock()
        .map_err(|_| failure("retained registry poisoned"))?
        .get(&handle)
        .cloned()
        .ok_or_else(|| failure("retained owner unavailable"))
}
struct Call(Arc<Owner>);
impl Drop for Call {
    fn drop(&mut self) {
        if let Ok(mut gate) = self.0.gate.lock() {
            gate.active -= 1;
        }
    }
}
fn enter(handle: u64) -> Result<Call> {
    let owner = lookup(handle)?;
    let mut gate = owner
        .gate
        .lock()
        .map_err(|_| failure("retained gate poisoned"))?;
    if gate.releasing || gate.references == 0 || gate.active >= 1024 {
        return Err(failure("retained owner releasing"));
    }
    gate.active = gate
        .active
        .checked_add(1)
        .ok_or_else(|| failure("retained callback limit"))?;
    drop(gate);
    Ok(Call(owner))
}
fn guard(work: impl FnOnce() -> Result<AbiStatus>) -> AbiStatus {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)) {
        Ok(Ok(status)) => status,
        Ok(Err(_)) => abi::FAILED,
        Err(_) => abi::PANICKED,
    }
}
pub(super) fn open(
    execution: Arc<LoadedExecution>,
    dispatch: Arc<dyn RetainedHostDispatch>,
) -> Result<NativeRetainedHostPortV1> {
    let mut registry = owners()
        .lock()
        .map_err(|_| failure("retained registry poisoned"))?;
    if let Some((&id, owner)) = registry.iter().find(|(_, owner)| {
        Arc::ptr_eq(&owner.execution, &execution) && Arc::ptr_eq(&owner.dispatch, &dispatch)
    }) {
        let mut gate = owner
            .gate
            .lock()
            .map_err(|_| failure("retained gate poisoned"))?;
        if gate.releasing {
            return Err(failure("retained owner releasing"));
        }
        gate.references = gate
            .references
            .checked_add(1)
            .ok_or_else(|| failure("retained reference limit"))?;
        drop(gate);
        drop(registry);
        return table(id);
    }
    if registry.len() >= 4096 {
        return Err(failure("retained owner capacity exhausted"));
    }
    // compare_exchange works on both pinned Rust 1.98 and 1.99.
    let mut id = NEXT.load(Ordering::Acquire);
    loop {
        let next = id
            .checked_add(1)
            .ok_or_else(|| failure("retained identity exhausted"))?;
        match NEXT.compare_exchange(id, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => break,
            Err(current) => id = current,
        }
    }
    let output = table(id)?;
    registry.insert(
        id,
        Arc::new(Owner {
            execution,
            dispatch,
            gate: Mutex::new(Gate {
                references: 1,
                active: 0,
                releasing: false,
            }),
        }),
    );
    drop(registry);
    Ok(output)
}
fn table(handle: u64) -> Result<NativeRetainedHostPortV1> {
    Ok(NativeRetainedHostPortV1 {
        size: u32::try_from(std::mem::size_of::<NativeRetainedHostPortV1>())
            .map_err(|_| failure("retained table size"))?,
        version: 1,
        handle,
        retain,
        release,
        begin,
        read,
        control,
        ack,
    })
}
pub(super) fn retry_release(execution: &LoadedExecution) -> Result<bool> {
    let handle = {
        let registry = owners()
            .lock()
            .map_err(|_| failure("retained registry poisoned"))?;
        let Some((&handle, owner)) = registry
            .iter()
            .find(|(_, owner)| std::ptr::eq(Arc::as_ptr(&owner.execution), execution))
        else {
            return Ok(true);
        };
        if owner
            .gate
            .lock()
            .map_err(|_| failure("retained gate poisoned"))?
            .references
            != 1
        {
            return Err(failure("retained observers still own multiple references"));
        }
        drop(registry);
        handle
    };
    // Retry the unresolved final release, never begin or producer work.
    match release_inner(handle, true) {
        abi::OK => Ok(true),
        abi::BUSY => Ok(false),
        status => Err(failure(format!(
            "retained recovery release status {status}"
        ))),
    }
}

pub(super) fn has_owner(execution: &LoadedExecution) -> Result<bool> {
    Ok(owners()
        .lock()
        .map_err(|_| failure("retained registry poisoned"))?
        .values()
        .any(|owner| std::ptr::eq(Arc::as_ptr(&owner.execution), execution)))
}
unsafe extern "C" fn retain(handle: u64) -> AbiStatus {
    guard(|| {
        let owner = lookup(handle)?;
        let mut gate = owner
            .gate
            .lock()
            .map_err(|_| failure("retained gate poisoned"))?;
        if gate.releasing {
            return Ok(abi::BUSY);
        }
        gate.references = gate
            .references
            .checked_add(1)
            .ok_or_else(|| failure("retained reference limit"))?;
        drop(gate);
        Ok(abi::OK)
    })
}
unsafe extern "C" fn release(handle: u64) -> AbiStatus {
    release_inner(handle, false)
}
fn release_inner(handle: u64, final_only: bool) -> AbiStatus {
    guard(|| {
        let owner = lookup(handle)?;
        {
            let mut gate = owner
                .gate
                .lock()
                .map_err(|_| failure("retained gate poisoned"))?;
            if gate.releasing || gate.active != 0 {
                return Ok(abi::BUSY);
            }
            if gate.references > 1 {
                if final_only {
                    return Ok(abi::BUSY);
                }
                gate.references -= 1;
                return Ok(abi::OK);
            }
            gate.releasing = true;
        }
        // Panic/error/BUSY preserve the reference and recovery pin.
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| owner.dispatch.release()));
        let mut gate = owner
            .gate
            .lock()
            .map_err(|_| failure("retained gate poisoned"))?;
        match result {
            Ok(Ok(true)) => {
                gate.references = 0;
                drop(gate);
                let mut bound = owner
                    .execution
                    .retained_dispatch
                    .lock()
                    .map_err(|_| failure("retained dispatch poisoned"))?;
                if bound
                    .as_ref()
                    .is_some_and(|dispatch| Arc::ptr_eq(dispatch, &owner.dispatch))
                {
                    bound.take();
                }
                drop(bound);
                owners()
                    .lock()
                    .map_err(|_| failure("retained registry poisoned"))?
                    .remove(&handle);
                Ok(abi::OK)
            }
            other => {
                gate.releasing = false;
                drop(gate);
                match other {
                    Ok(Ok(false)) => Ok(abi::BUSY),
                    Ok(Err(error)) => Err(error),
                    Err(_) => Ok(abi::PANICKED),
                    _ => unreachable!(),
                }
            }
        }
    })
}
unsafe fn bytes<'a>(slice: NativeRetainedSliceV1) -> Result<&'a [u8]> {
    let length = usize::try_from(slice.length).map_err(|_| failure("retained length"))?;
    if length > abi::MAX_BUFFER_BYTES || (length != 0 && slice.pointer.is_null()) {
        return Err(failure("retained slice invalid"));
    }
    Ok(if length == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(slice.pointer, length) }
    })
}
pub(super) fn table_v2(
    mut base: NativeRetainedHostPortV1,
) -> bmux_plugin_sdk::abi::retained::NativeRetainedHostPortV2 {
    base.version = 2;
    base.size = u32::try_from(std::mem::size_of::<
        bmux_plugin_sdk::abi::retained::NativeRetainedHostPortV2,
    >())
    .expect("fixed ABI size");
    bmux_plugin_sdk::abi::retained::NativeRetainedHostPortV2 {
        base,
        lookup: recover,
    }
}

unsafe extern "C" fn recover(
    handle: u64,
    request_id: u64,
    request: *const NativeRetainedRequestV1,
    reservation: *const NativeRetainedReservationV1,
    operation: *mut u64,
) -> AbiStatus {
    unsafe { begin_or_lookup(handle, request_id, request, reservation, operation, true) }
}

unsafe extern "C" fn begin(
    handle: u64,
    request_id: u64,
    request: *const NativeRetainedRequestV1,
    reservation: *const NativeRetainedReservationV1,
    operation: *mut u64,
) -> AbiStatus {
    unsafe { begin_or_lookup(handle, request_id, request, reservation, operation, false) }
}

unsafe fn begin_or_lookup(
    handle: u64,
    request_id: u64,
    request: *const NativeRetainedRequestV1,
    reservation: *const NativeRetainedReservationV1,
    operation: *mut u64,
    recovery: bool,
) -> AbiStatus {
    guard(|| {
        if operation.is_null() {
            return Ok(abi::INVALID_ARGUMENT);
        }
        unsafe {
            operation.write(0);
        }
        if request.is_null() || reservation.is_null() {
            return Ok(abi::INVALID_ARGUMENT);
        }
        // Inspect only the common header until the complete representation is proved.
        let header = request.cast::<u32>();
        let budget_header = reservation.cast::<u32>();
        if unsafe { header.read() } as usize != std::mem::size_of::<NativeRetainedRequestV1>()
            || unsafe { header.add(1).read() } != 1
            || unsafe { budget_header.read() } as usize
                != std::mem::size_of::<NativeRetainedReservationV1>()
            || unsafe { budget_header.add(1).read() } != 1
        {
            return Ok(abi::UNSUPPORTED);
        }
        let request = unsafe { request.read() };
        let reservation = unsafe { reservation.read() };
        if let Err(status) = reservation.validate() {
            return Ok(status);
        }
        if request_id == 0 {
            return Ok(abi::INVALID_ARGUMENT);
        }
        let kind = match request.kind {
            0 => bmux_plugin_sdk::ServiceKind::Query,
            1 => bmux_plugin_sdk::ServiceKind::Command,
            2 => bmux_plugin_sdk::ServiceKind::Event,
            _ => return Ok(abi::INVALID_ARGUMENT),
        };
        let parsed = (|| {
            Ok::<_, bmux_plugin_sdk::PluginError>((
                std::str::from_utf8(unsafe { bytes(request.capability)? })
                    .map_err(|e| failure(e.to_string()))?,
                std::str::from_utf8(unsafe { bytes(request.interface)? })
                    .map_err(|e| failure(e.to_string()))?,
                std::str::from_utf8(unsafe { bytes(request.operation)? })
                    .map_err(|e| failure(e.to_string()))?,
                unsafe { bytes(request.payload)? },
            ))
        })();
        let (capability, interface_id, name, payload) = match parsed {
            Ok(facts) => facts,
            Err(_) if recovery => return Ok(abi::INVALID_ARGUMENT),
            Err(error) => return Err(error),
        };
        if capability.is_empty() || interface_id.is_empty() || name.is_empty() {
            return Ok(abi::INVALID_ARGUMENT);
        }
        let call = enter(handle)?;
        let facts = bmux_plugin_sdk::HostServiceRequest {
            capability,
            kind,
            interface: interface_id,
            operation: name,
            payload,
        };
        let id = if recovery {
            match call.0.dispatch.lookup(request_id, &facts, reservation) {
                Ok(Some(id)) => id,
                Ok(None) => return Ok(abi::NOT_FOUND),
                Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation { .. }) => {
                    return Ok(abi::UNSUPPORTED);
                }
                Err(error) => return Err(error),
            }
        } else {
            call.0.dispatch.begin(request_id, &facts, reservation)?
        };
        if id == 0 {
            return Err(failure("retained dispatch returned zero identity"));
        }
        unsafe {
            operation.write(id);
        }
        Ok(abi::OK)
    })
}
unsafe extern "C" fn read(
    handle: u64,
    operation: u64,
    output: *mut u8,
    capacity: u32,
    length: *mut u32,
    receipt: *mut u64,
) -> AbiStatus {
    guard(|| {
        if length.is_null()
            || receipt.is_null()
            || capacity as usize > abi::MAX_BUFFER_BYTES
            || (capacity != 0 && output.is_null())
        {
            return Ok(abi::INVALID_ARGUMENT);
        }
        unsafe {
            length.write(0);
            receipt.write(0);
        }
        let call = enter(handle)?;
        match call
            .0
            .dispatch
            .observe(operation, abi::MAX_BUFFER_BYTES as u64)?
        {
            RetainedHostObservation::Pending => Ok(abi::PENDING),
            RetainedHostObservation::Terminal {
                receipt: token,
                outcome,
            } => {
                if token == 0 {
                    return Err(failure("retained terminal has no receipt"));
                }
                let bytes = match outcome.encode() {
                    Ok(bytes) => bytes,
                    Err(status) => return Ok(status),
                };
                unsafe {
                    length.write(
                        u32::try_from(bytes.len())
                            .map_err(|_| failure("retained result length"))?,
                    );
                    receipt.write(token);
                }
                if (capacity as usize) < bytes.len() {
                    return Ok(abi::BUFFER_TOO_SMALL);
                }
                unsafe {
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), output, bytes.len());
                }
                Ok(abi::OK)
            }
        }
    })
}
unsafe extern "C" fn control(handle: u64, operation: u64, command: u32) -> AbiStatus {
    guard(|| {
        if !matches!(command, 1..=3) {
            return Ok(abi::UNSUPPORTED);
        }
        let call = enter(handle)?;
        Ok(if call.0.dispatch.control(operation, command)? {
            abi::OK
        } else {
            abi::PENDING
        })
    })
}
unsafe extern "C" fn ack(handle: u64, operation: u64, receipt: u64) -> AbiStatus {
    guard(|| {
        if operation == 0 || receipt == 0 {
            return Ok(abi::INVALID_ARGUMENT);
        }
        let call = enter(handle)?;
        call.0.dispatch.acknowledge(operation, receipt)?;
        Ok(abi::OK)
    })
}
