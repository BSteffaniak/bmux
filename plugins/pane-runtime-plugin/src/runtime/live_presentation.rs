//! Explicitly negotiated live projection. Never used by durable presentation v1.
//!
//! Integration contract with execution: `live_revision` is cheap and observes grid
//! revision, geometry and projected raw watermark under the same geometry gate
//! as `live_screen`. `grid_revision` MUST equal `GridSnapshot::revision`. Notifications
//! follow projection, geometry, durable progress and lifecycle commits. Source
//! epoch is the execution token; `source_reset_revision` additionally fences
//! reconstructed/repaired parser state even if its grid revision is reused.
//!
//! No journal is invented here: until producer publication is integrated, every
//! changed grid is a bounded full repair snapshot. Consumers own retained scene
//! and commit displayed input modes only after successful write/flush.
use std::io::{self, Write};
use std::time::Duration;

use bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::{
    TerminalExecutionError, TerminalExecutionPhase, TerminalExecutionRef,
};
use bmux_pane_runtime_plugin_api::pane_runtime_live_presentation_v1::{
    LiveCapabilities, LiveCursor, LiveError, LiveFrame,
};

use super::execution;

pub(crate) fn capabilities() -> LiveCapabilities {
    LiveCapabilities {
        schema_version: 1,
        max_timeout_ms: 1000,
        max_response_bytes: 4 * 1024 * 1024,
    }
}

fn authority(error: TerminalExecutionError) -> LiveError {
    LiveError::Authority { error }
}

fn failed(reason: &impl ToString) -> LiveError {
    authority(TerminalExecutionError::OperationFailed {
        reason: reason.to_string(),
    })
}

fn validate(
    reference: &TerminalExecutionRef,
    after: Option<&LiveCursor>,
    timeout_ms: u32,
    max_bytes: u32,
) -> Result<(), LiveError> {
    let caps = capabilities();
    if timeout_ms > caps.max_timeout_ms || max_bytes == 0 || max_bytes > caps.max_response_bytes {
        return Err(LiveError::InvalidRequest {
            reason: "timeout <=1000ms; response budget 1..=4194304".into(),
        });
    }
    if after.is_some_and(|cursor| cursor.execution != *reference) {
        return Err(authority(TerminalExecutionError::StaleExecution));
    }
    Ok(())
}

fn cursor(reference: &TerminalExecutionRef) -> Result<LiveCursor, LiveError> {
    let revision = execution::live_revision(reference).map_err(authority)?;
    // Durable status is deliberately sampled separately. It is not the grid's
    // projected watermark, nor a claim that every displayed byte is persisted.
    let status = execution::status(reference).map_err(authority)?;
    Ok(LiveCursor {
        execution: reference.clone(),
        source_epoch: reference.execution_token,
        source_reset_revision: revision.reset_revision,
        grid_revision: revision.grid_revision,
        geometry_revision: revision.geometry_revision,
        projected_raw_watermark: revision.projected_watermark,
        durable_raw_watermark: status.output_watermark,
        target_exited: !matches!(status.phase, TerminalExecutionPhase::Running),
        output_drained: status.output_drained,
        terminal_session_ended: status.terminal_session_ended,
        recovery_required: revision.capture_failure.is_some() || status.error.is_some(),
    })
}

fn same_grid(a: &LiveCursor, b: &LiveCursor) -> bool {
    a.execution == b.execution
        && a.source_epoch == b.source_epoch
        && a.source_reset_revision == b.source_reset_revision
        && a.grid_revision == b.grid_revision
        && a.geometry_revision == b.geometry_revision
}

fn same_cursor(a: &LiveCursor, b: &LiveCursor) -> bool {
    same_grid(a, b)
        && a.projected_raw_watermark == b.projected_raw_watermark
        && a.durable_raw_watermark == b.durable_raw_watermark
        && a.target_exited == b.target_exited
        && a.output_drained == b.output_drained
        && a.terminal_session_ended == b.terminal_session_ended
        && a.recovery_required == b.recovery_required
}

fn read_current(
    reference: &TerminalExecutionRef,
    after: Option<&LiveCursor>,
    max_bytes: u32,
) -> Result<Option<LiveFrame>, LiveError> {
    read_current_with(
        after,
        max_bytes,
        || cursor(reference),
        || execution::live_screen(reference),
    )
}

fn read_current_with(
    after: Option<&LiveCursor>,
    max_bytes: u32,
    mut read_cursor: impl FnMut() -> Result<LiveCursor, LiveError>,
    mut read_screen: impl FnMut() -> Result<
        (
            bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalExecutionScreen,
            execution::LiveRevision,
        ),
        TerminalExecutionError,
    >,
) -> Result<Option<LiveFrame>, LiveError> {
    let before = read_cursor()?;
    if after.is_some_and(|previous| same_cursor(previous, &before)) {
        return Ok(None);
    }
    let (current, screen) = if after.is_some_and(|previous| same_grid(previous, &before)) {
        (before, None)
    } else {
        let (screen, revision) = read_screen().map_err(|error| match error {
            TerminalExecutionError::OperationFailed { ref reason }
                if reason == "screen exceeds response budget" =>
            {
                LiveError::Oversized { max_bytes }
            }
            error => authority(error),
        })?;
        // The immutable screen and grid cursor were captured together. Output
        // continuing during encoding is normal, not a reason to discard them.
        let mut current = read_cursor()?;
        current.source_reset_revision = revision.reset_revision;
        current.grid_revision = revision.grid_revision;
        current.geometry_revision = revision.geometry_revision;
        current.projected_raw_watermark = revision.projected_watermark;
        (current, Some(screen))
    };
    let frame = LiveFrame {
        cursor: current,
        screen,
    };
    let mut budget = ResponseBudget {
        remaining: max_bytes as usize,
    };
    if serde_json::to_writer(&mut budget, &frame).is_err() {
        return Err(LiveError::Oversized { max_bytes });
    }
    Ok(Some(frame))
}

/// Stateless read is wait with a zero deadline. No delivery acknowledgment or
/// scene authority is retained on the server.
///
/// # Errors
/// Returns authorization, coherence, budget or runtime-bridge failures.
pub(crate) fn read(
    reference: &TerminalExecutionRef,
    after: Option<&LiveCursor>,
    max_bytes: u32,
    authorize: impl Fn() -> Result<(), TerminalExecutionError> + Sync,
) -> Result<Option<LiveFrame>, LiveError> {
    wait_present(reference, after, 0, max_bytes, authorize)
}

/// Wait on the persistent pane owner runtime, never a transient request runtime.
///
/// # Errors
/// Returns authorization, invalid cursor/budget, incoherent capture, oversized
/// response, subscription or owner-runtime failures. Busy is safely retryable.
pub(crate) fn wait_present(
    reference: &TerminalExecutionRef,
    after: Option<&LiveCursor>,
    timeout_ms: u32,
    max_bytes: u32,
    authorize: impl Fn() -> Result<(), TerminalExecutionError> + Sync,
) -> Result<Option<LiveFrame>, LiveError> {
    validate(reference, after, timeout_ms, max_bytes)?;
    authorize().map_err(authority)?;
    execution::drive_live(wait_on_owner(
        reference, after, timeout_ms, max_bytes, &authorize,
    ))
    .map_err(authority)?
}

async fn wait_on_owner(
    reference: &TerminalExecutionRef,
    after: Option<&LiveCursor>,
    timeout_ms: u32,
    max_bytes: u32,
    authorize: &(impl Fn() -> Result<(), TerminalExecutionError> + Sync),
) -> Result<Option<LiveFrame>, LiveError> {
    // Subscribe before checking state; changes committed during reads remain
    // observable. Cancellation never advances server-side delivery state.
    let mut changes = execution::subscribe(reference).map_err(authority)?;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(u64::from(timeout_ms));
    loop {
        authorize().map_err(authority)?;
        if let Some(frame) = read_current(reference, after, max_bytes)? {
            authorize().map_err(authority)?;
            return Ok(Some(frame));
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
        match tokio::time::timeout_at(deadline, changes.changed()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(failed(&error)),
            Err(_) => {
                authorize().map_err(authority)?;
                return Ok(None);
            }
        }
    }
}

struct ResponseBudget {
    remaining: usize,
}
impl Write for ResponseBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.remaining = self
            .remaining
            .checked_sub(bytes.len())
            .ok_or_else(|| io::Error::other("live response exceeds budget"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference() -> TerminalExecutionRef {
        TerminalExecutionRef {
            session_id: uuid::Uuid::nil(),
            pane_id: uuid::Uuid::nil(),
            execution_token: uuid::Uuid::nil(),
        }
    }

    fn token() -> LiveCursor {
        LiveCursor {
            execution: reference(),
            source_epoch: uuid::Uuid::nil(),
            source_reset_revision: 0,
            grid_revision: 1,
            geometry_revision: 1,
            projected_raw_watermark: 5,
            durable_raw_watermark: 2,
            target_exited: false,
            output_drained: false,
            terminal_session_ended: false,
            recovery_required: false,
        }
    }

    #[test]
    fn protocol_revision_geometry_and_metadata_wake_without_hashing() {
        let before = token();
        let mut next = token();
        assert!(same_cursor(&before, &next));
        next.grid_revision += 1; // protocol-only change is a grid revision
        assert!(!same_grid(&before, &next));
        next = token();
        next.geometry_revision += 1;
        assert!(!same_grid(&before, &next));
        next = token();
        next.source_reset_revision += 1;
        assert!(!same_grid(&before, &next));
        assert!(!same_cursor(&before, &next));
        next = token();
        next.durable_raw_watermark += 1;
        assert!(same_grid(&before, &next));
        assert!(!same_cursor(&before, &next));
        next = token();
        next.recovery_required = true;
        assert!(!same_cursor(&before, &next));
    }

    #[test]
    fn source_change_requires_snapshot_and_execution_change_is_rejected() {
        let before = token();
        let mut next = token();
        next.source_epoch = uuid::Uuid::from_u128(1);
        assert!(!same_grid(&before, &next));
        next.execution.execution_token = uuid::Uuid::from_u128(2);
        assert!(validate(&reference(), Some(&next), 0, 4096).is_err());
        assert!(validate(&reference(), None, 1001, 4096).is_err());
        assert!(validate(&reference(), None, 0, 0).is_err());
    }

    #[test]
    fn continuous_output_during_encoding_returns_captured_frame_without_retry() {
        use bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalExecutionScreen;
        let calls = std::cell::Cell::new(0_u64);
        let frame = read_current_with(
            None,
            65536,
            || {
                let mut cursor = token();
                calls.set(calls.get() + 1);
                cursor.grid_revision = calls.get() * 10;
                cursor.projected_raw_watermark = calls.get() * 100;
                Ok(cursor)
            },
            || {
                Ok((
                    TerminalExecutionScreen {
                        execution: reference(),
                        output_watermark: 150,
                        cols: 1,
                        rows: 1,
                        cursor_col: 0,
                        cursor_row: 0,
                        cursor_visible: true,
                        text: "x".into(),
                        encoded_grid: vec![],
                    },
                    execution::LiveRevision {
                        grid_revision: 15,
                        reset_revision: 3,
                        geometry_revision: 4,
                        projected_watermark: 150,
                        durable_watermark: 100,
                        capture_failure: None,
                    },
                ))
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(calls.get(), 2);
        assert_eq!(frame.cursor.grid_revision, 15);
        assert_eq!(frame.cursor.projected_raw_watermark, 150);
        assert_eq!(frame.cursor.source_reset_revision, 3);
        assert_eq!(frame.cursor.geometry_revision, 4);
        assert_eq!(frame.screen.unwrap().output_watermark, 150);
    }

    #[test]
    fn unchanged_read_never_captures_or_encodes_a_screen() {
        let cursor = token();
        assert!(
            read_current_with(
                Some(&cursor),
                65536,
                || Ok(cursor.clone()),
                || panic!("unchanged read captured a screen")
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn response_counting_is_bounded_without_allocating_an_encoded_copy() {
        let mut budget = ResponseBudget { remaining: 3 };
        assert_eq!(budget.write(b"abc").unwrap(), 3);
        assert!(budget.write(b"d").is_err());
    }
}
