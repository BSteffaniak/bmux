//! The activation scope that owns work spawned through plug-rt.
//!
//! The host enters a scope while it polls plugin code; tasks spawned from that
//! code join the scope and re-enter it on every poll, so nested spawns inherit
//! it. Bundled builds carry the host's [`BackgroundTasks`] directly. Inside a
//! dylib the scope is the instance handle the host registered it under; the
//! host resolves it to the same [`BackgroundTasks`].
use crate::background::BackgroundTasks;
use std::{
    cell::RefCell,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

#[derive(Clone)]
pub(crate) enum Current {
    #[cfg_attr(bmux_plugin_dylib, allow(dead_code))] // Bundled builds only.
    Local(BackgroundTasks),
    Remote(u64),
}

thread_local! {
    static CURRENT: RefCell<Option<Current>> = const { RefCell::new(None) };
}

/// Restores the previously entered scope on drop.
#[must_use = "the scope is entered only while the guard lives"]
pub struct ScopeGuard {
    previous: Option<Current>,
}
impl Drop for ScopeGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        let _ = CURRENT.try_with(|current| *current.borrow_mut() = previous);
    }
}

pub(crate) fn enter(scope: Current) -> ScopeGuard {
    let previous = CURRENT
        .try_with(|current| current.borrow_mut().replace(scope))
        .ok()
        .flatten();
    ScopeGuard { previous }
}

pub(crate) fn current() -> Option<Current> {
    CURRENT
        .try_with(|current| current.borrow().clone())
        .ok()
        .flatten()
}

/// Enter the dylib-side scope of one plugin instance handle (dynamic adapter).
pub fn enter_instance(handle: u64) -> ScopeGuard {
    enter(Current::Remote(handle))
}

/// The instance handle of the scope entered on this thread, or 0 for none.
#[cfg_attr(not(bmux_plugin_dylib), allow(dead_code))]
pub(crate) fn current_remote() -> u64 {
    match current() {
        Some(Current::Remote(handle)) => handle,
        _ => 0,
    }
}

/// A future that enters `scope` for each poll.
pub(crate) struct Scoped<F> {
    pub(crate) scope: Current,
    pub(crate) future: F,
}
impl<F: Future> Future for Scoped<F> {
    type Output = F::Output;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        // SAFETY: `future` is structurally pinned; `scope` is never moved out.
        let this = unsafe { self.get_unchecked_mut() };
        let _guard = enter(this.scope.clone());
        unsafe { Pin::new_unchecked(&mut this.future) }.poll(cx)
    }
}
