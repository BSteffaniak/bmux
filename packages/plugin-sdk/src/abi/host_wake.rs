//! Explicitly negotiated native host wake contract v1 (`CAP_HOST_WAKE_V1`).
//!
//! A v3 descriptor advertising bit 2048 MUST be at least the size of
//! [`NativePluginDescriptorV3HostWakeV1`] and export a non-null `read_wake`.
//! `read_wake` has exactly `read`'s argument, status and output semantics and
//! additionally borrows a host [`NativeHostWakerV1`] table for that call only.
//!
//! The guest keeps the waker only by calling `retain` (one reference per kept
//! copy) and must eventually `release` every reference it retained. `wake`
//! signals that a later `read_wake` of the same operation may make progress; it
//! never executes author code or effects, may run on any thread, and may run
//! reentrantly during `read_wake`. Host callbacks are panic-contained and
//! report `PANICKED` instead of unwinding across the boundary.
//!
//! After `read_wake` returns `PENDING` or `BUSY`, the guest guarantees a later
//! `wake` through a waker passed to that call once progress is possible: the
//! author future woke, a terminal callback drain may now succeed, or a
//! concurrent poll released the operation. A host using `read_wake` therefore
//! never polls on a timer. A guest without the bit is a legacy guest: hosts
//! read it with `read` (no-op waker) and must keep periodic polling for it.

use super::{AbiStatus, INVALID_ARGUMENT, NativePluginDescriptorV3, OK, PANICKED};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    task::{RawWaker, RawWakerVTable, Waker},
};

pub const CAP_HOST_WAKE_V1: u64 = 2048;
pub const HOST_WAKE_VERSION: u32 = 1;

/// Host-owned wake handle. `data` is opaque host state, never an author value.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NativeHostWakerV1 {
    pub size: u32,
    pub version: u32,
    pub data: u64,
    pub retain: unsafe extern "C" fn(u64) -> AbiStatus,
    pub release: unsafe extern "C" fn(u64) -> AbiStatus,
    pub wake: unsafe extern "C" fn(u64) -> AbiStatus,
}

pub type NativeReadWakeV1 = unsafe extern "C" fn(
    u64,
    u64,
    *mut u8,
    usize,
    *mut usize,
    *const NativeHostWakerV1,
) -> AbiStatus;

/// Size-checked descriptor tail; the v3 prefix (and its size) remains frozen.
#[repr(C)]
pub struct NativePluginDescriptorV3HostWakeV1 {
    pub base: NativePluginDescriptorV3,
    pub read_wake: NativeReadWakeV1,
}

/// Descriptor size of a guest that advertises [`CAP_HOST_WAKE_V1`].
pub const NATIVE_DESCRIPTOR_SIZE_HOST_WAKE_V1: u32 = {
    let size = std::mem::size_of::<NativePluginDescriptorV3HostWakeV1>();
    assert!(size <= u32::MAX as usize);
    let bytes = size.to_le_bytes();
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
};

const HOST_WAKER_SIZE: u32 = {
    let size = std::mem::size_of::<NativeHostWakerV1>();
    assert!(size <= u32::MAX as usize);
    let bytes = size.to_le_bytes();
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
};

// ---- Host side --------------------------------------------------------------

unsafe extern "C" fn host_retain(data: u64) -> AbiStatus {
    catch_unwind(|| {
        // SAFETY: `data` came from `Arc::into_raw` and a reference is held by
        // the caller (the borrowed table or a previously retained copy).
        unsafe { Arc::increment_strong_count(data as *const Waker) };
        OK
    })
    .unwrap_or(PANICKED)
}
unsafe extern "C" fn host_release(data: u64) -> AbiStatus {
    catch_unwind(|| {
        // SAFETY: releases exactly one reference acquired by `host_retain`.
        unsafe { Arc::decrement_strong_count(data as *const Waker) };
        OK
    })
    .unwrap_or(PANICKED)
}
unsafe extern "C" fn host_wake(data: u64) -> AbiStatus {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the caller holds a reference for the duration of this call.
        unsafe { &*(data as *const Waker) }.wake_by_ref();
        OK
    }))
    .unwrap_or(PANICKED)
}

/// One host reference to a real observer waker, lent to a single `read_wake`.
/// Dropping it releases only the host's own reference; guest copies survive.
pub struct HostWaker {
    table: NativeHostWakerV1,
}
impl HostWaker {
    #[must_use]
    pub fn new(waker: &Waker) -> Self {
        let data = Arc::into_raw(Arc::new(waker.clone())) as u64;
        Self {
            table: NativeHostWakerV1 {
                size: HOST_WAKER_SIZE,
                version: HOST_WAKE_VERSION,
                data,
                retain: host_retain,
                release: host_release,
                wake: host_wake,
            },
        }
    }
    #[must_use]
    pub const fn as_ptr(&self) -> *const NativeHostWakerV1 {
        &raw const self.table
    }
}
impl Drop for HostWaker {
    fn drop(&mut self) {
        // SAFETY: the reference created by `new`.
        unsafe { Arc::decrement_strong_count(self.table.data as *const Waker) };
    }
}

// ---- Guest side -------------------------------------------------------------

struct Held {
    table: NativeHostWakerV1,
    retained: bool,
}
impl Held {
    fn retain(table: NativeHostWakerV1) -> Self {
        // A failed retain yields an inert copy: it never releases or wakes a
        // reference it does not own.
        let retained = unsafe { (table.retain)(table.data) } == OK;
        Self { table, retained }
    }
}
impl Drop for Held {
    fn drop(&mut self) {
        if self.retained {
            let _ = unsafe { (self.table.release)(self.table.data) };
        }
    }
}

const VTABLE: RawWakerVTable = RawWakerVTable::new(clone_raw, wake_raw, wake_by_ref_raw, drop_raw);
unsafe fn clone_raw(data: *const ()) -> RawWaker {
    let held = unsafe { &*data.cast::<Held>() };
    RawWaker::new(
        Box::into_raw(Box::new(Held::retain(held.table))).cast(),
        &VTABLE,
    )
}
unsafe fn wake_raw(data: *const ()) {
    unsafe {
        wake_by_ref_raw(data);
        drop_raw(data);
    }
}
unsafe fn wake_by_ref_raw(data: *const ()) {
    let held = unsafe { &*data.cast::<Held>() };
    if held.retained {
        let _ = unsafe { (held.table.wake)(held.table.data) };
    }
}
unsafe fn drop_raw(data: *const ()) {
    drop(unsafe { Box::from_raw(data.cast::<Held>().cast_mut()) });
}

/// Adopt a borrowed host waker table as a Rust waker that retains its own
/// host reference.
///
/// # Safety
/// `pointer` must be null or readable for its advertised size during this call.
pub(crate) unsafe fn guest_waker(pointer: *const NativeHostWakerV1) -> Result<Waker, AbiStatus> {
    if pointer.is_null() || !pointer.is_aligned() {
        return Err(INVALID_ARGUMENT);
    }
    let size = unsafe { std::ptr::addr_of!((*pointer).size).read() };
    let version = unsafe { std::ptr::addr_of!((*pointer).version).read() };
    if version != HOST_WAKE_VERSION || size < HOST_WAKER_SIZE {
        return Err(INVALID_ARGUMENT);
    }
    let table = unsafe { pointer.read() };
    if table.data == 0 {
        return Err(INVALID_ARGUMENT);
    }
    let held = Held::retain(table);
    if !held.retained {
        return Err(PANICKED);
    }
    // SAFETY: the vtable upholds the RawWaker contract for `Held`.
    Ok(unsafe { Waker::from_raw(RawWaker::new(Box::into_raw(Box::new(held)).cast(), &VTABLE)) })
}

/// Guest-local progress waiters: readers that returned `BUSY` or a pending
/// terminal drain. Any release of a concurrent poll, callback admission, entry
/// lease or cancellation subscription wakes them to retry; spurious wakes are
/// permitted, lost wakes are not (registration precedes the retried check).
static PROGRESS: Mutex<Vec<(u64, Waker)>> = Mutex::new(Vec::new());
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

pub(crate) struct ProgressRegistration(u64);
impl ProgressRegistration {
    pub(crate) fn new(waker: &Waker) -> Self {
        let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut waiters) = PROGRESS.lock() {
            waiters.push((token, waker.clone()));
        }
        Self(token)
    }
    /// The reader does not await guest progress (author future owns the wake,
    /// or the outcome is terminal): withdraw without waking.
    pub(crate) fn withdraw(&self) {
        if let Ok(mut waiters) = PROGRESS.lock() {
            waiters.retain(|(token, _)| *token != self.0);
        }
    }
    pub(crate) const fn token(&self) -> u64 {
        self.0
    }
}

pub(crate) fn notify_progress() {
    notify_progress_except(0);
}

pub(crate) fn notify_progress_except(own: u64) {
    let woken = PROGRESS.lock().map_or_else(
        |_| Vec::new(),
        |mut waiters| {
            let (keep, wake): (Vec<_>, Vec<_>) =
                waiters.drain(..).partition(|(token, _)| *token == own);
            *waiters = keep;
            wake
        },
    );
    for (_, waker) in woken {
        waker.wake();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct Count(AtomicUsize);
    impl std::task::Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn host_waker_round_trips_references_and_wakes() {
        let count = Arc::new(Count(AtomicUsize::new(0)));
        let observer = Waker::from(count.clone());
        let host = HostWaker::new(&observer);
        let guest = unsafe { guest_waker(host.as_ptr()) }.unwrap();
        let clone = guest.clone();
        drop(host);
        guest.wake_by_ref();
        clone.wake();
        guest.wake();
        assert_eq!(count.0.load(Ordering::SeqCst), 3);
        // Every guest and host reference was released: only ours remains.
        drop(observer);
        assert_eq!(Arc::strong_count(&count), 1);
    }

    #[test]
    fn malformed_host_waker_tables_are_rejected() {
        let count = Arc::new(Count(AtomicUsize::new(0)));
        let host = HostWaker::new(&Waker::from(count));
        let mut table = unsafe { host.as_ptr().read() };
        assert_eq!(
            unsafe { guest_waker(std::ptr::null()) }.err(),
            Some(INVALID_ARGUMENT)
        );
        table.version = 2;
        assert_eq!(
            unsafe { guest_waker(&raw const table) }.err(),
            Some(INVALID_ARGUMENT)
        );
        table.version = HOST_WAKE_VERSION;
        table.size = 8;
        assert_eq!(
            unsafe { guest_waker(&raw const table) }.err(),
            Some(INVALID_ARGUMENT)
        );
    }

    #[test]
    fn progress_notification_skips_own_registration() {
        // PROGRESS is process-global; other tests may notify concurrently, so
        // assert only facts that a concurrent notification cannot change.
        let other_count = Arc::new(Count(AtomicUsize::new(0)));
        let own = ProgressRegistration::new(&Waker::noop().clone());
        let _other = ProgressRegistration::new(&Waker::from(other_count.clone()));
        notify_progress_except(own.token());
        assert_eq!(other_count.0.load(Ordering::SeqCst), 1);
        own.withdraw();
        assert!(
            PROGRESS
                .lock()
                .unwrap()
                .iter()
                .all(|(token, _)| *token != own.token())
        );
    }

    #[test]
    fn descriptor_tail_preserves_v3_prefix() {
        assert_eq!(
            std::mem::offset_of!(NativePluginDescriptorV3HostWakeV1, read_wake),
            std::mem::size_of::<NativePluginDescriptorV3>()
        );
        assert_eq!(CAP_HOST_WAKE_V1 & super::super::SCOPED_CAPABILITIES, 0);
    }
}
