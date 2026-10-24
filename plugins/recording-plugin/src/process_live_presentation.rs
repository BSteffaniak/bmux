//! Recording ownership facade for negotiated live presentation. Never refreshes
//! capture/status or treats projected progress as a durable recording receipt.
use bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::{
    TerminalExecutionError, TerminalExecutionRef,
};
use bmux_pane_runtime_plugin_api::pane_runtime_live_presentation_v1::{
    self as live, LiveCapabilities, LiveCursor, LiveError, LiveFrame,
};
use bmux_plugin::ServiceCallerDispatchClient;
use uuid::Uuid;

use crate::process_runtime::ProcessService;

fn unavailable(error: &impl ToString) -> LiveError {
    LiveError::Authority {
        error: TerminalExecutionError::OperationFailed {
            reason: error.to_string(),
        },
    }
}

fn validate_frame(
    reference: &TerminalExecutionRef,
    frame: Option<LiveFrame>,
) -> Result<Option<LiveFrame>, LiveError> {
    if frame.as_ref().is_some_and(|frame| {
        frame.cursor.execution != *reference
            || frame.cursor.source_epoch != reference.execution_token
            || frame.screen.as_ref().is_some_and(|screen| {
                screen.execution != *reference
                    || screen.output_watermark != frame.cursor.projected_raw_watermark
            })
    }) {
        return Err(LiveError::Authority {
            error: TerminalExecutionError::StaleExecution,
        });
    }
    Ok(frame)
}

impl ProcessService {
    /// Resolve durable ownership only; this lane does not finalize or persist.
    fn live_reference(&self, run_id: Uuid) -> Result<TerminalExecutionRef, LiveError> {
        let value = self.load(run_id).map_err(|error| unavailable(&error))?;
        let reference = value
            .execution
            .as_ref()
            .ok_or_else(|| unavailable(&"run has no execution"))?;
        Ok(TerminalExecutionRef {
            session_id: reference.session_id,
            pane_id: reference.pane_id,
            execution_token: reference.execution_token,
        })
    }

    /// Query the explicitly negotiated pane capability; no durable-v1 fallback.
    ///
    /// # Errors
    /// Returns typed-dispatch transport or capability availability failures.
    pub(crate) async fn live_capabilities(&self) -> anyhow::Result<LiveCapabilities> {
        let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
        Ok(live::client::capabilities(&mut client).await?)
    }

    /// Read the live projection for the execution durably owned by this run.
    ///
    /// # Errors
    /// Returns ownership, stale execution, transport or pane validation errors.
    pub(crate) async fn live_read(
        &self,
        run_id: Uuid,
        after: Option<LiveCursor>,
        max_bytes: u32,
    ) -> Result<Option<LiveFrame>, LiveError> {
        let reference = self.live_reference(run_id)?;
        let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
        let frame = live::client::read(&mut client, reference.clone(), after, max_bytes)
            .await
            .map_err(|error| unavailable(&error))??;
        validate_frame(&reference, frame)
    }

    /// Wait without advancing recording state or owning a displayed scene.
    ///
    /// # Errors
    /// Returns ownership, stale execution, transport or pane validation errors.
    pub(crate) async fn live_wait_present(
        &self,
        run_id: Uuid,
        after: Option<LiveCursor>,
        timeout_ms: u32,
        max_bytes: u32,
    ) -> Result<Option<LiveFrame>, LiveError> {
        let reference = self.live_reference(run_id)?;
        let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
        let frame = live::client::wait_present(
            &mut client,
            reference.clone(),
            after,
            timeout_ms,
            max_bytes,
        )
        .await
        .map_err(|error| unavailable(&error))??;
        validate_frame(&reference, frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_frame_preserves_stateless_timeout() {
        let reference = TerminalExecutionRef {
            session_id: Uuid::nil(),
            pane_id: Uuid::nil(),
            execution_token: Uuid::nil(),
        };
        assert!(validate_frame(&reference, None).unwrap().is_none());
    }

    #[test]
    fn stale_source_is_not_adopted() {
        let reference = TerminalExecutionRef {
            session_id: Uuid::nil(),
            pane_id: Uuid::nil(),
            execution_token: Uuid::nil(),
        };
        let frame = LiveFrame {
            cursor: LiveCursor {
                execution: reference.clone(),
                source_epoch: Uuid::from_u128(1),
                source_reset_revision: 0,
                grid_revision: 0,
                geometry_revision: 0,
                projected_raw_watermark: 0,
                durable_raw_watermark: 0,
                target_exited: false,
                output_drained: false,
                terminal_session_ended: false,
                recovery_required: false,
            },
            screen: None,
        };
        assert!(validate_frame(&reference, Some(frame)).is_err());
    }
}
