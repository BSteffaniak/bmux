//! Explicit v3 native terminal-session operations; durable authority is separate.
use super::execution;
use bmux_pane_runtime_plugin_api::{
    pane_runtime_execution_v1 as v1, pane_runtime_execution_v3 as v3,
};
use std::sync::atomic::Ordering;

fn legacy(reference: &v3::TerminalExecutionRef) -> v1::ExecutionRef {
    v1::ExecutionRef {
        session_id: reference.session_id,
        pane_id: reference.pane_id,
        execution_token: reference.execution_token,
    }
}
pub(crate) fn error(error: v1::ExecutionError) -> v3::TerminalExecutionError {
    match error {
        v1::ExecutionError::Unavailable => v3::TerminalExecutionError::Unavailable,
        v1::ExecutionError::NotFound => v3::TerminalExecutionError::NotFound,
        v1::ExecutionError::StaleExecution => v3::TerminalExecutionError::StaleExecution,
        v1::ExecutionError::Denied { reason } => v3::TerminalExecutionError::Denied { reason },
        v1::ExecutionError::InvalidOptions { reason } => {
            v3::TerminalExecutionError::InvalidOptions { reason }
        }
        v1::ExecutionError::LaunchFailed { reason } => {
            v3::TerminalExecutionError::LaunchFailed { reason }
        }
        v1::ExecutionError::OperationFailed { reason } => {
            v3::TerminalExecutionError::OperationFailed { reason }
        }
    }
}
pub(crate) fn authorize(
    ctx: &bmux_plugin_sdk::NativeServiceContext,
    reference: &v3::TerminalExecutionRef,
    action: &str,
) -> Result<(), v3::TerminalExecutionError> {
    crate::handlers::execution::authorize(ctx, &legacy(reference), action).map_err(error)
}
pub(crate) fn create(
    name: Option<String>,
    options: v3::TerminalProgramOptions,
    lifecycle: v3::TerminalLifecycle,
    authorize: impl Fn(uuid::Uuid) -> anyhow::Result<()> + Send + Sync + 'static,
) -> Result<v3::TerminalExecutionRef, v3::TerminalExecutionError> {
    match lifecycle {
        v3::TerminalLifecycle::ConventionalTerminalSession => {}
    }
    let reference = execution::new_program_session_mode(
        name,
        v1::ProgramOptions {
            program: options.program,
            args: options.args,
            cwd: options.cwd,
            env: options.env,
            cols: options.cols,
            rows: options.rows,
        },
        true,
        authorize,
    )
    .map_err(error)?;
    Ok(v3::TerminalExecutionRef {
        session_id: reference.session_id,
        pane_id: reference.pane_id,
        execution_token: reference.execution_token,
    })
}
fn validate(reference: &v3::TerminalExecutionRef) -> Result<(), v3::TerminalExecutionError> {
    execution::with_pane(&legacy(reference), |pane| {
        if !pane.execution.conventional_session.load(Ordering::SeqCst) {
            return Err(v1::ExecutionError::OperationFailed {
                reason: "execution is not a negotiated conventional terminal session".into(),
            });
        }
        Ok(())
    })
    .map_err(error)
}
pub(crate) fn status(
    reference: &v3::TerminalExecutionRef,
) -> Result<v3::TerminalExecutionStatus, v3::TerminalExecutionError> {
    validate(reference)?;
    let current = execution::status(&legacy(reference)).map_err(error)?;
    let cleanup_complete = execution::with_pane(&legacy(reference), |pane| {
        Ok(pane.execution.group_kill_issued.load(Ordering::SeqCst)
            && !pane.execution.cleanup_failed.load(Ordering::SeqCst)
            && matches!(&*pane.cleanup.borrow(), Some(Ok(()))))
    })
    .map_err(error)?;
    let failed = current.error.is_some();
    let ended = execution::with_pane(&legacy(reference), |pane| {
        Ok(pane.execution.terminal_session_ended.load(Ordering::SeqCst))
    })
    .map_err(error)?;
    Ok(v3::TerminalExecutionStatus {
        execution: reference.clone(),
        lifecycle: v3::TerminalLifecycle::ConventionalTerminalSession,
        phase: match current.phase {
            v1::ExecutionPhase::Running => v3::TerminalExecutionPhase::Running,
            v1::ExecutionPhase::Exited => v3::TerminalExecutionPhase::Exited,
            v1::ExecutionPhase::Failed => v3::TerminalExecutionPhase::Failed,
        },
        session_end: if failed {
            v3::TerminalSessionEnd::RecoveryRequired
        } else if ended {
            v3::TerminalSessionEnd::Ended
        } else if current.exit_code.is_some() || current.signal.is_some() {
            v3::TerminalSessionEnd::Draining
        } else {
            v3::TerminalSessionEnd::Active
        },
        terminal_session_ended: ended,
        capture_integrity: if failed {
            v3::TerminalCaptureIntegrity::Incomplete
        } else if current.output_drained {
            v3::TerminalCaptureIntegrity::Complete
        } else {
            v3::TerminalCaptureIntegrity::Capturing
        },
        exit_code: current.exit_code,
        signal: current.signal,
        output_drained: current.output_drained,
        output_watermark: current.output_watermark,
        cleanup_complete,
        error: current.error,
    })
}
pub(crate) fn screen(
    reference: &v3::TerminalExecutionRef,
) -> Result<v3::TerminalExecutionScreen, v3::TerminalExecutionError> {
    validate(reference)?;
    let screen = execution::screen(&legacy(reference)).map_err(error)?;
    Ok(v3::TerminalExecutionScreen {
        execution: reference.clone(),
        output_watermark: screen.output_watermark,
        cols: screen.cols,
        rows: screen.rows,
        cursor_col: screen.cursor_col,
        cursor_row: screen.cursor_row,
        cursor_visible: screen.cursor_visible,
        text: screen.text,
        encoded_grid: screen.encoded_grid,
    })
}
pub(crate) fn send(
    reference: &v3::TerminalExecutionRef,
    bytes: Vec<u8>,
) -> Result<(), v3::TerminalExecutionError> {
    validate(reference)?;
    execution::send(&legacy(reference), bytes).map_err(error)
}
pub(crate) fn resize(
    reference: &v3::TerminalExecutionRef,
    cols: u16,
    rows: u16,
) -> Result<(), v3::TerminalExecutionError> {
    validate(reference)?;
    execution::resize(&legacy(reference), cols, rows).map_err(error)
}
pub(crate) fn wait(
    reference: &v3::TerminalExecutionRef,
    timeout: u32,
    authorize: impl Fn() -> Result<(), v3::TerminalExecutionError> + Sync,
) -> Result<v3::TerminalExecutionStatus, v3::TerminalExecutionError> {
    validate(reference)?;
    if timeout > 30_000 {
        return Err(v3::TerminalExecutionError::InvalidOptions {
            reason: "wait timeout exceeds 30000ms".into(),
        });
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(u64::from(timeout));
    loop {
        authorize()?;
        let current = status(reference)?;
        if current.output_drained
            || current.error.is_some()
            || std::time::Instant::now() >= deadline
        {
            return Ok(current);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

pub(crate) fn terminate(
    reference: &v3::TerminalExecutionRef,
    grace_ms: u32,
    ctx: &bmux_plugin_sdk::NativeServiceContext,
) -> Result<v3::TerminalExecutionStatus, v3::TerminalExecutionError> {
    validate(reference)?;
    let old = legacy(reference);
    execution::terminate(&old, grace_ms, || {
        crate::handlers::execution::authorize(ctx, &old, "pane.close")
    })
    .map_err(error)?;
    status(reference)
}
