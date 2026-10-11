//! Authorization at the exact execution service boundary.
use bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::{
    TerminalExecutionError as ExecutionError, TerminalExecutionRef as ExecutionRef,
};
use bmux_plugin_sdk::NativeServiceContext;

pub(crate) fn authorize(
    ctx: &NativeServiceContext,
    execution: &ExecutionRef,
    action: &str,
) -> Result<(), ExecutionError> {
    if ctx.cancellation.is_cancelled() {
        return Err(ExecutionError::OperationFailed {
            reason: "execution request cancelled".into(),
        });
    }
    super::spawn::authorize_session(ctx, execution.session_id, action).map_err(|error| {
        ExecutionError::Denied {
            reason: format!("{error:?}"),
        }
    })
}
