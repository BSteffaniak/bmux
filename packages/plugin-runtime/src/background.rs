//! Activation-scoped ownership of cooperative background work.
//!
//! This is also the plug-rt task scope: work spawned through
//! [`crate::rt::spawn`] while a scope is entered is registered here, so the host
//! stops it at teardown. Teardown follows one order: stop admitting and cancel
//! (cooperative tasks observe [`TaskCancellation`]), drain cooperative tasks,
//! abort raw spawned tasks (each future is dropped by the side that owns it),
//! then wait for running blocking jobs, which cannot be aborted, and report
//! any that miss the deadline by name.
use crate::rt::{self, JoinHandle};
use std::{
    collections::BTreeMap,
    fmt,
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::watch;

/// Cancellation observed by an activation's tasks.
#[derive(Debug, Clone)]
pub struct TaskCancellation(watch::Receiver<bool>);
impl TaskCancellation {
    /// Wait until shutdown has been requested.
    pub async fn cancelled(&mut self) {
        while !*self.0.borrow_and_update() {
            if self.0.changed().await.is_err() {
                return;
            }
        }
    }
}

struct AbortFn(Box<dyn Fn() + Send + Sync>);
impl fmt::Debug for AbortFn {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AbortFn")
    }
}

#[derive(Debug)]
struct Spawned {
    name: String,
    abort: Option<AbortFn>,
}

#[derive(Debug)]
struct State {
    closing: bool,
    /// Raw tasks admitted after this point are aborted on admission.
    aborting: bool,
    failures: Vec<String>,
    tasks: Vec<(String, JoinHandle<Result<(), String>>)>,
    next: u64,
    /// Raw plug-rt tasks, removed when their future is dropped.
    spawned: BTreeMap<u64, Spawned>,
    /// Running or queued blocking jobs, removed when they return.
    blocking: BTreeMap<u64, String>,
}

/// One activation's bounded task collection. The host must drain it before
/// releasing plugin resources. Cancellation never aborts in-flight blocking IO.
#[derive(Debug, Clone)]
pub struct BackgroundTasks {
    state: Arc<Mutex<State>>,
    #[cfg(not(bmux_plugin_dylib))]
    runtime: Option<tokio::runtime::Handle>,
    drain: Arc<tokio::sync::Mutex<()>>,
    cancel: watch::Sender<bool>,
    settled: watch::Sender<u64>,
}
impl Default for BackgroundTasks {
    fn default() -> Self {
        Self::new()
    }
}

/// Registration of one raw task or blocking job; dropping it settles the entry.
#[derive(Debug)]
pub(crate) struct Ticket {
    id: u64,
    blocking: bool,
    state: Arc<Mutex<State>>,
    settled: watch::Sender<u64>,
}
impl Ticket {
    pub(crate) const fn id(&self) -> u64 {
        self.id
    }
}
impl Drop for Ticket {
    fn drop(&mut self) {
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if self.blocking {
                state.blocking.remove(&self.id);
            } else {
                state.spawned.remove(&self.id);
            }
        }
        self.settled
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }
}

impl BackgroundTasks {
    /// Create an open activation scope.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                closing: false,
                aborting: false,
                failures: Vec::new(),
                tasks: Vec::new(),
                next: 1,
                spawned: BTreeMap::new(),
                blocking: BTreeMap::new(),
            })),
            #[cfg(not(bmux_plugin_dylib))]
            runtime: tokio::runtime::Handle::try_current().ok(),
            drain: Arc::new(tokio::sync::Mutex::new(())),
            cancel: watch::channel(false).0,
            settled: watch::channel(0).0,
        }
    }

    /// Enter this scope on the current thread: plug-rt spawns join it.
    pub fn enter(&self) -> rt::ScopeGuard {
        rt::scope::enter(rt::scope::Current::Local(self.clone()))
    }

    /// Wrap `future` so every poll runs inside this scope.
    pub fn scoped<F: Future>(&self, future: F) -> impl Future<Output = F::Output> + use<F> {
        rt::scope::Scoped {
            scope: rt::scope::Current::Local(self.clone()),
            future,
        }
    }

    fn admit(&self, name: &str, blocking: bool) -> Result<Ticket, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "background task scope poisoned")?;
        if state.closing {
            return Err("background task scope is closing".into());
        }
        let id = state.next;
        state.next = state.next.wrapping_add(1);
        if blocking {
            state.blocking.insert(id, name.to_string());
        } else {
            state.spawned.insert(
                id,
                Spawned {
                    name: name.to_string(),
                    abort: None,
                },
            );
        }
        drop(state);
        Ok(Ticket {
            id,
            blocking,
            state: Arc::clone(&self.state),
            settled: self.settled.clone(),
        })
    }

    /// Admit one raw plug-rt task. Refused once teardown began.
    pub(crate) fn admit_task(&self, name: &str) -> Result<Ticket, String> {
        self.admit(name, false)
    }

    /// Admit one blocking job. Refused once teardown began.
    pub(crate) fn admit_blocking(&self, name: &str) -> Result<Ticket, String> {
        self.admit(name, true)
    }

    /// Attach the abort action of an admitted raw task. If teardown is
    /// already aborting raw tasks, the task is aborted immediately.
    pub(crate) fn bind_abort(&self, id: u64, abort: Box<dyn Fn() + Send + Sync>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let aborting = state.aborting;
        let Some(entry) = state.spawned.get_mut(&id) else {
            return;
        };
        if aborting {
            drop(state);
            abort();
        } else {
            entry.abort = Some(AbortFn(abort));
        }
    }

    /// Register work atomically against shutdown. Completed tasks retain their
    /// outcomes until drained; at most 32 tasks may belong to one activation.
    ///
    /// # Errors
    /// Rejects closed, poisoned, or full scopes, and missing runtimes.
    pub fn spawn<F, Fut>(&self, name: &str, work: F) -> Result<(), String>
    where
        F: FnOnce(TaskCancellation) -> Fut,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "background task scope poisoned")?;
        if state.closing {
            return Err("background task scope is closing".into());
        }
        if state.tasks.len() == 32 {
            return Err("background task limit reached".into());
        }
        let task = self.spawn_cooperative(name, work(TaskCancellation(self.cancel.subscribe())))?;
        state.tasks.push((name.to_string(), task));
        Ok(())
    }

    #[cfg(not(bmux_plugin_dylib))]
    fn spawn_cooperative<Fut>(
        &self,
        _name: &str,
        work: Fut,
    ) -> Result<JoinHandle<Result<(), String>>, String>
    where
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        let runtime = self.runtime.as_ref().ok_or("host runtime unavailable")?;
        Ok(JoinHandle::tokio(runtime.spawn(self.scoped(work))))
    }

    #[cfg(bmux_plugin_dylib)]
    #[allow(clippy::unnecessary_wraps)] // Same signature as the bundled build.
    fn spawn_cooperative<Fut>(
        &self,
        name: &str,
        work: Fut,
    ) -> Result<JoinHandle<Result<(), String>>, String>
    where
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        // Inside a dylib the host-side scope of the entered instance owns it.
        Ok(rt::spawn_named(name, work))
    }

    /// Request cancellation without waiting, so the host can stop all producers
    /// before draining scopes and tearing down their service dependencies.
    ///
    /// # Errors
    /// Reports poisoned scope state.
    pub fn cancel(&self) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "background task scope poisoned")?;
        state.closing = true;
        self.cancel.send_replace(true);
        Ok(())
    }

    /// Abort raw spawned tasks only; cooperative tasks keep draining.
    fn abort_spawned(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.aborting = true;
        let aborts: Vec<AbortFn> = state
            .spawned
            .values_mut()
            .filter_map(|entry| entry.abort.take())
            .collect();
        drop(state);
        // Never call into task handles while holding the scope lock.
        for abort in aborts {
            (abort.0)();
        }
    }

    /// Abort every retained task at a teardown deadline. Handles stay retained
    /// until drained, and an aborted task is recorded as a failure, so aborting
    /// can never turn incomplete cleanup into success. Code is never unloaded.
    pub fn abort(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closing = true;
        self.cancel.send_replace(true);
        for (_, task) in &state.tasks {
            task.abort();
        }
        drop(state);
        self.abort_spawned();
    }

    /// Names of task handles that have not been reaped by a drain yet,
    /// including raw spawned tasks and running blocking jobs.
    #[must_use]
    pub fn pending_names(&self) -> Vec<String> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .tasks
            .iter()
            .map(|(name, _)| name.clone())
            .chain(state.spawned.values().map(|entry| entry.name.clone()))
            .chain(
                state
                    .blocking
                    .values()
                    .map(|name| format!("{name} (blocking)")),
            )
            .collect()
    }

    /// Names of blocking jobs still running.
    #[must_use]
    pub fn blocking_names(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .blocking
            .values()
            .cloned()
            .collect()
    }

    async fn wait_until(&self, settled: impl Fn(&State) -> bool) {
        let mut changes = self.settled.subscribe();
        loop {
            {
                let state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if settled(&state) {
                    return;
                }
            }
            if changes.changed().await.is_err() {
                return;
            }
        }
    }

    /// Drain until `deadline`. On expiry the remaining tasks are aborted and
    /// the error names them; their handles remain retained. Running blocking
    /// jobs are then awaited until the deadline and stragglers are reported.
    /// Host-side teardown: it uses the host runtime's timers.
    ///
    /// # Errors
    /// Reports deadline expiry (with pending task names), task failures, or
    /// poisoned state.
    pub async fn shutdown_until(
        &self,
        deadline: tokio::time::Instant,
        progress: Option<crate::shutdown_progress::ShutdownProgress>,
    ) -> Result<(), String> {
        let outcome = if let Ok(outcome) =
            tokio::time::timeout_at(deadline, self.shutdown_retained(progress)).await
        {
            outcome
        } else {
            let pending = self.pending_names();
            self.abort();
            tracing::warn!(
                ?pending,
                "background tasks missed the teardown deadline; aborted and abandoned"
            );
            Err(format!(
                "background tasks did not drain before the teardown deadline; aborted: {}",
                pending.join(", ")
            ))
        };
        // Blocking jobs cannot be aborted: wait for them within the same
        // deadline, then report (never unload) any that are still running.
        let blocking_settled =
            tokio::time::timeout_at(deadline, self.wait_until(|state| state.blocking.is_empty()))
                .await
                .is_ok();
        if !blocking_settled {
            let stragglers = self.blocking_names();
            tracing::warn!(
                ?stragglers,
                "blocking plugin jobs still running after the teardown deadline; abandoned"
            );
        }
        outcome
    }

    /// Final step before an instance is released: stop admitting, abort raw
    /// tasks (each future is dropped by the side that owns it), then wait
    /// until `deadline` for those drops and for running blocking jobs.
    ///
    /// Returns the work still outstanding at the deadline, which is abandoned
    /// and reported, never unloaded. Cooperative task outcomes are not
    /// re-reported here; [`Self::shutdown_until`] owns them.
    pub async fn abort_and_settle(&self, deadline: tokio::time::Instant) -> Vec<String> {
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.closing = true;
        }
        self.cancel.send_replace(true);
        self.abort_spawned();
        let settled = tokio::time::timeout_at(
            deadline,
            self.wait_until(|state| state.spawned.is_empty() && state.blocking.is_empty()),
        )
        .await
        .is_ok();
        if settled {
            return Vec::new();
        }
        let stragglers = self.pending_names();
        tracing::warn!(
            ?stragglers,
            "plugin work still running when its instance was released; abandoned"
        );
        stragglers
    }

    /// Explicitly acknowledge repaired task failures after all handles settled.
    /// This never cancels, detaches, or discards unfinished work.
    pub(crate) fn acknowledge_failures(&self) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "background task scope poisoned")?;
        if !state.closing || !state.tasks.is_empty() {
            return Err("background tasks have not drained".into());
        }
        state.failures.clear();
        Ok(())
    }

    /// Cancel and drain all work. A timeout retains task handles so shutdown can
    /// be retried; it does not authorize unloading plugin code or state.
    ///
    /// # Errors
    /// Reports timeout, task failure, panic, or poisoned state.
    pub async fn shutdown(&self, timeout: Duration) -> Result<(), String> {
        tokio::time::timeout(timeout, self.shutdown_retained(None))
            .await
            .map_err(|_| "background tasks did not drain".to_string())?
    }

    /// Drain without a total deadline; observers own inactivity policy.
    ///
    /// Cooperative tasks are awaited; raw spawned tasks, which have no
    /// cancellation token, are aborted once cooperative work drained and are
    /// awaited until their futures were dropped. Blocking jobs are not
    /// awaited here (see [`Self::shutdown_until`]).
    ///
    /// # Errors
    /// Reports task failures or poisoned state while retaining unfinished owners.
    pub async fn shutdown_retained(
        &self,
        progress: Option<crate::shutdown_progress::ShutdownProgress>,
    ) -> Result<(), String> {
        self.cancel()?;
        let _drain = self.drain.lock().await;
        loop {
            // Poll retained handles without removing them: cancelling shutdown
            // itself must not detach unfinished work.
            let done = std::future::poll_fn(|cx| {
                let Ok(mut state) = self.state.lock() else {
                    return std::task::Poll::Ready(Err(
                        "background task scope poisoned".to_string()
                    ));
                };
                if state.tasks.is_empty() {
                    return std::task::Poll::Ready(Ok(None));
                }
                for index in (0..state.tasks.len()).rev() {
                    let (name, task) = &mut state.tasks[index];
                    if let std::task::Poll::Ready(result) = std::pin::Pin::new(task).poll(cx) {
                        let name = name.clone();
                        let (_, task) = state.tasks.swap_remove(index);
                        drop(task);
                        if !matches!(result, Ok(Ok(()))) {
                            state.failures.push(format!("{name}: {result:?}"));
                        }
                        return std::task::Poll::Ready(Ok(Some(())));
                    }
                }
                std::task::Poll::Pending
            });
            match done.await {
                Err(error) => return Err(error),
                Ok(None) => break,
                Ok(Some(())) => {
                    if let Some(progress) = &progress
                        && let Err(error) = progress.advance("background task settled")
                    {
                        tracing::warn!(%error, "shutdown progress publication failed");
                    }
                }
            }
        }
        self.abort_spawned();
        self.wait_until(|state| state.spawned.is_empty()).await;
        let state = self
            .state
            .lock()
            .map_err(|_| "background task scope poisoned")?;
        if state.failures.is_empty() {
            Ok(())
        } else {
            Err(state.failures.join("; "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancellation_drains_and_closes_scope() {
        let tasks = BackgroundTasks::new();
        let (sent, received) = tokio::sync::oneshot::channel();
        tasks
            .spawn("listener", |mut cancel| async move {
                cancel.cancelled().await;
                sent.send(()).unwrap();
                Ok(())
            })
            .unwrap();
        tasks.shutdown(Duration::from_secs(1)).await.unwrap();
        received.await.unwrap();
        assert!(tasks.spawn("late", |_| async { Ok(()) }).is_err());
        tasks.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test]
    async fn timeout_retains_work_and_failures_survive_retry() {
        let tasks = BackgroundTasks::new();
        let (sent, received) = tokio::sync::oneshot::channel();
        tasks
            .spawn("blocked", |_| async move {
                received.await.unwrap();
                Err("effect failed".into())
            })
            .unwrap();
        assert!(tasks.shutdown(Duration::from_millis(1)).await.is_err());
        sent.send(()).unwrap();
        for _ in 0..2 {
            assert!(
                tasks
                    .shutdown(Duration::from_secs(1))
                    .await
                    .unwrap_err()
                    .contains("effect failed")
            );
        }
    }

    #[tokio::test]
    async fn panics_and_capacity_are_explicit() {
        let tasks = BackgroundTasks::new();
        tasks
            .spawn("panic", |_| async { panic!("broken worker") })
            .unwrap();
        assert!(
            tasks
                .shutdown(Duration::from_secs(1))
                .await
                .unwrap_err()
                .contains("panic")
        );
        let tasks = BackgroundTasks::new();
        for _ in 0..32 {
            tasks
                .spawn("bounded", |mut cancel| async move {
                    cancel.cancelled().await;
                    Ok(())
                })
                .unwrap();
        }
        assert!(tasks.spawn("overflow", |_| async { Ok(()) }).is_err());
        tasks.shutdown(Duration::from_secs(1)).await.unwrap();
    }
}
