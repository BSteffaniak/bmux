//! Authorization at the exact execution service boundary.
use bmux_pane_runtime_plugin_api::pane_runtime_execution_v1::{ExecutionError, ExecutionRef};
use bmux_plugin_sdk::NativeServiceContext;

pub(super) fn authorize(
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
