//! Bridge one prepared entry and one local permit into selected execution.
#[cfg(test)]
mod tests;
use super::RustPluginAdapter;
use crate::{PluginCommandError, PluginFuture, PluginResult};
use std::{
    future::Future,
    sync::{Arc, RwLock},
};

/// Typed failure for a re-entrant entry that cannot be admitted immediately.
fn reentrant_gate_conflict() -> PluginCommandError {
    PluginCommandError::unavailable(
        "reentrant call would deadlock on exclusive instance gate: an ancestor entry of this \
         plugin instance holds the adapter gate exclusively, so the re-entrant query is \
         rejected instead of waiting",
    )
}

impl<P: Send + Sync + 'static> RustPluginAdapter<P> {
    pub(super) fn schedule<T: Send + 'static>(
        &self,
        execution: Option<crate::PluginCallbackContext>,
        exclusive: bool,
        callback: impl FnOnce(&RwLock<P>) -> PluginResult<T> + Send + 'static,
    ) -> PluginFuture<'static, PluginResult<T>> {
        let plugin = Arc::clone(&self.0);
        let gate = Arc::clone(&self.1);
        let instance = self.2;
        Box::pin(async move {
            let execution = execution.ok_or_else(|| {
                PluginCommandError::unavailable("callback execution authority missing")
            })?;
            let instance = instance.ok_or_else(|| {
                PluginCommandError::unavailable("callback instance binding missing")
            })?;
            let entry = execution.enter_adapter_entry(instance)?;
            let permit = if entry.is_reentrant() {
                // The host admitted a contract-approved re-entry while an
                // ancestor entry of this same instance is live and holds this
                // gate. Waiting could never complete, so admit now (shared
                // reads may pass queued writers) or fail explicitly.
                gate.try_acquire_reentrant(exclusive)?
                    .ok_or_else(reentrant_gate_conflict)?
            } else {
                let mut admission = Box::pin(gate.acquire(exclusive));
                let mut cancelled = execution.cancelled();
                let permit = std::future::poll_fn(|cx| {
                    if let std::task::Poll::Ready(result) = cancelled.as_mut().poll(cx) {
                        return std::task::Poll::Ready(Err(result.err().unwrap_or_else(|| {
                            PluginCommandError::unavailable("callback cancelled before entry")
                        })));
                    }
                    admission.as_mut().poll(cx)
                })
                .await?;
                // Fence queue-time notification before handing ownership to the
                // selected executor. A failed fence makes native authority non-live.
                drop(cancelled);
                permit
            };
            execution.check_live()?;
            let output = Arc::new(std::sync::Mutex::new(None));
            let writer = Arc::clone(&output);
            execution
                .execute_entered(
                    instance,
                    entry,
                    Box::new(move || {
                        let _permit = permit;
                        if plugin.is_poisoned() {
                            return Err(super::author_poisoned());
                        }
                        let result = callback(&plugin);
                        *writer.lock().map_err(|_| {
                            PluginCommandError::failed("callback result poisoned")
                        })? = Some(result);
                        Ok(())
                    }),
                )
                .await?;
            let result = output
                .lock()
                .map_err(|_| PluginCommandError::failed("callback result poisoned"))?
                .take();
            result.ok_or_else(|| PluginCommandError::failed("callback settled without result"))?
        })
    }
}
