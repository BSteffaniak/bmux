//! Read authorization boundary for explicitly negotiated live presentation.
//! Coordinator wires generated routes; no durable-v1 fallback or side effects.
use bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalExecutionRef;
use bmux_pane_runtime_plugin_api::pane_runtime_live_presentation_v1::{
    LiveCapabilities, LiveCursor, LiveError, LiveFrame,
};
use bmux_plugin_sdk::NativeServiceContext;

pub(crate) fn capabilities() -> LiveCapabilities {
    crate::runtime::live_presentation::capabilities()
}

/// Read one authorized live update without changing durable recording state.
///
/// # Errors
/// Returns authorization, budget, coherence or owner-runtime failures.
pub(crate) fn read(
    host: &crate::runtime::PaneRuntimeHost,
    ctx: &NativeServiceContext,
    execution: &TerminalExecutionRef,
    after: Option<&LiveCursor>,
    max_bytes: u32,
) -> Result<Option<LiveFrame>, LiveError> {
    crate::runtime::live_presentation::read(host, execution, after, max_bytes, || {
        super::execution::authorize(ctx, execution, "read")
    })
}

/// Wait for an authorized live update on the persistent pane owner runtime.
///
/// # Errors
/// Returns authorization, subscription, budget, coherence or runtime failures.
pub(crate) fn wait_present(
    host: &crate::runtime::PaneRuntimeHost,
    ctx: &NativeServiceContext,
    execution: &TerminalExecutionRef,
    after: Option<&LiveCursor>,
    timeout_ms: u32,
    max_bytes: u32,
) -> Result<Option<LiveFrame>, LiveError> {
    crate::runtime::live_presentation::wait_present(
        host,
        execution,
        after,
        timeout_ms,
        max_bytes,
        || super::execution::authorize(ctx, execution, "read"),
    )
}
