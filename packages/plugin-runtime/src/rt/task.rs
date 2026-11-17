//! `spawn`, `spawn_blocking` and the handles they return.
use std::{
    fmt,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
};

/// Why a task produced no value.
#[derive(Debug, Clone, PartialEq, Eq)]
enum JoinErrorKind {
    Cancelled,
    Panicked(String),
    /// The owning activation scope stopped admitting work.
    Closed,
    /// No host runtime is reachable (outside a runtime, or a dylib whose host
    /// never installed plug-rt).
    Unavailable(String),
}

/// Error returned by awaiting a [`JoinHandle`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinError(JoinErrorKind);
impl JoinError {
    pub(crate) const fn cancelled() -> Self {
        Self(JoinErrorKind::Cancelled)
    }
    pub(crate) const fn panicked(message: String) -> Self {
        Self(JoinErrorKind::Panicked(message))
    }
    pub(crate) const fn closed() -> Self {
        Self(JoinErrorKind::Closed)
    }
    pub(crate) fn unavailable(reason: impl Into<String>) -> Self {
        Self(JoinErrorKind::Unavailable(reason.into()))
    }
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        matches!(self.0, JoinErrorKind::Cancelled)
    }
    #[must_use]
    pub const fn is_panic(&self) -> bool {
        matches!(self.0, JoinErrorKind::Panicked(_))
    }
    /// The scope was closed (teardown began) before the task was admitted.
    #[must_use]
    pub const fn is_closed(&self) -> bool {
        matches!(self.0, JoinErrorKind::Closed)
    }
    #[cfg(not(bmux_plugin_dylib))]
    fn from_tokio(error: &tokio::task::JoinError) -> Self {
        if error.is_cancelled() {
            Self::cancelled()
        } else {
            Self::panicked(error.to_string())
        }
    }
}
impl fmt::Display for JoinError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            JoinErrorKind::Cancelled => formatter.write_str("task was cancelled"),
            JoinErrorKind::Panicked(message) => write!(formatter, "task panicked: {message}"),
            JoinErrorKind::Closed => formatter.write_str("task scope is closed"),
            JoinErrorKind::Unavailable(reason) => {
                write!(formatter, "host runtime unavailable: {reason}")
            }
        }
    }
}
impl std::error::Error for JoinError {}

pub(crate) fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(ToString::to_string)
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".into())
}

enum SlotState<T> {
    Waiting(Option<Waker>),
    Done(Result<T, JoinError>),
    Taken,
}
/// Result cell shared by a dylib task and its handle. Lives entirely on the
/// plugin side; the host never sees `T`.
pub(crate) struct Slot<T>(Mutex<SlotState<T>>);
impl<T> Slot<T> {
    pub(crate) const fn new() -> Self {
        Self(Mutex::new(SlotState::Waiting(None)))
    }
    /// Record the first outcome only.
    pub(crate) fn complete(&self, outcome: Result<T, JoinError>) {
        let waker = {
            let mut state = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match &mut *state {
                SlotState::Waiting(waker) => {
                    let waker = waker.take();
                    *state = SlotState::Done(outcome);
                    waker
                }
                SlotState::Done(_) | SlotState::Taken => None,
            }
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
    /// Replace an unobserved outcome (used when admission was refused).
    pub(crate) fn overwrite(&self, outcome: Result<T, JoinError>) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !matches!(&*state, SlotState::Taken) {
            *state = SlotState::Done(outcome);
        }
    }
    fn is_done(&self) -> bool {
        !matches!(
            &*self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            SlotState::Waiting(_)
        )
    }
    fn poll_take(&self, cx: &Context<'_>) -> Poll<Result<T, JoinError>> {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match std::mem::replace(&mut *state, SlotState::Taken) {
            SlotState::Done(outcome) => Poll::Ready(outcome),
            SlotState::Waiting(_) => {
                *state = SlotState::Waiting(Some(cx.waker().clone()));
                Poll::Pending
            }
            SlotState::Taken => Poll::Ready(Err(JoinError::unavailable(
                "join handle polled after completion",
            ))),
        }
    }
}

enum Inner<T> {
    #[cfg(not(bmux_plugin_dylib))]
    Tokio(tokio::task::JoinHandle<T>),
    Remote {
        slot: Arc<Slot<T>>,
        abort: Option<Box<dyn Fn() + Send + Sync>>,
    },
    Failed(Option<JoinError>),
}

/// Handle to a spawned task or blocking job. Dropping it detaches the work;
/// the activation scope still owns it and stops it at teardown.
pub struct JoinHandle<T>(Inner<T>);
impl<T> fmt::Debug for JoinHandle<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JoinHandle")
            .field("finished", &self.is_finished())
            .finish()
    }
}
impl<T> JoinHandle<T> {
    #[cfg(not(bmux_plugin_dylib))]
    pub(crate) const fn tokio(handle: tokio::task::JoinHandle<T>) -> Self {
        Self(Inner::Tokio(handle))
    }
    pub(crate) fn remote(slot: Arc<Slot<T>>, abort: Option<Box<dyn Fn() + Send + Sync>>) -> Self {
        Self(Inner::Remote { slot, abort })
    }
    pub(crate) const fn failed(error: JoinError) -> Self {
        Self(Inner::Failed(Some(error)))
    }
    /// Request cancellation. A started blocking job cannot be aborted.
    pub fn abort(&self) {
        match &self.0 {
            #[cfg(not(bmux_plugin_dylib))]
            Inner::Tokio(handle) => handle.abort(),
            Inner::Remote {
                abort: Some(abort), ..
            } => abort(),
            Inner::Remote { abort: None, .. } | Inner::Failed(_) => {}
        }
    }
    #[must_use]
    pub fn is_finished(&self) -> bool {
        match &self.0 {
            #[cfg(not(bmux_plugin_dylib))]
            Inner::Tokio(handle) => handle.is_finished(),
            Inner::Remote { slot, .. } => slot.is_done(),
            Inner::Failed(_) => true,
        }
    }
}
impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: no field is structurally pinned except the Tokio handle,
        // which is `Unpin`.
        let this = unsafe { self.get_unchecked_mut() };
        match &mut this.0 {
            #[cfg(not(bmux_plugin_dylib))]
            Inner::Tokio(handle) => Pin::new(handle)
                .poll(cx)
                .map(|result| result.map_err(|error| JoinError::from_tokio(&error))),
            Inner::Remote { slot, .. } => slot.poll_take(cx),
            Inner::Failed(error) => Poll::Ready(Err(error
                .take()
                .unwrap_or_else(|| JoinError::unavailable("join handle polled after failure")))),
        }
    }
}

/// Spawn a task on the host runtime, owned by the current activation scope.
pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    spawn_named("plugin-task", future)
}

/// [`spawn`] with a name used in teardown diagnostics.
pub fn spawn_named<F>(name: &str, future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    #[cfg(bmux_plugin_dylib)]
    {
        super::guest::installed().map_or_else(
            || JoinHandle::failed(JoinError::unavailable(super::guest::NOT_INSTALLED)),
            |client| client.spawn(super::scope::current_remote(), name, future),
        )
    }
    #[cfg(not(bmux_plugin_dylib))]
    {
        bundled::spawn(name, future)
    }
}

/// Run blocking work on the host's blocking pool, owned by the current scope.
pub fn spawn_blocking<F, T>(work: F) -> JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    spawn_blocking_named("plugin-blocking", work)
}

/// [`spawn_blocking`] with a name used in teardown diagnostics.
pub fn spawn_blocking_named<F, T>(name: &str, work: F) -> JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    #[cfg(bmux_plugin_dylib)]
    {
        super::guest::installed().map_or_else(
            || JoinHandle::failed(JoinError::unavailable(super::guest::NOT_INSTALLED)),
            |client| client.spawn_blocking(super::scope::current_remote(), name, work),
        )
    }
    #[cfg(not(bmux_plugin_dylib))]
    {
        bundled::spawn_blocking(name, work)
    }
}

/// Run blocking work that a host-admitted operation awaits (for example an
/// adapter executing a synchronous author callback for that operation).
///
/// Unlike [`spawn_blocking`] it is owned by the operation, not admitted into
/// the activation scope, so it still runs while that scope is closing for
/// teardown: the host's own deactivation must be able to execute. The work
/// still runs inside the current scope, so tasks it spawns join it.
pub fn spawn_blocking_for_operation<F, T>(name: &str, work: F) -> JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    #[cfg(bmux_plugin_dylib)]
    {
        super::guest::installed().map_or_else(
            || JoinHandle::failed(JoinError::unavailable(super::guest::NOT_INSTALLED)),
            |client| {
                client.spawn_blocking_for_operation(super::scope::current_remote(), name, work)
            },
        )
    }
    #[cfg(not(bmux_plugin_dylib))]
    {
        let _ = name;
        bundled::spawn_blocking_for_operation(work)
    }
}

/// Yield once to the scheduler. Runtime-agnostic.
pub async fn yield_now() {
    let mut yielded = false;
    std::future::poll_fn(|cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
}

#[cfg(not(bmux_plugin_dylib))]
mod bundled {
    use super::{JoinError, JoinHandle};
    use crate::rt::scope::{self, Current, Scoped};
    use std::future::Future;

    fn runtime() -> Result<tokio::runtime::Handle, JoinError> {
        tokio::runtime::Handle::try_current()
            .map_err(|error| JoinError::unavailable(error.to_string()))
    }

    pub(super) fn spawn<F>(name: &str, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let runtime = match runtime() {
            Ok(runtime) => runtime,
            Err(error) => return JoinHandle::failed(error),
        };
        let Some(Current::Local(tasks)) = scope::current() else {
            // Outside any activation the host runtime alone owns the task.
            return JoinHandle::tokio(runtime.spawn(future));
        };
        let Ok(ticket) = tasks.admit_task(name) else {
            return JoinHandle::failed(JoinError::closed());
        };
        let id = ticket.id();
        let scoped = Scoped {
            scope: Current::Local(tasks.clone()),
            future,
        };
        let handle = runtime.spawn(async move {
            let _ticket = ticket;
            scoped.await
        });
        let abort = handle.abort_handle();
        tasks.bind_abort(id, Box::new(move || abort.abort()));
        JoinHandle::tokio(handle)
    }

    pub(super) fn spawn_blocking_for_operation<F, T>(work: F) -> JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let runtime = match runtime() {
            Ok(runtime) => runtime,
            Err(error) => return JoinHandle::failed(error),
        };
        let current = scope::current();
        JoinHandle::tokio(runtime.spawn_blocking(move || {
            let _scope = current.map(scope::enter);
            work()
        }))
    }

    pub(super) fn spawn_blocking<F, T>(name: &str, work: F) -> JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let runtime = match runtime() {
            Ok(runtime) => runtime,
            Err(error) => return JoinHandle::failed(error),
        };
        let Some(Current::Local(tasks)) = scope::current() else {
            return JoinHandle::tokio(runtime.spawn_blocking(work));
        };
        let Ok(ticket) = tasks.admit_blocking(name) else {
            return JoinHandle::failed(JoinError::closed());
        };
        JoinHandle::tokio(runtime.spawn_blocking(move || {
            let _ticket = ticket;
            let _scope = scope::enter(Current::Local(tasks));
            work()
        }))
    }
}
