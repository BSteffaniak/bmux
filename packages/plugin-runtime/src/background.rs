//! Activation-scoped ownership of cooperative background work.
use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::watch, task::JoinHandle};

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

#[derive(Debug)]
struct State {
    closing: bool,
    failures: Vec<String>,
    tasks: Vec<(String, JoinHandle<Result<(), String>>)>,
}

/// One activation's bounded task collection. The host must drain it before
/// releasing plugin resources. Cancellation never aborts in-flight blocking IO.
#[derive(Debug, Clone)]
pub struct BackgroundTasks {
    state: Arc<Mutex<State>>,
    runtime: Option<tokio::runtime::Handle>,
    drain: Arc<tokio::sync::Mutex<()>>,
    cancel: watch::Sender<bool>,
}
impl Default for BackgroundTasks {
    fn default() -> Self {
        Self::new()
    }
}
impl BackgroundTasks {
    /// Create an open activation scope.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                closing: false,
                failures: Vec::new(),
                tasks: Vec::new(),
            })),
            runtime: tokio::runtime::Handle::try_current().ok(),
            drain: Arc::new(tokio::sync::Mutex::new(())),
            cancel: watch::channel(false).0,
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
        let runtime = self.runtime.as_ref().ok_or("host runtime unavailable")?;
        let task = runtime.spawn(work(TaskCancellation(self.cancel.subscribe())));
        state.tasks.push((name.to_string(), task));
        Ok(())
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
