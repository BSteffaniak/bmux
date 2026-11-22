//! Exact, generation-guarded program execution on the existing PTY runtime.
use super::{PaneRuntimeCommand, PaneRuntimeHandle};
use bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::{
    TerminalExecutionError as ExecutionError, TerminalExecutionPhase as ExecutionPhase,
    TerminalExecutionRef as ExecutionRef, TerminalExecutionScreen as ExecutionScreen,
    TerminalExecutionStatus as ExecutionStatus, TerminalProgramOptions as ProgramOptions,
};
use bmux_session_models::SessionId;
use std::time::Duration;
use switchy_thread::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio::sync::Semaphore;

use crate::platform::{self, StopAction};

type SpawnAuthorization = Arc<dyn Fn() -> anyhow::Result<()> + Send + Sync>;

pub(super) struct ExecutionCapture {
    /// The pane's terminal session, once its leader has been spawned. Its lock is
    /// the control fence: leader exit, stops and retirement serialize on it.
    pub(super) session: Mutex<Option<Arc<super::session_control::SessionControl>>>,
    pub(super) spawn_authorization: Mutex<Option<SpawnAuthorization>>,
    pub(super) exact_environment: Mutex<
        Option<Vec<bmux_pane_runtime_plugin_api::pane_runtime_environment_v1::EnvironmentEntry>>,
    >,
    pub(super) group_kill_issued: AtomicBool,
    pub(super) target_stop_issued: AtomicBool,
    pub(super) cleanup_failed: AtomicBool,
    // Cleanup has its own authority: an earlier reader/process error in outcome
    // must not hide the reason resource release failed.
    cleanup_error: Mutex<Option<String>>,
    pub(super) managed_program: AtomicBool,
    pub(super) terminal_session_ended: AtomicBool,
    pub(super) terminal_owner_retired: AtomicBool,
    #[cfg(test)]
    pub(super) inject_reader_failure: AtomicBool,
    pub(super) capture_order: Mutex<()>,
    projection_progress: Mutex<u64>,
    projection_changed: switchy_thread::sync::Condvar,
    pub(super) durable_capture: Mutex<Option<super::durable_capture::DurableCapture>>,
    pub(super) capture_writer: Mutex<Option<super::capture_writer::CaptureWriter>>,
    pub(super) master: Mutex<Option<bmux_pty::PtyMaster>>,
    pub(super) output_drained: AtomicBool,
    pub(super) geometry_revision: AtomicU64,
    pub(super) output_watermark: AtomicU64,
    pub(super) outcome: Mutex<(Option<u32>, Option<String>, Option<String>)>,
    changes: switchy_async::sync::watch::Sender<()>,
    admission: Arc<Semaphore>,
}

impl Default for ExecutionCapture {
    fn default() -> Self {
        Self {
            session: Mutex::new(None),
            spawn_authorization: Mutex::new(None),
            exact_environment: Mutex::new(None),
            group_kill_issued: AtomicBool::new(false),
            target_stop_issued: AtomicBool::new(false),
            cleanup_failed: AtomicBool::new(false),
            cleanup_error: Mutex::new(None),
            managed_program: AtomicBool::new(false),
            terminal_session_ended: AtomicBool::new(false),
            terminal_owner_retired: AtomicBool::new(false),
            #[cfg(test)]
            inject_reader_failure: AtomicBool::new(false),
            capture_order: Mutex::new(()),
            projection_progress: Mutex::new(0),
            projection_changed: switchy_thread::sync::Condvar::new(),
            durable_capture: Mutex::new(None),
            capture_writer: Mutex::new(None),
            master: Mutex::new(None),
            output_drained: AtomicBool::new(false),
            geometry_revision: AtomicU64::new(0),
            output_watermark: AtomicU64::new(0),
            outcome: Mutex::new((None, None, None)),
            changes: switchy_async::sync::watch::channel(()).0,
            admission: Arc::new(Semaphore::new(64)),
        }
    }
}

impl ExecutionCapture {
    /// Apply only the currently published legacy request. Hold the geometry
    /// lock through the ioctl so synchronous view sizing cannot be overwritten
    /// by an older queued command after it has acknowledged success.
    pub(super) fn resize_if_current(
        &self,
        size: &Mutex<(u16, u16)>,
        rows: u16,
        cols: u16,
    ) -> std::io::Result<bool> {
        let current = size
            .lock()
            .map_err(|_| std::io::Error::other("geometry lock poisoned"))?;
        if *current != (rows, cols) {
            return Ok(false);
        }
        self.master
            .lock()
            .map_err(|_| std::io::Error::other("PTY master lock poisoned"))?
            .as_ref()
            .ok_or_else(|| std::io::Error::other("PTY master unavailable"))?
            .resize(bmux_pty::PtySize { rows, cols })
            .map_err(std::io::Error::other)?;
        Ok(true)
    }

    pub(super) fn writer(&self) -> std::io::Result<Option<super::capture_writer::CaptureWriter>> {
        self.capture_writer
            .lock()
            .map(|writer| writer.clone())
            .map_err(|_| std::io::Error::other("capture writer authority poisoned"))
    }

    pub(super) fn capture_failure(&self) -> Option<String> {
        match self.writer() {
            Ok(Some(writer)) => match writer.progress() {
                Ok(progress) => progress.failure.or(progress.boundary.error),
                Err(error) => Some(error.to_string()),
            },
            Ok(None) => None,
            Err(error) => Some(error.to_string()),
        }
    }

    pub(super) fn fail_capture(&self, reason: &str) {
        if let Ok(Some(writer)) = self.writer() {
            writer.fail(reason);
        }
        self.record_error(reason.to_string());
    }

    pub(super) fn durable_capture_complete(&self) -> bool {
        match self.writer() {
            Ok(Some(writer)) => writer.progress().is_ok_and(|progress| {
                progress.boundary.eof
                    && progress.boundary.error.is_none()
                    && progress.failure.is_none()
            }),
            Ok(None) => self.output_drained.load(Ordering::SeqCst),
            Err(_) => false,
        }
    }

    pub(super) fn durable_watermark(&self) -> u64 {
        match self.writer() {
            Ok(Some(writer)) => writer.progress().map_or(0, |progress| {
                progress
                    .boundary
                    .bytes
                    .min(self.output_watermark.load(Ordering::SeqCst))
            }),
            Ok(None) => self.output_watermark.load(Ordering::SeqCst),
            Err(_) => 0,
        }
    }

    // Resize freezes capture publication, then waits without holding parser locks.
    // The bounded capture queue can drain while no new bytes enter its boundary.
    pub(super) fn wait_for_projection(&self, boundary: u64) -> std::io::Result<()> {
        let progress = self
            .projection_progress
            .lock()
            .map_err(|_| std::io::Error::other("projection progress lock poisoned"))?;
        let (progress, _) = self
            .projection_changed
            .wait_timeout_while(progress, std::time::Duration::from_secs(2), |progress| {
                *progress < boundary
            })
            .map_err(|_| std::io::Error::other("projection progress lock poisoned"))?;
        if *progress < boundary {
            return Err(std::io::Error::other(
                "output projection did not reach resize boundary",
            ));
        }
        Ok(())
    }

    pub(super) fn commit_projection(&self) -> std::io::Result<()> {
        let mut progress = self
            .projection_progress
            .lock()
            .map_err(|_| std::io::Error::other("projection progress lock poisoned"))?;
        *progress = self.output_watermark.load(Ordering::SeqCst);
        self.projection_changed.notify_all();
        Ok(())
    }

    pub(super) fn subscribe(&self) -> switchy_async::sync::watch::Receiver<()> {
        self.changes.subscribe()
    }

    pub(super) fn notify(&self) {
        self.changes.send_replace(());
    }
    pub(super) fn authorize_spawn(&self) -> anyhow::Result<()> {
        let authorization = self
            .spawn_authorization
            .lock()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?
            .clone();
        if let Some(authorize) = authorization {
            authorize()?;
        }
        Ok(())
    }

    pub(super) fn cleanup_complete(&self, resource_cleanup_acknowledged: bool) -> bool {
        // Resource release and owner retirement are independent acknowledgements.
        // Neither a kill request nor capture EOF substitutes for either receipt.
        resource_cleanup_acknowledged
            && self.terminal_owner_retired.load(Ordering::SeqCst)
            && !self.cleanup_failed.load(Ordering::SeqCst)
    }

    /// Release the session's native authority only after the session ended.
    /// Output capture remains pending until the reader observes EOF.
    pub(super) fn finish_owner_retirement(
        &self,
        end_lifetime: impl FnOnce() -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.terminal_session_ended.load(Ordering::SeqCst),
            "cannot retire an active terminal scope"
        );
        end_lifetime()?;
        self.terminal_owner_retired.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Record that the session leader has exited. Must run before the leader is
    /// reaped: from then on its pid is never signalled.
    pub(super) fn mark_leader_exited(&self) {
        if let Ok(session) = self.session.lock()
            && let Some(control) = session.as_ref()
        {
            control.mark_leader_exited();
        }
    }

    /// Settle the session after its leader was reaped: sweep what is left when
    /// `sweep` is set, then end and retire it. Without `sweep`, a session that
    /// still has members is left running and retired later by
    /// [`Self::retire_released_session`] or swept by a forced stop.
    pub(super) fn settle_after_leader_exit(&self, sweep: bool) {
        let Ok(session) = self.session.lock() else {
            self.record_cleanup_error("terminal session lock poisoned".into());
            return;
        };
        let Some(control) = session.as_ref() else {
            return;
        };
        if self.terminal_owner_retired.load(Ordering::SeqCst) {
            return;
        }
        if sweep {
            if let Err(error) = control.force_stop() {
                self.record_cleanup_error(format!("terminal session cleanup failed: {error}"));
            }
        } else {
            match control.occupied() {
                Ok(false) => {
                    tracing::debug!("terminal session empty after leader exit; retiring");
                }
                Ok(true) => {
                    tracing::debug!("terminal session members left running after leader exit");
                    control.release_survivors();
                    return;
                }
                Err(error) => {
                    self.record_cleanup_error(format!(
                        "terminal session inspection failed: {error}"
                    ));
                }
            }
        }
        self.terminal_session_ended.store(true, Ordering::SeqCst);
        if let Err(error) = self.finish_owner_retirement(|| control.end_lifetime()) {
            self.record_cleanup_error(format!("terminal session release failed: {error}"));
        }
        self.notify();
    }

    /// Retire a session whose leader exited while members were deliberately
    /// left running (`sweep_on_exit = false`), once the pane no longer needs it.
    pub(super) fn retire_released_session(&self) {
        let Ok(session) = self.session.lock() else {
            self.record_cleanup_error("terminal session lock poisoned".into());
            return;
        };
        let Some(control) = session.as_ref() else {
            return;
        };
        if self.terminal_owner_retired.load(Ordering::SeqCst) || !control.leader_exited() {
            return;
        }
        tracing::debug!("terminal released by left-running members; retiring session");
        self.terminal_session_ended.store(true, Ordering::SeqCst);
        if let Err(error) = self.finish_owner_retirement(|| control.end_lifetime()) {
            self.record_cleanup_error(format!("terminal session release failed: {error}"));
        }
        self.notify();
    }

    /// End the pane's lifetime: kill whatever still runs in its session unless
    /// the session already ended or was already swept, then release it.
    pub(super) fn release_session(&self) -> anyhow::Result<()> {
        let session = self
            .session
            .lock()
            .map_err(|_| anyhow::anyhow!("terminal session lock poisoned"))?;
        let Some(control) = session.as_ref() else {
            return Ok(());
        };
        if !self.terminal_session_ended.load(Ordering::SeqCst)
            && !self.terminal_owner_retired.load(Ordering::SeqCst)
            && !self.group_kill_issued.load(Ordering::SeqCst)
        {
            control.force_stop()?;
            self.group_kill_issued.store(true, Ordering::SeqCst);
            self.target_stop_issued.store(true, Ordering::SeqCst);
            if control.leader_exited() {
                self.terminal_session_ended.store(true, Ordering::SeqCst);
                return self.finish_owner_retirement(|| control.end_lifetime());
            }
        }
        control.end_lifetime()
    }

    /// Stop the pane's program. A forced stop kills the whole session and
    /// verifies it is empty; a cooperative one asks the leader to exit.
    pub(super) fn signal_owned_group(
        &self,
        host: &crate::runtime::PaneRuntimeHost,
        action: StopAction,
    ) -> anyhow::Result<bool> {
        // The session lock is the control fence shared with leader exit and
        // retirement: a retired session is never signalled again.
        let session = self
            .session
            .lock()
            .map_err(|_| anyhow::anyhow!("terminal session lock poisoned"))?;
        if self.terminal_session_ended.load(Ordering::SeqCst)
            || self.terminal_owner_retired.load(Ordering::SeqCst)
        {
            return Ok(false);
        }
        if let Some(control) = session.as_ref() {
            return match action {
                StopAction::Cooperative => {
                    if self.group_kill_issued.load(Ordering::SeqCst)
                        || !platform::supports_cooperative_stop()
                    {
                        // Unsupported soft termination is not a cleanup failure;
                        // the bounded caller escalates to a forced stop.
                        return Ok(false);
                    }
                    let signalled = control.terminate_leader()?;
                    if signalled {
                        self.target_stop_issued.store(true, Ordering::SeqCst);
                    }
                    Ok(signalled)
                }
                StopAction::Force => {
                    if self.group_kill_issued.load(Ordering::SeqCst) {
                        return Ok(false);
                    }
                    control.force_stop()?;
                    self.group_kill_issued.store(true, Ordering::SeqCst);
                    self.target_stop_issued.store(true, Ordering::SeqCst);
                    if let Err(error) = host.shutdown_progress().advance("terminal session swept") {
                        tracing::warn!(%error, "shutdown progress publication failed");
                    }
                    if control.leader_exited() {
                        // The leader was already reaped; the sweep just ended
                        // the members it had left running.
                        self.terminal_session_ended.store(true, Ordering::SeqCst);
                        self.finish_owner_retirement(|| control.end_lifetime())?;
                    }
                    self.notify();
                    Ok(true)
                }
            };
        }
        drop(session);
        if action == StopAction::Force && self.group_kill_issued.load(Ordering::SeqCst) {
            return Ok(false);
        }
        let master = self
            .master
            .lock()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let Some(master) = master.as_ref() else {
            if self.managed_program.load(Ordering::SeqCst) {
                anyhow::bail!(
                    "managed execution PTY authority released; descendant cleanup requires recovery"
                );
            }
            return Ok(false);
        };
        // Before the session is adopted, only the live PTY supplies authority.
        // A saved PID or PGID is never authority to signal a recycled group.
        let Some(signaled) = platform::stop_foreground_group(master, action)? else {
            if self.output_drained.load(Ordering::SeqCst)
                && !self.managed_program.load(Ordering::SeqCst)
            {
                return Ok(false);
            }
            anyhow::bail!(
                "PTY foreground process-group authority unavailable; descendant cleanup requires recovery"
            );
        };
        if signaled && action == StopAction::Force {
            self.group_kill_issued.store(true, Ordering::SeqCst);
        }
        Ok(signaled)
    }

    pub(super) fn record_exit(&self, result: &std::io::Result<bmux_pty::ChildExit>) {
        if let Ok(mut outcome) = self.outcome.lock() {
            match result {
                Ok(status) => {
                    outcome.0 = Some(status.exit_code());
                    // Report signals by number, as the execution API always has.
                    outcome.1 = status.signal_number().map_or_else(
                        || status.signal().map(ToString::to_string),
                        |number| Some(number.to_string()),
                    );
                }
                Err(error) => outcome.2 = Some(format!("process wait failed: {error}")),
            }
        }
        self.notify();
    }
    pub(super) fn record_cleanup_error(&self, error: String) {
        if let Ok(mut first) = self.cleanup_error.lock()
            && first.is_none()
        {
            tracing::warn!(%error, "execution cleanup failed");
            *first = Some(error.clone());
        }
        self.cleanup_failed.store(true, Ordering::SeqCst);
        self.record_error(error);
    }

    pub(super) fn cleanup_error(&self) -> Option<String> {
        if !self.cleanup_failed.load(Ordering::SeqCst) {
            return None;
        }
        Some(
            self.cleanup_error
                .lock()
                .ok()
                .and_then(|error| error.clone())
                .unwrap_or_else(|| "cleanup error detail unavailable".into()),
        )
    }

    pub(super) fn record_error(&self, error: String) {
        if let Ok(mut outcome) = self.outcome.lock() {
            outcome.2.get_or_insert(error);
        }
        self.notify();
    }
}

pub(crate) fn managed_program_ids(
    host: &crate::runtime::PaneRuntimeHost,
) -> anyhow::Result<std::collections::BTreeSet<uuid::Uuid>> {
    let handle = host
        .spawn_runtime_handle()
        .ok_or_else(|| anyhow::anyhow!("execution runtime unavailable"))?;
    let manager = handle
        .0
        .lock()
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    Ok(manager
        .runtimes
        .values()
        .flat_map(|runtime| runtime.panes.values())
        .filter(|pane| pane.execution.managed_program.load(Ordering::SeqCst))
        .map(|pane| pane.meta.id)
        .collect())
}

pub(super) fn failed(error: impl std::fmt::Display) -> ExecutionError {
    ExecutionError::OperationFailed {
        reason: error.to_string(),
    }
}

fn validate_token(actual: uuid::Uuid, reference: &ExecutionRef) -> Result<(), ExecutionError> {
    if actual != reference.execution_token {
        return Err(ExecutionError::StaleExecution);
    }
    Ok(())
}

pub(super) fn with_pane<T>(
    host: &crate::runtime::PaneRuntimeHost,
    reference: &ExecutionRef,
    action: impl FnOnce(&PaneRuntimeHandle) -> Result<T, ExecutionError>,
) -> Result<T, ExecutionError> {
    let handle = host
        .spawn_runtime_handle()
        .ok_or(ExecutionError::Unavailable)?;
    let manager = handle.0.lock().map_err(failed)?;
    let pane = manager
        .runtimes
        .get(&SessionId(reference.session_id))
        .and_then(|runtime| runtime.panes.get(&reference.pane_id))
        .ok_or(ExecutionError::NotFound)?;
    let token = pane
        .resurrection_state
        .lock()
        .map_err(failed)?
        .observation_execution;
    validate_token(token, reference)?;
    action(pane)
}

pub(crate) fn status(
    host: &crate::runtime::PaneRuntimeHost,
    reference: &ExecutionRef,
) -> Result<ExecutionStatus, ExecutionError> {
    with_pane(host, reference, |pane| {
        let outcome = pane.execution.outcome.lock().map_err(failed)?;
        let error = outcome
            .2
            .clone()
            .or_else(|| pane.execution.capture_failure());
        tracing::debug!(
            exit_code = ?outcome.0,
            signal = ?outcome.1,
            exited = pane.exited.load(Ordering::SeqCst),
            output_drained = pane.execution.output_drained.load(Ordering::SeqCst),
            output_watermark = pane.execution.output_watermark.load(Ordering::SeqCst),
            "execution status snapshot"
        );
        Ok(ExecutionStatus {
            lifecycle: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalLifecycle::ConventionalTerminalSession,
            session_end: if pane.execution.terminal_session_ended.load(Ordering::SeqCst) {
                bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalSessionEnd::Ended
            } else if error.is_some() {
                bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalSessionEnd::RecoveryRequired
            } else if pane.exited.load(Ordering::SeqCst) {
                bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalSessionEnd::Draining
            } else { bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalSessionEnd::Active },
            terminal_session_ended: pane.execution.terminal_session_ended.load(Ordering::SeqCst),
            capture_integrity: if pane.execution.capture_failure().is_some() {
                bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalCaptureIntegrity::Incomplete
            } else if pane.execution.durable_capture_complete() && pane.execution.output_drained.load(Ordering::SeqCst) {
                bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalCaptureIntegrity::Complete
            } else if outcome.2.is_some() { bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalCaptureIntegrity::Incomplete
            } else { bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalCaptureIntegrity::Capturing },
            cleanup_complete: pane.execution.cleanup_complete(matches!(
                &*pane.cleanup.borrow(),
                Some(Ok(()))
            )),
            execution: reference.clone(),
            phase: if error.is_some() {
                ExecutionPhase::Failed
            } else if pane.exited.load(Ordering::SeqCst) {
                ExecutionPhase::Exited
            } else {
                ExecutionPhase::Running
            },
            exit_code: if outcome.1.is_none() { outcome.0 } else { None },
            signal: outcome.1.clone(),
            error,
            output_drained: pane.execution.output_drained.load(Ordering::SeqCst) && pane.execution.durable_capture_complete(),
            output_watermark: pane.execution.durable_watermark(),
        })
    })
}

pub(crate) fn screen(
    host: &crate::runtime::PaneRuntimeHost,
    reference: &ExecutionRef,
) -> Result<ExecutionScreen, ExecutionError> {
    screen_mode(host, reference, true)
}

/// Only the explicitly negotiated live presentation interface may call this.
pub(crate) fn live_screen(
    host: &crate::runtime::PaneRuntimeHost,
    reference: &ExecutionRef,
) -> Result<(ExecutionScreen, LiveRevision), ExecutionError> {
    capture_screen(host, reference, false)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LiveRevision {
    pub(crate) grid_revision: u64,
    pub(crate) reset_revision: u64,
    pub(crate) geometry_revision: u64,
    pub(crate) projected_watermark: u64,
    pub(crate) durable_watermark: u64,
    pub(crate) capture_failure: Option<String>,
}

pub(crate) fn live_revision(
    host: &crate::runtime::PaneRuntimeHost,
    reference: &ExecutionRef,
) -> Result<LiveRevision, ExecutionError> {
    with_pane(host, reference, |pane| {
        let _size = pane.last_requested_size.lock().map_err(failed)?;
        let grid = pane.terminal_grid.lock().map_err(failed)?;
        let durable_watermark = match pane.execution.writer().map_err(failed)? {
            Some(writer) => writer.progress().map_err(failed)?.boundary.bytes,
            None => pane.execution.output_watermark.load(Ordering::SeqCst),
        };
        Ok(LiveRevision {
            grid_revision: grid.grid().revision(),
            reset_revision: grid.grid().reset_revision(),
            geometry_revision: pane.execution.geometry_revision.load(Ordering::SeqCst),
            projected_watermark: pane.execution.output_watermark.load(Ordering::SeqCst),
            durable_watermark,
            capture_failure: pane.execution.capture_failure(),
        })
    })
}

pub(crate) fn subscribe(
    host: &crate::runtime::PaneRuntimeHost,
    reference: &ExecutionRef,
) -> Result<switchy_async::sync::watch::Receiver<()>, ExecutionError> {
    with_pane(host, reference, |pane| Ok(pane.execution.subscribe()))
}

fn screen_mode(
    host: &crate::runtime::PaneRuntimeHost,
    reference: &ExecutionRef,
    durable: bool,
) -> Result<ExecutionScreen, ExecutionError> {
    capture_screen(host, reference, durable).map(|(screen, _)| screen)
}

fn capture_screen(
    host: &crate::runtime::PaneRuntimeHost,
    reference: &ExecutionRef,
    durable: bool,
) -> Result<(ExecutionScreen, LiveRevision), ExecutionError> {
    let (snapshot, text, revision, barrier) = with_pane(host, reference, |pane| {
        // Capture immutable grid and its cursor in ONE geometry/grid transaction.
        // Encoding after unlocking cannot invalidate this captured publication.
        let _size = pane.last_requested_size.lock().map_err(failed)?;
        let stream = pane.terminal_grid.lock().map_err(failed)?;
        let writer = pane.execution.writer().map_err(failed)?;
        let progress = writer
            .as_ref()
            .map(super::capture_writer::CaptureWriter::progress)
            .transpose()
            .map_err(failed)?;
        let projected_watermark = pane.execution.output_watermark.load(Ordering::SeqCst);
        let grid = stream.grid();
        let revision = LiveRevision {
            grid_revision: grid.revision(),
            reset_revision: grid.reset_revision(),
            geometry_revision: pane.execution.geometry_revision.load(Ordering::SeqCst),
            projected_watermark,
            durable_watermark: progress
                .as_ref()
                .map_or(projected_watermark, |p| p.boundary.bytes),
            capture_failure: pane.execution.capture_failure(),
        };
        let barrier = if durable {
            writer
                .zip(progress)
                .map(|(writer, progress)| (writer, progress.projected))
        } else {
            None
        };
        let snapshot = stream
            .try_snapshot(0, grid.height(), super::RESPONSE_OUTPUT_BUDGET)
            .ok_or_else(|| failed("screen exceeds response budget"))?;
        let text = bmux_terminal_grid::visible_text(grid, 0, grid.height());
        Ok((snapshot, text, revision, barrier))
    })?;
    let encoded_grid = serde_json::to_vec(&snapshot).map_err(failed)?;
    if encoded_grid.len().saturating_add(text.len()) > super::RESPONSE_OUTPUT_BUDGET {
        return Err(failed("screen exceeds response budget"));
    }
    if let Some((writer, ticket)) = barrier {
        writer.wait_committed(ticket).map_err(failed)?;
    }
    Ok((
        ExecutionScreen {
            execution: reference.clone(),
            output_watermark: revision.projected_watermark,
            cols: snapshot.width,
            rows: snapshot.height,
            cursor_col: snapshot.cursor.col,
            cursor_row: snapshot.cursor.row,
            cursor_visible: snapshot.cursor.visible,
            text,
            encoded_grid,
        },
        revision,
    ))
}

pub(crate) fn validate_options(options: &ProgramOptions) -> Result<(), ExecutionError> {
    let invalid = |reason: &str| ExecutionError::InvalidOptions {
        reason: reason.into(),
    };
    if options.program.is_empty()
        || options.program.contains('\0')
        || options.args.iter().any(|arg| arg.contains('\0'))
    {
        return Err(invalid(
            "program and arguments must be non-NUL; program must not be empty",
        ));
    }
    validate_size(options.cols, options.rows)?;
    if options
        .env
        .iter()
        .any(|(key, value)| key.is_empty() || key.contains(['=', '\0']) || value.contains('\0'))
    {
        return Err(invalid("invalid environment entry"));
    }
    if let Some(cwd) = &options.cwd {
        let path = std::path::Path::new(cwd);
        if cwd.contains('\0') || !path.is_absolute() || !crate::files::is_dir(path) {
            return Err(invalid(
                "cwd must be an existing absolute execution-host directory",
            ));
        }
    }
    if options.env.iter().any(|(key, _)| {
        matches!(
            key.as_str(),
            "TERM"
                | "COLORTERM"
                | "TERM_PROGRAM"
                | "TERM_PROGRAM_VERSION"
                | "BMUX_PANE_ID"
                | "APPLICATION_RESURRECTION_HOST"
        )
    }) {
        return Err(invalid(
            "environment cannot override runtime-owned terminal or execution identity",
        ));
    }
    Ok(())
}

fn validate_size(cols: u16, rows: u16) -> Result<(), ExecutionError> {
    if cols == 0 || rows == 0 || cols > 1000 || rows > 1000 {
        return Err(ExecutionError::InvalidOptions {
            reason: "PTY dimensions must be 1..=1000".into(),
        });
    }
    Ok(())
}

pub(crate) fn create(
    host: &crate::runtime::PaneRuntimeHost,
    name: Option<String>,
    options: ProgramOptions,
    authorize: impl Fn(uuid::Uuid) -> anyhow::Result<()> + Send + Sync + 'static,
) -> Result<ExecutionRef, ExecutionError> {
    let authorize = Arc::new(authorize);
    // Authorization precedes observing the durable store/runtime installation.
    authorize(crate::ids::new_uuid()).map_err(|error| ExecutionError::Denied {
        reason: error.to_string(),
    })?;
    let reservation = super::reservations::reserve(host, crate::ids::new_uuid(), options, |id| {
        authorize(id).map_err(|error| {
            bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalRecoveryError::Denied {
                reason: error.to_string(),
            }
        })
    })
    .map_err(|error| match error {
        bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalRecoveryError::Denied { reason } => ExecutionError::Denied { reason },
        error => failed(format!("{error:?}")),
    })?;
    let launch_authorize = Arc::clone(&authorize);
    // Name is presentation only; durable admission owns the identity.
    let receipt =
        super::reservations::launch_named(host, &reservation, name, move |id| launch_authorize(id))
            .map_err(|error| failed(format!("{error:?}")))?;
    if receipt.error.is_some() {
        return Err(failed(format!(
            "launch requires reconciliation: {:?}",
            receipt.error
        )));
    }
    Ok(receipt.reservation.execution)
}

pub(super) fn create_reserved(
    host: &crate::runtime::PaneRuntimeHost,
    name: Option<String>,
    options: ProgramOptions,
    reserved: Option<&ExecutionRef>,
    authorize: impl Fn(uuid::Uuid) -> anyhow::Result<()> + Send + Sync + 'static,
) -> Result<ExecutionRef, ExecutionError> {
    create_reserved_environment(host, name, options, reserved, None, authorize)
}

fn install_environment(
    prepared: &super::PreparedFreshRuntime,
    environment: Option<
        Vec<bmux_pane_runtime_plugin_api::pane_runtime_environment_v1::EnvironmentEntry>,
    >,
) -> Result<(), ExecutionError> {
    if let Some(environment) = environment {
        for pane in prepared
            .runtime
            .as_ref()
            .expect("prepared runtime")
            .panes
            .values()
        {
            *pane.execution.exact_environment.lock().map_err(failed)? = Some(environment.clone());
        }
    }
    Ok(())
}

pub(super) fn create_reserved_environment(
    host: &crate::runtime::PaneRuntimeHost,
    name: Option<String>,
    options: ProgramOptions,
    reserved: Option<&ExecutionRef>,
    environment: Option<
        Vec<bmux_pane_runtime_plugin_api::pane_runtime_environment_v1::EnvironmentEntry>,
    >,
    authorize: impl Fn(uuid::Uuid) -> anyhow::Result<()> + Send + Sync + 'static,
) -> Result<ExecutionRef, ExecutionError> {
    let authorize = Arc::new(authorize);
    let id = reserved.map_or_else(
        || SessionId(crate::ids::new_uuid()),
        |reference| SessionId(reference.session_id),
    );
    let session = bmux_session_models::Session::with_id(id, name);
    authorize(id.0).map_err(|error| ExecutionError::Denied {
        reason: error.to_string(),
    })?;
    validate_options(&options)?;
    let handle = host
        .spawn_runtime_handle()
        .ok_or_else(|| failed("pane execution runtime is not registered"))?;
    let sessions = host
        .states()
        .get::<bmux_session_state::SessionManagerHandle>()
        .and_then(|entry| entry.read().ok().map(|guard| guard.clone()))
        .ok_or_else(|| failed("session manager is not registered for program creation"))?;
    let runtime = handle.1.clone();
    let result = drive_startup(&runtime, async {
        let mut prepared = handle
            .0
            .lock()
            .map_err(failed)?
            .prepare_reserved_program_runtime(
                id,
                None,
                Some(super::PaneLaunchSpec {
                    program: options.program,
                    args: options.args,
                    cwd: options.cwd,
                    env: options.env,
                }),
                (options.rows, options.cols),
                reserved,
            )
            .map_err(failed)?;
        install_reserved_capture(host, &prepared, reserved)?;
        install_environment(&prepared, environment)?;
        for pane in prepared
            .runtime
            .as_ref()
            .expect("prepared runtime")
            .panes
            .values()
        {
            let spawn_authorize = Arc::clone(&authorize);
            *pane.execution.spawn_authorization.lock().map_err(failed)? =
                Some(Arc::new(move || spawn_authorize(id.0)));
        }
        if let Err(error) = await_startup(&mut prepared).await {
            return Err(rollback_prepared(&mut prepared, error).await);
        }
        let reference = {
            let pane = prepared
                .runtime
                .as_ref()
                .expect("prepared runtime")
                .panes
                .values()
                .next()
                .expect("initial pane");
            ExecutionRef {
                session_id: id.0,
                pane_id: pane.meta.id,
                execution_token: pane
                    .resurrection_state
                    .lock()
                    .map_err(failed)?
                    .observation_execution,
            }
        };
        let commit = authorize(id.0)
            .map_err(|error| ExecutionError::Denied {
                reason: error.to_string(),
            })
            .and_then(|()| {
                handle
                    .0
                    .lock()
                    .map_err(failed)?
                    .commit_fresh_runtime(id, &mut prepared, || sessions.0.insert_session(session))
                    .map_err(failed)
            });
        if let Err(error) = commit {
            return Err(rollback_prepared(&mut prepared, error).await);
        }
        Ok(reference)
    })?;
    if result.is_err() {
        let _ = sessions.0.remove_session(id);
    }
    result
}

/// Drive plugin-local asynchronous live reads on the persistent owner runtime.
pub(crate) fn drive_live<F>(
    host: &crate::runtime::PaneRuntimeHost,
    future: F,
) -> Result<F::Output, ExecutionError>
where
    F: std::future::Future + Send,
    F::Output: Send,
{
    let owner = host
        .spawn_runtime_handle()
        .ok_or_else(|| failed("pane execution runtime unavailable"))?;
    drive_startup(&owner.1, future)
}

fn drive_startup<F>(
    runtime: &switchy_async::runtime::Handle,
    future: F,
) -> Result<F::Output, ExecutionError>
where
    F: std::future::Future + Send,
    F::Output: Send,
{
    if runtime.runtime_flavor() != switchy_async::RuntimeFlavor::MultiThread {
        return Err(failed(
            "pane owner requires a persistent multi-thread runtime",
        ));
    }
    // Caller runtimes are transient; pane tasks inherit the persistent owner.
    match switchy_async::runtime::Handle::try_current() {
        Ok(current) if current.runtime_flavor() == switchy_async::RuntimeFlavor::MultiThread => {
            Ok(switchy_async::task::block_in_place(|| {
                runtime.block_on(future)
            }))
        }
        // No runtime on this thread: drive the owner from it.
        Err(_) => Ok(runtime.block_on(future)),
        // A current-thread runtime cannot block in place, so another thread drives the owner.
        // simulation-safe: simulated runtimes report MultiThread, so only native builds get here
        Ok(_) => std::thread::scope(|scope| scope.spawn(|| runtime.block_on(future)).join())
            .map_err(|_| failed("pane startup bridge thread panicked")),
    }
}

async fn await_startup(prepared: &mut super::PreparedFreshRuntime) -> Result<(), ExecutionError> {
    match switchy_async::time::timeout(Duration::from_secs(10), prepared.wait_until_ready()).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(ExecutionError::LaunchFailed {
            reason: error.to_string(),
        }),
        Err(_) => Err(ExecutionError::LaunchFailed {
            reason: "program startup timed out".into(),
        }),
    }
}

async fn rollback_prepared(
    prepared: &mut super::PreparedFreshRuntime,
    error: ExecutionError,
) -> ExecutionError {
    let mut cleanup = Vec::new();
    for pane in prepared
        .runtime
        .as_mut()
        .expect("uncommitted runtime")
        .panes
        .values_mut()
    {
        if let Err(error) = super::stop_unpublished_pane(pane).await {
            cleanup.push(error.to_string());
        }
    }
    if cleanup.is_empty() {
        error
    } else {
        ExecutionError::LaunchFailed {
            reason: format!("{error:?}; cleanup failed: {cleanup:?}"),
        }
    }
}

fn queued_input_authority(
    pane: &super::PaneRuntimeHandle,
    authority: super::input_authority::InputAuthority,
    deadline: Option<switchy_time::Instant>,
) -> super::input_authority::InputAuthority {
    let execution = Arc::clone(&pane.execution);
    let exited = Arc::clone(&pane.exited);
    let generation = Arc::clone(&pane.resurrection_state);
    let token = generation
        .lock()
        .ok()
        .map(|state| state.observation_execution);
    super::input_authority::InputAuthority::new(move || {
        authority.check().map_err(|error| error.to_string())?;
        let current = generation
            .lock()
            .map_err(|_| "execution identity poisoned".to_string())?
            .observation_execution;
        if token != Some(current) {
            return Err("queued input execution replaced".into());
        }
        if exited.load(Ordering::SeqCst)
            || execution.terminal_session_ended.load(Ordering::SeqCst)
            || execution.terminal_owner_retired.load(Ordering::SeqCst)
            || execution.capture_failure().is_some()
            || deadline.is_some_and(|deadline| switchy_time::Instant::now() >= deadline)
        {
            return Err("queued input authority ended".into());
        }
        Ok(())
    })
}

pub(crate) fn send(
    host: &crate::runtime::PaneRuntimeHost,
    reference: &ExecutionRef,
    data: Vec<u8>,
    authority: super::input_authority::InputAuthority,
) -> Result<(), ExecutionError> {
    authority.check().map_err(failed)?;
    if data.len() > 65_536 {
        return Err(ExecutionError::InvalidOptions {
            reason: "send exceeds 65536 bytes".into(),
        });
    }
    with_pane(host, reference, |pane| {
        if pane.exited.load(Ordering::SeqCst) {
            return Err(failed("execution has exited"));
        }
        let permit = Arc::clone(&pane.execution.admission)
            .try_acquire_owned()
            .map_err(failed)?;
        let authority = queued_input_authority(pane, authority, None);
        // The write's authority recheck runs on the PTY writer, but the host
        // answers it only while this invocation is retained: settle the write
        // before the invocation closes instead of re-authorizing afterwards.
        let receipt = super::input_authority::settlement_receipt(host).map_err(failed)?;
        let recording = host.pane_input_recording_data(&data);
        pane.input_tx
            .send(PaneRuntimeCommand::AdmittedInput(
                data, receipt, permit, authority,
            ))
            .map_err(failed)?;
        if let Some((sink, bytes)) = recording {
            sink.0.record(
                super::RecordingEventKind::PaneInputRaw,
                super::RecordingPayload::Bytes { data: bytes },
                super::RecordMeta {
                    session_id: Some(reference.session_id),
                    pane_id: Some(reference.pane_id),
                    client_id: None,
                },
            );
        }
        Ok(())
    })
}

pub(super) fn validate_alternate_source(
    grid: &bmux_terminal_grid::TerminalGrid,
    expected_grid_revision: u64,
) -> Result<(), ExecutionError> {
    if grid.mode() != bmux_terminal_grid::GridMode::Alternate {
        return Err(ExecutionError::InvalidOptions {
            reason: "execution sizing requires the alternate screen; normal-screen zoom is local"
                .into(),
        });
    }
    if grid.revision() != expected_grid_revision {
        return Err(failed(
            "stale source grid revision; refresh the structured source",
        ));
    }
    Ok(())
}

fn validate_view_size(cols: u16, rows: u16) -> Result<(), ExecutionError> {
    validate_size(cols, rows)?;
    if usize::from(cols) * usize::from(rows) > 262_144 {
        return Err(ExecutionError::InvalidOptions {
            reason: "PTY dimensions exceed 262144 cells".into(),
        });
    }
    Ok(())
}

/// Conditional last-writer execution sizing, never view state or layout mutation.
/// Reserve and drain outside the manager, then revalidate identity and mode in
/// the same geometry transaction that commits the resize.
pub(crate) fn resize_alternate(
    host: &crate::runtime::PaneRuntimeHost,
    reference: &ExecutionRef,
    expected_grid_revision: u64,
    cols: u16,
    rows: u16,
    authorize: impl Fn() -> Result<(), ExecutionError>,
) -> Result<(), ExecutionError> {
    authorize()?;
    validate_view_size(cols, rows)?;
    let execution = with_pane(host, reference, |pane| Ok(Arc::clone(&pane.execution)))?;
    let writer = execution.writer().map_err(failed)?;
    let reservation = writer
        .as_ref()
        .map(super::capture_writer::CaptureWriter::reserve_resize)
        .transpose()
        .map_err(failed)?;
    let order = execution.capture_order.lock().map_err(failed)?;
    if let Some(writer) = &writer {
        writer.check_open().map_err(failed)?;
        let boundary = writer.progress().map_err(failed)?.admitted.raw_end;
        writer
            .wait_projected_raw(boundary, Duration::from_secs(2))
            .map_err(failed)?;
    }
    let boundary = execution
        .durable_capture
        .lock()
        .map_err(failed)?
        .as_ref()
        .map(super::durable_capture::DurableCapture::raw_bytes);
    if let Some(boundary) = boundary {
        execution.wait_for_projection(boundary).map_err(failed)?;
    }
    authorize()?;
    let mut ticket = None;
    let result = with_pane(host, reference, |pane| {
        if !Arc::ptr_eq(&execution, &pane.execution) {
            return Err(ExecutionError::StaleExecution);
        }
        if pane.exited.load(Ordering::SeqCst)
            || execution.terminal_session_ended.load(Ordering::SeqCst)
            || execution.terminal_owner_retired.load(Ordering::SeqCst)
            || execution.capture_failure().is_some()
        {
            return Err(failed("execution is not available for sizing"));
        }
        let permit = Arc::clone(&execution.admission)
            .try_acquire_owned()
            .map_err(failed)?;
        pane.resize_pty_under_order(
            rows,
            cols,
            Some(permit),
            reservation,
            &mut ticket,
            Some(expected_grid_revision),
        )?;
        if ticket.is_none()
            && let Some(writer) = &writer
        {
            ticket = Some(writer.progress().map_err(failed)?.projected);
        }
        Ok(())
    });
    drop(order);
    result?;
    if let (Some(writer), Some(ticket)) = (writer, ticket) {
        writer.wait_committed(ticket).map_err(failed)?;
    }
    Ok(())
}

pub(crate) fn resize_ordered(
    host: &crate::runtime::PaneRuntimeHost,
    reference: &ExecutionRef,
    cols: u16,
    rows: u16,
) -> Result<(), ExecutionError> {
    with_pane(host, reference, |pane| {
        if pane.execution.writer().map_err(failed)?.is_none()
            && pane
                .execution
                .durable_capture
                .lock()
                .map_err(failed)?
                .is_none()
        {
            return Err(failed(
                "ordered-events-v1 capture is not available for this execution",
            ));
        }
        Ok(())
    })?;
    resize(host, reference, cols, rows)
}

pub(crate) fn resize(
    host: &crate::runtime::PaneRuntimeHost,
    reference: &ExecutionRef,
    cols: u16,
    rows: u16,
) -> Result<(), ExecutionError> {
    validate_size(cols, rows)?;
    let execution = with_pane(host, reference, |pane| Ok(Arc::clone(&pane.execution)))?;
    let writer = execution.writer().map_err(failed)?;
    if writer.is_none() {
        return with_pane(host, reference, |pane| {
            if pane.exited.load(Ordering::SeqCst) {
                return Err(failed("execution has exited"));
            }
            let permit = Arc::clone(&pane.execution.admission)
                .try_acquire_owned()
                .map_err(failed)?;
            if *pane.last_requested_size.lock().map_err(failed)? == (rows, cols) {
                return Ok(());
            }
            let revision = pane.execution.geometry_revision.load(Ordering::SeqCst);
            pane.resize_pty_admitted(rows, cols, Some(permit));
            if pane.execution.geometry_revision.load(Ordering::SeqCst) == revision {
                return Err(failed("terminal geometry could not be committed"));
            }
            Ok(())
        });
    }
    // Capacity and the ordering fence are acquired without the runtime manager.
    let reservation = writer
        .as_ref()
        .map(super::capture_writer::CaptureWriter::reserve_resize)
        .transpose()
        .map_err(failed)?;
    let order = execution.capture_order.lock().map_err(failed)?;
    if let Some(writer) = &writer {
        writer.check_open().map_err(failed)?;
        let boundary = writer.progress().map_err(failed)?.admitted.raw_end;
        writer
            .wait_projected_raw(boundary, Duration::from_secs(2))
            .map_err(failed)?;
    }
    let mut ticket = None;
    let result = with_pane(host, reference, |pane| {
        if pane.exited.load(Ordering::SeqCst) {
            return Err(failed("execution has exited"));
        }
        let permit = Arc::clone(&pane.execution.admission)
            .try_acquire_owned()
            .map_err(failed)?;
        if *pane.last_requested_size.lock().map_err(failed)? == (rows, cols) {
            if let Some(writer) = &writer {
                ticket = Some(writer.progress().map_err(failed)?.projected);
            }
            return Ok(());
        }
        let geometry_revision = pane.execution.geometry_revision.load(Ordering::SeqCst);
        pane.resize_pty_under_order(rows, cols, Some(permit), reservation, &mut ticket, None)?;
        if pane.execution.geometry_revision.load(Ordering::SeqCst) == geometry_revision
            || *pane.last_requested_size.lock().map_err(failed)? != (rows, cols)
        {
            return Err(failed("terminal geometry could not be committed"));
        }
        pane.execution.notify();
        Ok(())
    });
    drop(order);
    result?;
    if let (Some(writer), Some(ticket)) = (writer, ticket) {
        writer.wait_committed(ticket).map_err(failed)?;
    }
    Ok(())
}

pub(crate) fn presentation_wait_capabilities()
-> bmux_pane_runtime_plugin_api::pane_runtime_presentation_wait_v1::WaitCapabilities {
    bmux_pane_runtime_plugin_api::pane_runtime_presentation_wait_v1::WaitCapabilities {
        schema_version: 1,
        max_timeout_ms: 1000,
        max_response_bytes: 4 * 1024 * 1024,
        token_bytes: 32,
    }
}

fn validate_presentation_wait(
    after: Option<&[u8]>,
    timeout_ms: u32,
    max_bytes: u32,
) -> Result<(), bmux_pane_runtime_plugin_api::pane_runtime_presentation_wait_v1::WaitError> {
    use bmux_pane_runtime_plugin_api::pane_runtime_presentation_wait_v1::WaitError;
    let caps = presentation_wait_capabilities();
    if after.is_some_and(|token| token.len() != 32)
        || timeout_ms > caps.max_timeout_ms
        || max_bytes == 0
        || max_bytes > caps.max_response_bytes
    {
        return Err(WaitError::InvalidRequest {
            reason: "requires 32-byte token, timeout <=1000ms and response budget 1..=4194304"
                .into(),
        });
    }
    Ok(())
}

fn presentation_revision(
    screen: &ExecutionScreen,
    state: &ExecutionStatus,
) -> Result<Vec<u8>, ExecutionError> {
    use sha2::{Digest, Sha256};
    // Include protocol/parser state and geometry even when no output byte was
    // appended. Lifecycle changes wake completion without inventing output.
    Ok(Sha256::digest(serde_json::to_vec(&(screen, state)).map_err(failed)?).to_vec())
}

pub(crate) fn wait_present(
    host: &crate::runtime::PaneRuntimeHost,
    reference: &ExecutionRef,
    after: Option<&[u8]>,
    timeout_ms: u32,
    max_bytes: u32,
    authorize: impl Fn() -> Result<(), ExecutionError> + Sync,
) -> Result<
    Option<bmux_pane_runtime_plugin_api::pane_runtime_presentation_wait_v1::PresentationFrame>,
    bmux_pane_runtime_plugin_api::pane_runtime_presentation_wait_v1::WaitError,
> {
    use bmux_pane_runtime_plugin_api::pane_runtime_presentation_wait_v1::{
        PresentationFrame, WaitError,
    };
    validate_presentation_wait(after, timeout_ms, max_bytes)?;
    let authority = |error| WaitError::Authority { error };
    let owner = host
        .spawn_runtime_handle()
        .ok_or_else(|| authority(failed("pane execution runtime unavailable")))?;
    // This bounded wait owns no cursor or journal. Dropping the client request
    // cannot consume a frame; a retry supplies the same immutable token.
    drive_startup(&owner.1, async {
        let mut changes =
            with_pane(host, reference, |pane| Ok(pane.execution.subscribe())).map_err(authority)?;
        let deadline =
            switchy_async::time::Instant::now() + Duration::from_millis(u64::from(timeout_ms));
        loop {
            authorize().map_err(authority)?;
            let screen = screen(host, reference).map_err(|error| match error {
                ExecutionError::OperationFailed { ref reason }
                    if reason == "screen exceeds response budget" =>
                {
                    WaitError::Oversized { max_bytes }
                }
                error => authority(error),
            })?;
            let state = status(host, reference).map_err(authority)?;
            let revision = presentation_revision(&screen, &state).map_err(authority)?;
            if after != Some(revision.as_slice()) {
                let frame = PresentationFrame { revision, screen };
                let size = serde_json::to_vec(&frame)
                    .map_err(|error| authority(failed(error)))?
                    .len();
                if size > max_bytes as usize {
                    return Err(WaitError::Oversized { max_bytes });
                }
                return Ok(Some(frame));
            }
            if switchy_async::time::Instant::now() >= deadline {
                return Ok(None);
            }
            switchy_async::time::timeout_at(deadline, changes.changed())
                .await
                .map_or(Ok(()), |result| {
                    result.map_err(|error| authority(failed(error)))
                })?;
        }
    })
    .map_err(authority)?
}

pub(crate) fn wait(
    host: &crate::runtime::PaneRuntimeHost,
    reference: &ExecutionRef,
    timeout_ms: u32,
    authorize: impl Fn() -> Result<(), ExecutionError> + Sync,
) -> Result<ExecutionStatus, ExecutionError> {
    if timeout_ms > 30_000 {
        return Err(ExecutionError::InvalidOptions {
            reason: "wait timeout exceeds 30000ms; poll again".into(),
        });
    }
    let owner = host
        .spawn_runtime_handle()
        .ok_or_else(|| failed("pane execution runtime is not registered for wait"))?;
    drive_startup(&owner.1, async {
        // Subscribe before inspecting authority: a change racing the initial
        // snapshot remains pending, even before changed() is first awaited.
        let mut changes = with_pane(host, reference, |pane| Ok(pane.execution.subscribe()))?;
        let deadline =
            switchy_async::time::Instant::now() + Duration::from_millis(u64::from(timeout_ms));
        loop {
            authorize()?;
            let current = status(host, reference)?;
            if current.error.is_some()
                || (!matches!(current.phase, ExecutionPhase::Running)
                    && current.output_drained
                    && current.terminal_session_ended)
                || switchy_async::time::Instant::now() >= deadline
            {
                return Ok(current);
            }
            switchy_async::time::timeout_at(deadline, changes.changed())
                .await
                .map_or(Ok(()), |result| result.map_err(failed))?;
        }
    })?
}

pub(crate) fn terminate(
    host: &crate::runtime::PaneRuntimeHost,
    reference: &ExecutionRef,
    grace_ms: u32,
    authorize: impl Fn() -> Result<(), ExecutionError> + Sync,
) -> Result<ExecutionStatus, ExecutionError> {
    let owner = host
        .spawn_runtime_handle()
        .ok_or_else(|| failed("pane execution runtime is not registered for termination"))?;
    drive_startup(&owner.1, async {
        if grace_ms > 30_000 {
            return Err(ExecutionError::InvalidOptions {
                reason: "termination grace exceeds 30000ms".into(),
            });
        }
        let mut changes = with_pane(host, reference, |pane| Ok(pane.execution.subscribe()))?;
        let signal = |signal| {
            authorize()?;
            with_pane(host, reference, |pane| {
                pane.execution
                    .signal_owned_group(host, signal)
                    .map_err(failed)
            })
        };
        signal(StopAction::Cooperative)?;
        let deadline =
            switchy_async::time::Instant::now() + Duration::from_millis(u64::from(grace_ms));
        loop {
            authorize()?;
            let settled = with_pane(host, reference, |pane| {
                Ok(pane.exited.load(Ordering::SeqCst)
                    && pane.execution.output_drained.load(Ordering::SeqCst))
            })?;
            if settled || switchy_async::time::Instant::now() >= deadline {
                break;
            }
            switchy_async::time::timeout_at(deadline, changes.changed())
                .await
                .map_or(Ok(()), |result| result.map_err(failed))?;
        }
        // Failed phase is not evidence of process death.
        signal(StopAction::Force)?;
        await_cleanup_acknowledgement(host, reference, &authorize).await?;
        authorize()?;
        status(host, reference)
    })?
}

async fn await_cleanup_acknowledgement(
    host: &crate::runtime::PaneRuntimeHost,
    reference: &ExecutionRef,
    authorize: impl Fn() -> Result<(), ExecutionError>,
) -> Result<(), ExecutionError> {
    let mut cleanup = with_pane(host, reference, |pane| Ok(pane.cleanup.clone()))?;
    let deadline = switchy_async::time::Instant::now() + Duration::from_secs(30);
    loop {
        authorize()?;
        // Revalidate the generation and session after every wake.
        with_pane(host, reference, |_| Ok(()))?;
        match cleanup.borrow_and_update().clone() {
            Some(Ok(())) => return Ok(()),
            Some(Err(error)) => return Err(failed(error)),
            None => {}
        }
        if switchy_async::time::timeout_at(deadline, cleanup.changed())
            .await
            .is_err()
        {
            authorize()?;
            return Err(failed(
                "execution group/thread cleanup not acknowledged; recovery required",
            ));
        }
        // Channel loss is not an acknowledgement; the next authority check
        // reports session loss, or retained failure is returned explicitly.
        if cleanup.has_changed().is_err() && cleanup.borrow().is_none() {
            return Err(failed(
                "execution cleanup owner disconnected without acknowledgement",
            ));
        }
    }
}

/// Live human-input acknowledgment: PTY write/flush only, never durability or
/// application consumption. Admission is bounded and the manager lock is released
/// before waiting. A timeout is uncertain and must not trigger an automatic retry.
pub(super) fn send_human_recorded_acknowledged(
    host: &crate::runtime::PaneRuntimeHost,
    reference: &ExecutionRef,
    data: Vec<u8>,
    authority: super::input_authority::InputAuthority,
) -> Result<(), ExecutionError> {
    let sink = host
        .recording_sink()
        .ok_or_else(|| failed("recording sink unavailable"))?;
    send_human_acknowledged(host, reference, data.clone(), authority)?;
    // Fresh-effect caller owns deduplication. This enqueue is volatile; failure
    // after PTY write is uncertain and must close the epoch, never replay input.
    let result = sink.0.record_checked(
        super::RecordingEventKind::PaneInputRaw,
        super::RecordingPayload::Bytes { data },
        super::RecordMeta {
            session_id: Some(reference.session_id),
            pane_id: Some(reference.pane_id),
            client_id: None,
        },
    );
    if let Err(error) = result {
        with_pane(host, reference, |pane| {
            pane.execution
                .fail_capture(&format!("human input recording failed: {error}"));
            Ok(())
        })?;
        return Err(failed(error));
    }
    Ok(())
}

pub(super) fn send_human_acknowledged(
    host: &crate::runtime::PaneRuntimeHost,
    reference: &ExecutionRef,
    data: Vec<u8>,
    authority: super::input_authority::InputAuthority,
) -> Result<(), ExecutionError> {
    if data.is_empty() || data.len() > 16_384 {
        return Err(ExecutionError::InvalidOptions {
            reason: "human input must contain 1..=16384 bytes".into(),
        });
    }
    send_acknowledged(host, reference, data, authority)
}

pub(super) fn send_acknowledged(
    host: &crate::runtime::PaneRuntimeHost,
    reference: &ExecutionRef,
    data: Vec<u8>,
    authority: super::input_authority::InputAuthority,
) -> Result<(), ExecutionError> {
    authority.check().map_err(failed)?;
    let (sender, receiver) = switchy_thread::sync::mpsc::sync_channel(1);
    with_pane(host, reference, |pane| {
        if pane.exited.load(Ordering::SeqCst)
            || pane.execution.terminal_session_ended.load(Ordering::SeqCst)
            || pane.execution.capture_failure().is_some()
        {
            return Err(failed("execution is not accepting input"));
        }
        let permit = Arc::clone(&pane.execution.admission)
            .try_acquire_owned()
            .map_err(failed)?;
        let authority = queued_input_authority(
            pane,
            authority,
            Some(switchy_time::Instant::now() + Duration::from_secs(5)),
        );
        pane.input_tx
            .send(PaneRuntimeCommand::AdmittedInput(
                data, sender, permit, authority,
            ))
            .map_err(failed)
    })?;
    receiver
        .recv_timeout(Duration::from_secs(5))
        .map_err(failed)?
        .map_err(failed)
}

fn install_reserved_capture(
    host: &crate::runtime::PaneRuntimeHost,
    prepared: &super::PreparedFreshRuntime,
    reserved: Option<&ExecutionRef>,
) -> Result<(), ExecutionError> {
    if let Some(reference) = reserved {
        let capture = super::reservations::create_capture(host, reference)
            .map_err(|error| failed(format!("{error:?}")))?;
        let pane = prepared
            .runtime
            .as_ref()
            .expect("prepared runtime")
            .panes
            .values()
            .next()
            .expect("prepared pane");
        *pane.execution.durable_capture.lock().map_err(failed)? = Some(capture);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::StopAction;

    #[test]
    fn alternate_view_size_is_bounded_before_runtime_lookup() {
        for (cols, rows) in [(0, 1), (1, 0), (1001, 1), (1, 1001), (513, 512)] {
            assert!(matches!(
                super::validate_view_size(cols, rows),
                Err(super::ExecutionError::InvalidOptions { .. })
            ));
        }
        for (cols, rows) in [(1, 1), (1000, 1), (1, 1000), (512, 512)] {
            assert!(super::validate_view_size(cols, rows).is_ok());
        }
    }

    #[test]
    fn alternate_source_rejects_normal_stale_and_reentered_screen() {
        let mut source = bmux_terminal_grid::TerminalGridStream::new(
            10,
            2,
            bmux_terminal_grid::GridLimits::default(),
        )
        .unwrap();
        let normal = source.grid().revision();
        assert!(super::validate_alternate_source(source.grid(), normal).is_err());
        source.process(b"\x1b[?1049h");
        let alternate = source.grid().revision();
        assert!(super::validate_alternate_source(source.grid(), alternate).is_ok());
        assert!(super::validate_alternate_source(source.grid(), normal).is_err());
        source.process(b"\x1b[?1049l\x1b[?1049h");
        assert!(super::validate_alternate_source(source.grid(), alternate).is_err());
    }

    #[test]
    fn alternate_resize_authorization_precedes_runtime_access() {
        let host = crate::runtime::PaneRuntimeHost::detached();
        let host = &host;
        let execution = super::ExecutionRef {
            session_id: crate::ids::new_uuid(),
            pane_id: crate::ids::new_uuid(),
            execution_token: crate::ids::new_uuid(),
        };
        assert!(matches!(
            super::resize_alternate(host, &execution, 0, 80, 24, || {
                Err(super::ExecutionError::Denied {
                    reason: "revoked".into(),
                })
            }),
            Err(super::ExecutionError::Denied { .. })
        ));
    }

    #[switchy_async::test]
    async fn execution_keeps_live_drain_and_committed_capture_distinct() {
        let root = tempfile::tempdir().unwrap();
        let capture = super::super::durable_capture::DurableCapture::create(root.path()).unwrap();
        let (writer, mut owner) =
            super::super::capture_writer::CaptureWriter::start(capture).unwrap();
        let execution = super::ExecutionCapture::default();
        *execution.capture_writer.lock().unwrap() = Some(writer.clone());
        let ticket = writer
            .reserve_output()
            .unwrap()
            .output(b"tail".to_vec())
            .unwrap();
        writer.wait_committed(ticket).unwrap();
        // Disk may run ahead of projection, but v4 status cannot claim unseen
        // projected bytes. Nor does observed live EOF imply durable clean EOF.
        assert_eq!(execution.durable_watermark(), 0);
        execution.output_watermark.store(4, super::Ordering::SeqCst);
        assert_eq!(execution.durable_watermark(), 4);
        execution
            .output_drained
            .store(true, super::Ordering::SeqCst);
        assert!(!execution.durable_capture_complete());
        let end = writer.reserve_end().unwrap().end(None).unwrap();
        writer.wait_committed(end).unwrap();
        assert!(execution.durable_capture_complete());
        owner.take().join().unwrap();
    }

    #[switchy_async::test]
    async fn execution_capture_failure_never_acknowledges_clean_completion() {
        let root = tempfile::tempdir().unwrap();
        let capture = super::super::durable_capture::DurableCapture::create(root.path()).unwrap();
        let (writer, mut owner) =
            super::super::capture_writer::CaptureWriter::start(capture).unwrap();
        let execution = super::ExecutionCapture::default();
        *execution.capture_writer.lock().unwrap() = Some(writer.clone());
        execution
            .output_drained
            .store(true, super::Ordering::SeqCst);
        writer.fail("injected persistence failure");
        assert!(!execution.durable_capture_complete());
        assert_eq!(
            execution.capture_failure().as_deref(),
            Some("injected persistence failure")
        );
        assert!(writer.reserve_resize().is_err());
        owner.take().join().unwrap();
    }

    #[test]
    fn resize_barrier_waits_for_committed_projection_not_capture_or_early_watermark() {
        let execution = switchy_thread::sync::Arc::new(super::ExecutionCapture::default());
        let (started_tx, started_rx) = switchy_thread::sync::mpsc::channel();
        let (done_tx, done_rx) = switchy_thread::sync::mpsc::channel();
        let worker = switchy_thread::sync::Arc::clone(&execution);
        let thread = switchy_thread::thread::spawn(move || {
            let _order = worker.capture_order.lock().unwrap();
            started_tx.send(()).unwrap();
            worker.wait_for_projection(1024).unwrap();
            done_tx.send(()).unwrap();
        });
        started_rx.recv().unwrap();
        execution
            .output_watermark
            .store(1024, super::Ordering::SeqCst);
        assert!(done_rx.try_recv().is_err());
        // Parser publication does not need the capture gate or a new raw read.
        execution.commit_projection().unwrap();
        done_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        thread.join().unwrap();
    }

    #[test]
    fn resize_barrier_accepts_an_already_projected_boundary() {
        let execution = super::ExecutionCapture::default();
        execution
            .output_watermark
            .store(2048, super::Ordering::SeqCst);
        execution.commit_projection().unwrap();
        execution.wait_for_projection(1024).unwrap();
    }

    fn presentation_state() -> (super::ExecutionScreen, super::ExecutionStatus) {
        let reference = super::ExecutionRef {
            session_id: crate::ids::new_uuid(),
            pane_id: crate::ids::new_uuid(),
            execution_token: crate::ids::new_uuid(),
        };
        let screen = super::ExecutionScreen {
            execution: reference.clone(),
            output_watermark: 0,
            cols: 8,
            rows: 2,
            cursor_col: 0,
            cursor_row: 0,
            cursor_visible: true,
            text: String::new(),
            encoded_grid: b"protocol-a".to_vec(),
        };
        let state = serde_json::from_value(serde_json::json!({"execution":reference,"phase":"running","lifecycle":"conventional_terminal_session","session_end":"active","terminal_session_ended":false,"capture_integrity":"capturing","exit_code":null,"signal":null,"output_drained":false,"output_watermark":0,"cleanup_complete":false,"error":null})).unwrap();
        (screen, state)
    }

    #[test]
    fn presentation_revision_covers_resize_protocol_and_end_without_output() {
        let (mut screen, mut state) = presentation_state();
        let original = super::presentation_revision(&screen, &state).unwrap();
        assert_eq!(original.len(), 32);
        assert_eq!(
            original,
            super::presentation_revision(&screen, &state).unwrap()
        );
        screen.cols += 1;
        assert_ne!(
            original,
            super::presentation_revision(&screen, &state).unwrap()
        );
        screen.cols -= 1;
        screen.encoded_grid = b"protocol-b".to_vec();
        assert_ne!(
            original,
            super::presentation_revision(&screen, &state).unwrap()
        );
        screen.encoded_grid = b"protocol-a".to_vec();
        state.terminal_session_ended = true;
        assert_ne!(
            original,
            super::presentation_revision(&screen, &state).unwrap()
        );
    }

    #[test]
    fn presentation_wait_bounds_are_explicit() {
        assert!(super::validate_presentation_wait(Some(&[0; 32]), 1000, 4 * 1024 * 1024).is_ok());
        assert!(super::validate_presentation_wait(Some(&[0; 31]), 1000, 100).is_err());
        assert!(super::validate_presentation_wait(None, 1001, 100).is_err());
        assert!(super::validate_presentation_wait(None, 1000, 0).is_err());
        assert!(super::validate_presentation_wait(None, 1000, 4 * 1024 * 1024 + 1).is_err());
    }

    #[switchy_async::test]
    async fn canceled_wait_does_not_consume_token_or_notification() {
        let capture = super::ExecutionCapture::default();
        let mut changes = capture.subscribe();
        let (screen, state) = presentation_state();
        let token = super::presentation_revision(&screen, &state).unwrap();
        let canceled = changes.changed();
        drop(canceled);
        capture.notify();
        changes.changed().await.unwrap();
        assert_eq!(
            token,
            super::presentation_revision(&screen, &state).unwrap()
        );
    }

    #[test]
    fn retirement_releases_lease_before_publishing_without_fabricating_eof() {
        let capture = super::ExecutionCapture::default();
        assert!(
            capture
                .finish_owner_retirement(|| panic!("active scope must retain lease"))
                .is_err()
        );
        capture
            .terminal_session_ended
            .store(true, super::Ordering::SeqCst);
        assert!(
            capture
                .finish_owner_retirement(|| anyhow::bail!("lease failure"))
                .is_err()
        );
        assert!(!capture.terminal_owner_retired.load(super::Ordering::SeqCst));
        capture
            .finish_owner_retirement(|| {
                assert!(!capture.terminal_owner_retired.load(super::Ordering::SeqCst));
                assert!(!capture.output_drained.load(super::Ordering::SeqCst));
                Ok(())
            })
            .unwrap();
        assert!(capture.terminal_owner_retired.load(super::Ordering::SeqCst));
        assert!(!capture.output_drained.load(super::Ordering::SeqCst));
    }

    #[test]
    fn natural_cleanup_completes_without_kill_or_capture_eof() {
        let capture = super::ExecutionCapture::default();
        capture
            .terminal_session_ended
            .store(true, super::Ordering::SeqCst);
        capture
            .terminal_owner_retired
            .store(true, super::Ordering::SeqCst);

        assert!(capture.cleanup_complete(true));
        assert!(!capture.group_kill_issued.load(super::Ordering::SeqCst));
        assert!(!capture.target_stop_issued.load(super::Ordering::SeqCst));
        assert!(!capture.output_drained.load(super::Ordering::SeqCst));
        assert_eq!(*capture.outcome.lock().unwrap(), (None, None, None));
    }

    #[test]
    fn cleanup_requires_resource_acknowledgement_and_retired_authority() {
        let capture = super::ExecutionCapture::default();
        capture
            .group_kill_issued
            .store(true, super::Ordering::SeqCst);
        capture.output_drained.store(true, super::Ordering::SeqCst);
        capture
            .terminal_session_ended
            .store(true, super::Ordering::SeqCst);

        // Killing, actual EOF, and session end cannot replace owner retirement.
        assert!(!capture.cleanup_complete(true));
        capture
            .terminal_owner_retired
            .store(true, super::Ordering::SeqCst);
        assert!(!capture.cleanup_complete(false));
        assert!(capture.cleanup_complete(true));
    }

    #[test]
    fn acknowledged_cleanup_does_not_hide_cleanup_failure_or_change_capture_truth() {
        let capture = super::ExecutionCapture::default();
        capture
            .terminal_owner_retired
            .store(true, super::Ordering::SeqCst);
        capture.output_drained.store(true, super::Ordering::SeqCst);
        capture.record_cleanup_error("resource cleanup failed".into());

        assert!(!capture.cleanup_complete(true));
        assert!(capture.output_drained.load(super::Ordering::SeqCst));
        assert_eq!(
            capture.outcome.lock().unwrap().2.as_deref(),
            Some("resource cleanup failed")
        );
    }

    #[test]
    fn concurrent_termination_after_natural_session_end_does_not_dispatch() {
        let host = crate::runtime::PaneRuntimeHost::detached();
        let host = &host;
        let capture = switchy_thread::sync::Arc::new(super::ExecutionCapture::default());
        // A missing PTY would be a recovery error if signaling fell through.
        capture.managed_program.store(true, super::Ordering::SeqCst);
        let fence = capture.session.lock().unwrap();
        let first = switchy_thread::sync::Arc::clone(&capture);
        let second = switchy_thread::sync::Arc::clone(&capture);
        let (first_host, second_host) = (host.clone(), host.clone());
        let a = switchy_thread::thread::spawn(move || {
            first.signal_owned_group(&first_host, StopAction::Force)
        });
        let b = switchy_thread::thread::spawn(move || {
            second.signal_owned_group(&second_host, StopAction::Cooperative)
        });
        capture
            .terminal_session_ended
            .store(true, super::Ordering::SeqCst);
        drop(fence);

        assert!(!a.join().unwrap().unwrap());
        assert!(!b.join().unwrap().unwrap());
        assert!(!capture.terminal_owner_retired.load(super::Ordering::SeqCst));
        assert!(!capture.group_kill_issued.load(super::Ordering::SeqCst));
        assert!(!capture.target_stop_issued.load(super::Ordering::SeqCst));
        assert!(!capture.output_drained.load(super::Ordering::SeqCst));
        assert!(!capture.cleanup_complete(true));
    }

    #[test]
    fn concurrent_termination_after_natural_retirement_does_not_dispatch() {
        let host = crate::runtime::PaneRuntimeHost::detached();
        let host = &host;
        let capture = switchy_thread::sync::Arc::new(super::ExecutionCapture::default());
        let fence = capture.session.lock().unwrap();
        let first = switchy_thread::sync::Arc::clone(&capture);
        let second = switchy_thread::sync::Arc::clone(&capture);
        let (first_host, second_host) = (host.clone(), host.clone());
        let a = switchy_thread::thread::spawn(move || {
            first.signal_owned_group(&first_host, StopAction::Force)
        });
        let b = switchy_thread::thread::spawn(move || {
            second.signal_owned_group(&second_host, StopAction::Cooperative)
        });
        // Same fence used by natural retirement and side-effect authorization.
        capture
            .terminal_owner_retired
            .store(true, super::Ordering::SeqCst);
        drop(fence);
        assert!(!a.join().unwrap().unwrap());
        assert!(!b.join().unwrap().unwrap());
    }

    #[switchy_async::test]
    async fn retained_event_before_subscription_is_visible_without_pending_wake() {
        let capture = super::ExecutionCapture::default();
        capture.record_error("retained failure".into());
        let changes = capture.subscribe();
        assert!(!changes.has_changed().unwrap());
        assert_eq!(
            capture.outcome.lock().unwrap().2.as_deref(),
            Some("retained failure")
        );
        // Initial inspection, not a synthetic event, establishes readiness.
    }

    #[switchy_async::test]
    async fn simultaneous_waiters_observe_event_before_first_wait() {
        let capture = super::ExecutionCapture::default();
        let mut first = capture.subscribe();
        let mut second = capture.subscribe();
        capture.notify();
        let (a, b) = switchy_async::join!(first.changed(), second.changed());
        a.unwrap();
        b.unwrap();
        assert!(!first.has_changed().unwrap());
        assert!(!second.has_changed().unwrap());
    }

    #[switchy_async::test]
    async fn idle_capture_has_only_one_deadline_and_no_lifecycle_wake() {
        let capture = super::ExecutionCapture::default();
        let mut changes = capture.subscribe();
        let deadline = switchy_async::time::Instant::now() + super::Duration::from_millis(20);
        assert!(
            switchy_async::time::timeout_at(deadline, changes.changed())
                .await
                .is_err()
        );
        assert!(!changes.has_changed().unwrap());
        assert!(!capture.output_drained.load(super::Ordering::SeqCst));
        assert!(capture.outcome.lock().unwrap().0.is_none());
    }

    #[switchy_async::test]
    async fn errors_eof_and_cleanup_are_independent_acknowledgements() {
        let capture = super::ExecutionCapture::default();
        let mut changes = capture.subscribe();
        capture.record_error("reader failed".into());
        changes.changed().await.unwrap();
        assert!(!capture.output_drained.load(super::Ordering::SeqCst));
        assert!(!capture.cleanup_failed.load(super::Ordering::SeqCst));
        capture.output_drained.store(true, super::Ordering::SeqCst);
        capture.notify();
        changes.changed().await.unwrap();
        assert!(capture.outcome.lock().unwrap().0.is_none());
        assert!(capture.cleanup_error().is_none());
        capture.record_cleanup_error("cleanup failed".into());
        capture.record_cleanup_error("later cleanup failure".into());
        changes.changed().await.unwrap();
        assert!(capture.cleanup_failed.load(super::Ordering::SeqCst));
        assert_eq!(capture.cleanup_error().as_deref(), Some("cleanup failed"));
        assert_eq!(
            capture.outcome.lock().unwrap().2.as_deref(),
            Some("reader failed")
        );
    }

    /// A multi-thread runtime, as the server's, and a current-thread one, as a caller's.
    fn runtimes() -> (
        switchy_async::runtime::Runtime,
        switchy_async::runtime::Runtime,
    ) {
        let mut owner = switchy_async::runtime::Builder::new();
        owner
            .flavor(switchy_async::RuntimeFlavor::MultiThread)
            .worker_threads(2);
        let owner = owner.build().unwrap();
        let caller = switchy_async::runtime::Builder::new().build().unwrap();
        (owner, caller)
    }

    #[test]
    fn current_thread_control_preserves_cancellation_error() {
        let (owner, caller) = runtimes();
        let handle = owner.handle();
        let cancelled = switchy_thread::sync::atomic::AtomicBool::new(true);
        let authorize = || {
            assert!(cancelled.load(super::Ordering::SeqCst));
            Err::<(), _>(super::ExecutionError::OperationFailed {
                reason: "execution request cancelled".into(),
            })
        };
        let result = caller
            .block_on(async { super::drive_startup(&handle, async { authorize() }).unwrap() });
        assert!(
            matches!(result, Err(super::ExecutionError::OperationFailed { reason }) if reason == "execution request cancelled")
        );
    }

    #[test]
    fn current_thread_caller_keeps_tasks_on_persistent_owner() {
        let (owner, caller) = runtimes();
        let handle = owner.handle();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        caller.block_on(async {
            super::drive_startup(&handle, async {
                switchy_async::task::spawn(async move {
                    switchy_async::time::sleep(std::time::Duration::from_millis(20)).await;
                    sender.send(()).unwrap();
                });
            })
            .unwrap();
        });
        drop(caller);
        owner.block_on(async {
            switchy_async::time::timeout(std::time::Duration::from_secs(2), receiver)
                .await
                .unwrap()
                .unwrap();
        });
    }

    use super::*;
    #[test]
    fn literal_arguments_and_environment_are_validated_not_interpreted() {
        let mut options = ProgramOptions {
            program: "printf".into(),
            args: vec!["a;$(touch nope)".into(), String::new()],
            cwd: None,
            env: std::collections::BTreeMap::new(),
            cols: 80,
            rows: 24,
        };
        assert!(validate_options(&options).is_ok());
        options.env.insert("BAD=KEY".into(), "value".into());
        assert!(validate_options(&options).is_err());
        options.env.clear();
        options.cols = 0;
        assert!(validate_options(&options).is_err());
    }
    #[test]
    fn replaced_execution_never_accepts_old_reference() {
        let reference = ExecutionRef {
            session_id: crate::ids::new_uuid(),
            pane_id: crate::ids::new_uuid(),
            execution_token: crate::ids::new_uuid(),
        };
        assert!(validate_token(reference.execution_token, &reference).is_ok());
        assert_eq!(
            validate_token(crate::ids::new_uuid(), &reference),
            Err(ExecutionError::StaleExecution)
        );
    }

    #[test]
    fn program_denial_precedes_filesystem_and_runtime_observation() {
        let host = crate::runtime::PaneRuntimeHost::detached();
        let host = &host;
        let options = ProgramOptions {
            program: String::new(),
            args: Vec::new(),
            cwd: Some("/not-present".into()),
            env: std::collections::BTreeMap::new(),
            cols: 0,
            rows: 0,
        };
        assert!(matches!(
            create(host, None, options, |_| Err(anyhow::anyhow!("denied"))),
            Err(ExecutionError::Denied { .. })
        ));
    }

    #[test]
    fn spawn_boundary_rechecks_current_authorization() {
        let capture = ExecutionCapture::default();
        let allowed = Arc::new(AtomicBool::new(true));
        let current = Arc::clone(&allowed);
        *capture.spawn_authorization.lock().unwrap() = Some(Arc::new(move || {
            anyhow::ensure!(current.load(Ordering::SeqCst), "creation revoked");
            Ok(())
        }));
        capture.authorize_spawn().unwrap();
        allowed.store(false, Ordering::SeqCst);
        assert!(
            capture
                .authorize_spawn()
                .unwrap_err()
                .to_string()
                .contains("revoked")
        );
    }

    #[test]
    fn eof_and_exit_are_independent() {
        let capture = ExecutionCapture::default();
        capture.record_exit(&Ok(bmux_pty::ChildExit::with_exit_code(0)));
        assert!(!capture.output_drained.load(Ordering::SeqCst));
        assert_eq!(capture.outcome.lock().unwrap().0, Some(0));
        capture.output_drained.store(true, Ordering::SeqCst);
        assert!(capture.output_drained.load(Ordering::SeqCst));
    }
}
