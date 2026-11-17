//! Real wakers across the plug-rt boundary.
//!
//! Exporting lends a Rust `Waker` as an [`RtWaker`] for one call; a clone made
//! through its vtable boxes a `Waker` on the exporting side, which is also the
//! side that frees it. Importing wraps a received [`RtWaker`] as a Rust
//! `Waker`; a clone boxes the `RtWaker` on the importing side. With matching
//! build fingerprints the sender also passes its `Waker` directly and the
//! receiver uses it without any allocation.
use super::abi::{RtCx, RtWaker, RtWakerVTable};
use std::{
    ffi::c_void,
    mem::ManuallyDrop,
    panic::{AssertUnwindSafe, catch_unwind},
    task::{RawWaker, RawWakerVTable, Waker},
};

// ---- Export: a local Waker seen through the C shim --------------------------

static BORROWED: RtWakerVTable = RtWakerVTable {
    clone: borrowed_clone,
    // A borrowed waker is never consumed; receivers only clone or wake by ref.
    wake: borrowed_wake_by_ref,
    wake_by_ref: borrowed_wake_by_ref,
    drop: borrowed_drop,
};
static OWNED: RtWakerVTable = RtWakerVTable {
    clone: owned_clone,
    wake: owned_wake,
    wake_by_ref: owned_wake_by_ref,
    drop: owned_drop,
};

fn owned(waker: Waker) -> RtWaker {
    RtWaker {
        data: Box::into_raw(Box::new(waker)).cast_const().cast(),
        vtable: &raw const OWNED,
    }
}
fn contained(operation: impl FnOnce()) {
    // A panicking waker must not unwind into the other image.
    let _ = catch_unwind(AssertUnwindSafe(operation));
}
unsafe extern "C" fn borrowed_clone(data: *const c_void) -> RtWaker {
    // SAFETY: `data` is the `&Waker` lent for the duration of this call.
    let waker = unsafe { &*data.cast::<Waker>() };
    catch_unwind(AssertUnwindSafe(|| owned(waker.clone()))).unwrap_or_else(|_| noop())
}
unsafe extern "C" fn borrowed_wake_by_ref(data: *const c_void) {
    contained(|| unsafe { &*data.cast::<Waker>() }.wake_by_ref());
}
unsafe extern "C" fn borrowed_drop(_: *const c_void) {}
unsafe extern "C" fn owned_clone(data: *const c_void) -> RtWaker {
    unsafe { borrowed_clone(data) }
}
unsafe extern "C" fn owned_wake(data: *const c_void) {
    // SAFETY: `data` came from `owned` and is consumed exactly once.
    let waker = unsafe { Box::from_raw(data.cast::<Waker>().cast_mut()) };
    contained(move || waker.wake());
}
unsafe extern "C" fn owned_wake_by_ref(data: *const c_void) {
    unsafe { borrowed_wake_by_ref(data) }
}
unsafe extern "C" fn owned_drop(data: *const c_void) {
    // SAFETY: `data` came from `owned` and is released exactly once.
    let waker = unsafe { Box::from_raw(data.cast::<Waker>().cast_mut()) };
    contained(move || drop(waker));
}
unsafe extern "C" fn noop_clone(_: *const c_void) -> RtWaker {
    noop()
}
unsafe extern "C" fn noop_fn(_: *const c_void) {}
static NOOP: RtWakerVTable = RtWakerVTable {
    clone: noop_clone,
    wake: noop_fn,
    wake_by_ref: noop_fn,
    drop: noop_fn,
};
const fn noop() -> RtWaker {
    RtWaker {
        data: std::ptr::null(),
        vtable: &raw const NOOP,
    }
}

/// Lend `waker` to one cross-boundary call as an [`RtCx`].
pub(crate) fn with_cx<R>(waker: &Waker, matched: bool, call: impl FnOnce(*const RtCx) -> R) -> R {
    let cx = RtCx {
        waker: RtWaker {
            data: std::ptr::from_ref(waker).cast(),
            vtable: &raw const BORROWED,
        },
        rust_waker: if matched {
            std::ptr::from_ref(waker)
        } else {
            std::ptr::null()
        },
    };
    call(&raw const cx)
}

// ---- Import: a remote RtWaker seen as a Rust Waker --------------------------

const IMPORT_BORROWED: RawWakerVTable = RawWakerVTable::new(
    import_borrowed_clone,
    import_borrowed_wake_by_ref,
    import_borrowed_wake_by_ref,
    import_borrowed_drop,
);
const IMPORT_OWNED: RawWakerVTable = RawWakerVTable::new(
    import_owned_clone,
    import_owned_wake,
    import_owned_wake_by_ref,
    import_owned_drop,
);
unsafe fn import_borrowed_clone(data: *const ()) -> RawWaker {
    // SAFETY: `data` is the `RtWaker` borrowed for this call.
    let remote = unsafe { &*data.cast::<RtWaker>() };
    let owned = unsafe { ((*remote.vtable).clone)(remote.data) };
    RawWaker::new(
        Box::into_raw(Box::new(owned)).cast_const().cast(),
        &IMPORT_OWNED,
    )
}
unsafe fn import_borrowed_wake_by_ref(data: *const ()) {
    let remote = unsafe { &*data.cast::<RtWaker>() };
    unsafe { ((*remote.vtable).wake_by_ref)(remote.data) };
}
unsafe fn import_borrowed_drop(_: *const ()) {}
unsafe fn import_owned_clone(data: *const ()) -> RawWaker {
    unsafe { import_borrowed_clone(data) }
}
unsafe fn import_owned_wake(data: *const ()) {
    // SAFETY: the box was allocated by `import_borrowed_clone` on this side.
    let remote = unsafe { Box::from_raw(data.cast::<RtWaker>().cast_mut()) };
    unsafe { ((*remote.vtable).wake)(remote.data) };
}
unsafe fn import_owned_wake_by_ref(data: *const ()) {
    unsafe { import_borrowed_wake_by_ref(data) }
}
unsafe fn import_owned_drop(data: *const ()) {
    let remote = unsafe { Box::from_raw(data.cast::<RtWaker>().cast_mut()) };
    unsafe { ((*remote.vtable).drop)(remote.data) };
}

/// Run `poll` with the waker carried by a received [`RtCx`].
///
/// # Safety
/// `cx` must be a valid [`RtCx`] for the duration of this call, and its
/// `rust_waker` must be non-null only when build fingerprints match.
pub(crate) unsafe fn with_waker<R>(cx: *const RtCx, poll: impl FnOnce(&Waker) -> R) -> R {
    let cx = unsafe { &*cx };
    if !cx.rust_waker.is_null() {
        // SAFETY: same compiler, target and std: the layout is identical.
        return poll(unsafe { &*cx.rust_waker });
    }
    // SAFETY: the shim vtable is valid; the borrowed waker is never consumed
    // because only a reference to it escapes and clones re-own through vtable.
    let waker = ManuallyDrop::new(unsafe {
        Waker::from_raw(RawWaker::new(
            std::ptr::from_ref(&cx.waker).cast(),
            &IMPORT_BORROWED,
        ))
    });
    poll(&waker)
}
