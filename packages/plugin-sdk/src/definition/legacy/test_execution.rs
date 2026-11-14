//! Test-only selected execution; never used as production fallback.
use crate::{
    PluginBlockingCallback, PluginCallbackAuthority, PluginCallbackAuthoritySource,
    PluginCallbackContext, PluginCallbackEntryGuard, PluginCallbackExecution,
    PluginCallbackExecutor, PluginCallbackSettlement, PluginResult,
};
use bmux_plugin_runtime::PluginInstanceId;
use std::sync::Arc;

struct Authority;
impl PluginCallbackAuthoritySource for Authority {
    fn check_live(&self) -> PluginResult<()> {
        Ok(())
    }
    fn enter_adapter_entry(&self, _: PluginInstanceId) -> PluginResult<PluginCallbackEntryGuard> {
        Ok(PluginCallbackEntryGuard::from_host(()))
    }
    fn cancelled(&self) -> PluginCallbackSettlement {
        Box::pin(std::future::pending())
    }
}
struct Executor;
impl PluginCallbackExecution for Executor {
    fn execute(
        &self,
        _: PluginCallbackAuthority,
        _: PluginInstanceId,
        _: PluginBlockingCallback,
    ) -> PluginCallbackSettlement {
        Box::pin(async {
            Err(crate::PluginCommandError::unavailable(
                "test requires prepared entry",
            ))
        })
    }
    fn execute_entered(
        &self,
        authority: PluginCallbackAuthority,
        instance: PluginInstanceId,
        entry: PluginCallbackEntryGuard,
        callback: PluginBlockingCallback,
    ) -> PluginCallbackSettlement {
        Box::pin(async move {
            entry.validate_for(&authority, instance)?;
            tokio::task::spawn_blocking(move || {
                entry.validate_for(&authority, instance)?;
                let result = callback();
                drop(entry);
                result
            })
            .await
            .map_err(|error| crate::PluginCommandError::failed(error.to_string()))?
        })
    }
}
pub(super) fn context() -> PluginCallbackContext {
    PluginCallbackContext::from_host(
        PluginCallbackAuthority::from_host(Arc::new(Authority)),
        PluginCallbackExecutor::from_host(Arc::new(Executor)),
    )
}
