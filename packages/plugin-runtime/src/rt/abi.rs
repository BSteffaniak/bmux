//! `#[repr(C)]` host function table for dynamic (cdylib) plugins: `PlugRtV1`.
//!
//! A dynamic plugin never runs its own Tokio runtime. The host installs one
//! table per loaded library through the exported [`INSTALL_SYMBOL`] before any
//! instance is created, and every spawn, blocking job and timer goes through
//! it onto the host's single runtime.
//!
//! Ownership rules:
//! - Plugin futures cross as [`RtTask`] `{data, poll, drop}` and blocking work
//!   as [`RtJob`] `{data, run, drop}`. Ownership always transfers to the host,
//!   even when `spawn` refuses: the host then calls `drop` before returning.
//!   The host calls `drop` exactly once, always through the plugin's function,
//!   so memory is freed by the side that allocated it.
//! - [`RtWaker`] values are borrowed for one call unless cloned through their
//!   own vtable; each side frees only the boxes it allocated.
//! - Every plugin-side entry point catches panics and reports
//!   [`RT_PANICKED`]; nothing unwinds across the boundary.
//! - Libraries that ever received a table are never unloaded.
use std::{ffi::c_void, task::Waker};

/// Exported by every dynamic-mode plugin wrapper (`export_plugin_entrypoint!`).
pub const INSTALL_SYMBOL: &str = "bmux_plug_rt_install_v1";
pub const PLUG_RT_ABI_VERSION: u32 = 1;

pub type RtStatus = i32;
/// Completed (`poll`) or accepted (calls that admit work).
pub const RT_READY: RtStatus = 0;
pub const RT_PENDING: RtStatus = 1;
/// A plugin poll, drop, or blocking job panicked; the panic was contained.
pub const RT_PANICKED: RtStatus = 2;
/// The owning scope stopped admitting work.
pub const RT_CLOSED: RtStatus = 3;
pub const RT_INVALID: RtStatus = 4;
/// The library was not built in dynamic mode (`--cfg bmux_plugin_dylib`).
pub const RT_UNSUPPORTED: RtStatus = 5;
/// Size, version, or panic-strategy mismatch.
pub const RT_INCOMPATIBLE: RtStatus = 6;
/// The output buffer is too small; the required length was written and the
/// item is retained for the next call.
pub const RT_TOO_SMALL: RtStatus = 7;

/// Borrowed UTF-8 bytes, valid for one call.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RtStr {
    pub ptr: *const u8,
    pub len: usize,
}
impl RtStr {
    #[must_use]
    pub const fn new(value: &str) -> Self {
        Self {
            ptr: value.as_ptr(),
            len: value.len(),
        }
    }
    /// # Safety
    /// `ptr` must be readable for `len` bytes for the returned borrow.
    #[must_use]
    pub unsafe fn as_str<'a>(self) -> &'a str {
        if self.ptr.is_null() || self.len == 0 {
            return "";
        }
        let bytes = unsafe { std::slice::from_raw_parts(self.ptr, self.len) };
        std::str::from_utf8(bytes).unwrap_or("<invalid task name>")
    }
}

/// `extern "C"` waker shim. Functions belong to the side that created `data`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RtWaker {
    pub data: *const c_void,
    pub vtable: *const RtWakerVTable,
}
#[repr(C)]
pub struct RtWakerVTable {
    /// Returns an owned waker the receiver must eventually `wake` or `drop`.
    pub clone: unsafe extern "C" fn(*const c_void) -> RtWaker,
    /// Consumes an owned waker.
    pub wake: unsafe extern "C" fn(*const c_void),
    pub wake_by_ref: unsafe extern "C" fn(*const c_void),
    /// Releases an owned waker.
    pub drop: unsafe extern "C" fn(*const c_void),
}

/// Poll context passed into every cross-boundary poll.
///
/// `rust_waker` is non-null only when both images share a build fingerprint;
/// the receiver may then use the sender's `Waker` directly (zero allocation).
/// Otherwise the receiver must use the `waker` shim.
#[repr(C)]
pub struct RtCx {
    pub waker: RtWaker,
    pub rust_waker: *const Waker,
}

/// A plugin future. `poll` returns `RT_READY`, `RT_PENDING` or `RT_PANICKED`.
#[repr(C)]
pub struct RtTask {
    pub data: *mut c_void,
    pub poll: unsafe extern "C" fn(*mut c_void, *const RtCx) -> RtStatus,
    pub drop: unsafe extern "C" fn(*mut c_void),
}
// SAFETY: the plugin side only builds tasks from `Send` futures.
unsafe impl Send for RtTask {}

/// Plugin blocking work. `run` is called at most once, then `drop` exactly once.
#[repr(C)]
pub struct RtJob {
    pub data: *mut c_void,
    pub run: unsafe extern "C" fn(*mut c_void) -> RtStatus,
    pub drop: unsafe extern "C" fn(*mut c_void),
}
// SAFETY: the plugin side only builds jobs from `Send` closures.
unsafe impl Send for RtJob {}

/// Host function table. Fields past `size` must not be read.
#[repr(C)]
pub struct PlugRtV1 {
    pub size: u32,
    pub abi_version: u32,
    pub build_fingerprint: [u8; 32],
    /// Opaque host state passed back to every function.
    pub host: *const c_void,
    /// `scope` is the plugin instance handle that owns the task (0 = none).
    pub spawn: unsafe extern "C" fn(*const c_void, u64, RtTask, RtStr, *mut u64) -> RtStatus,
    pub task_abort: unsafe extern "C" fn(*const c_void, u64),
    pub spawn_blocking:
        unsafe extern "C" fn(*const c_void, u64, RtJob, RtStr, *mut u64) -> RtStatus,
    /// Nanoseconds on the host's monotonic runtime clock.
    pub now_ns: unsafe extern "C" fn(*const c_void) -> u64,
    pub timer_new: unsafe extern "C" fn(*const c_void, u64, *mut u64) -> RtStatus,
    pub timer_reset: unsafe extern "C" fn(*const c_void, u64, u64) -> RtStatus,
    pub timer_poll: unsafe extern "C" fn(*const c_void, u64, *const RtCx) -> RtStatus,
    pub timer_drop: unsafe extern "C" fn(*const c_void, u64),
    /// Serialized activation events (see `rt::events`). `scope` is the
    /// instance handle; `RT_UNSUPPORTED` means the host bound no events.
    /// Register a JSON channel owned by the scope's activation.
    pub event_register: unsafe extern "C" fn(*const c_void, u64, RtStr) -> RtStatus,
    /// Publish one JSON payload on a channel that accepts serialized input.
    pub event_publish:
        unsafe extern "C" fn(*const c_void, u64, RtStr, *const u8, usize) -> RtStatus,
    /// Subscribe (`state` = 1 for a state channel); the subscription belongs
    /// to the scope and is released with it.
    pub event_subscribe: unsafe extern "C" fn(*const c_void, u64, RtStr, u32, *mut u64) -> RtStatus,
    /// Copy the next item into `buf`: `RT_READY` (length written),
    /// `RT_PENDING` (the waker in `cx` is woken on the next item),
    /// `RT_TOO_SMALL` (length written, item retained) or `RT_CLOSED`.
    pub event_poll: unsafe extern "C" fn(
        *const c_void,
        u64,
        *const RtCx,
        *mut u8,
        usize,
        *mut usize,
    ) -> RtStatus,
    pub event_unsubscribe: unsafe extern "C" fn(*const c_void, u64),
    /// Asynchronous host service calls (see `rt::services`). `scope` is the
    /// instance handle that owns the call, `port` the invocation port the
    /// call is made through (its authority), and the request is the SDK's
    /// encoded host request. The call runs as a host task owned by the
    /// scope.
    pub service_call:
        unsafe extern "C" fn(*const c_void, u64, u64, *const u8, usize, *mut u64) -> RtStatus,
    /// Copy the encoded outcome into `buf`: `RT_READY` (length written; the
    /// call is finished and released), `RT_PENDING` (the waker in `cx` is
    /// woken on completion), `RT_TOO_SMALL` (length written, outcome
    /// retained) or `RT_CLOSED` (unknown or aborted call).
    pub service_poll: unsafe extern "C" fn(
        *const c_void,
        u64,
        *const RtCx,
        *mut u8,
        usize,
        *mut usize,
    ) -> RtStatus,
    /// Cancel and release a call; its host future is dropped.
    pub service_cancel: unsafe extern "C" fn(*const c_void, u64),
}
// SAFETY: the table is immutable after installation and its functions are
// thread-safe; `host` is only interpreted by the host.
unsafe impl Send for PlugRtV1 {}
unsafe impl Sync for PlugRtV1 {}

/// What the plugin reports back from installation.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlugRtGuestV1 {
    pub size: u32,
    pub abi_version: u32,
    pub build_fingerprint: [u8; 32],
    /// 1 when the library was built with `panic = "unwind"`.
    pub panic_unwind: u32,
}

pub type PlugRtInstallV1 = unsafe extern "C" fn(*const PlugRtV1, *mut PlugRtGuestV1) -> RtStatus;

pub(crate) const fn size_u32<T>() -> u32 {
    let size = std::mem::size_of::<T>();
    assert!(size <= u32::MAX as usize);
    let bytes = size.to_le_bytes();
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

const fn fnv(seed: u64, bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64 ^ seed;
    let mut index = 0;
    while index < bytes.len() {
        hash ^= bytes[index] as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        index += 1;
    }
    hash
}

/// Fingerprint of everything a direct `Waker` hand-off depends on: compiler
/// release and commit, target, panic strategy and this ABI version.
pub const BUILD_FINGERPRINT: [u8; 32] = {
    let compiler = env!("BMUX_PLUG_RT_COMPILER").as_bytes();
    let panic: u64 = if cfg!(panic = "unwind") { 1 } else { 2 };
    let version = PLUG_RT_ABI_VERSION as u64;
    let mut out = [0_u8; 32];
    let mut lane = 0;
    while lane < 4 {
        let hash = fnv((lane as u64) << 56 | panic << 8 | version, compiler).to_le_bytes();
        let mut byte = 0;
        while byte < 8 {
            out[lane * 8 + byte] = hash[byte];
            byte += 1;
        }
        lane += 1;
    }
    out
};
