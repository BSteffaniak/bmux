//! Exact, generation-guarded program execution on the existing PTY runtime.
use super::{PaneRuntimeCommand, PaneRuntimeHandle, spawn_runtime_handle};
use bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::{
    TerminalExecutionError as ExecutionError, TerminalExecutionPhase as ExecutionPhase,
    TerminalExecutionRef as ExecutionRef, TerminalExecutionScreen as ExecutionScreen,
    TerminalExecutionStatus as ExecutionStatus, TerminalProgramOptions as ProgramOptions,
};
use bmux_session_models::SessionId;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Duration;
use tokio::sync::Semaphore;

#[cfg(unix)]
pub(super) const GROUP_KILL: i32 = libc::SIGKILL;
#[cfg(not(unix))]
pub(super) const GROUP_KILL: i32 = 9;

type SpawnAuthorization = Arc<dyn Fn() -> anyhow::Result<()> + Send + Sync>;

pub(super) struct ExecutionCapture {
    pub(super) supervisor: Mutex<Option<Arc<super::supervisor::SupervisorControl>>>,
    #[cfg(unix)]
    pub(super) lifecycle_interrupter: Mutex<Option<std::os::unix::net::UnixStream>>,
    pub(super) spawn_authorization: Mutex<Option<SpawnAuthorization>>,
    pub(super) group_kill_issued: AtomicBool,
    pub(super) target_stop_issued: AtomicBool,
    pub(super) cleanup_failed: AtomicBool,
    pub(super) managed_program: AtomicBool,
    pub(super) terminal_session_ended: AtomicBool,
    pub(super) terminal_owner_retired: AtomicBool,
    #[cfg(test)]
    pub(super) inject_reader_failure: AtomicBool,
    pub(super) durable_capture: Mutex<Option<super::durable_capture::DurableCapture>>,
    pub(super) master: Mutex<Option<Box<dyn portable_pty::MasterPty + Send>>>,
    pub(super) output_drained: AtomicBool,
    pub(super) geometry_revision: AtomicU64,
    pub(super) output_watermark: AtomicU64,
    pub(super) outcome: Mutex<(Option<u32>, Option<String>, Option<String>)>,
    pub(super) lifecycle_done: tokio::sync::watch::Sender<bool>,
    changes: tokio::sync::watch::Sender<()>,
    admission: Arc<Semaphore>,
}

impl Default for ExecutionCapture {
    fn default() -> Self {
        Self {
            supervisor: Mutex::new(None),
            #[cfg(unix)]
            lifecycle_interrupter: Mutex::new(None),
            spawn_authorization: Mutex::new(None),
            group_kill_issued: AtomicBool::new(false),
            target_stop_issued: AtomicBool::new(false),
            cleanup_failed: AtomicBool::new(false),
            managed_program: AtomicBool::new(false),
            terminal_session_ended: AtomicBool::new(false),
            terminal_owner_retired: AtomicBool::new(false),
            #[cfg(test)]
            inject_reader_failure: AtomicBool::new(false),
            durable_capture: Mutex::new(None),
            master: Mutex::new(None),
            output_drained: AtomicBool::new(false),
            geometry_revision: AtomicU64::new(0),
            output_watermark: AtomicU64::new(0),
            outcome: Mutex::new((None, None, None)),
            lifecycle_done: tokio::sync::watch::channel(true).0,
            changes: tokio::sync::watch::channel(()).0,
            admission: Arc::new(Semaphore::new(64)),
        }
    }
}

impl ExecutionCapture {
    pub(super) fn interrupt_lifecycle(&self) {
        #[cfg(unix)]
        if let Ok(socket) = self.lifecycle_interrupter.lock()
            && let Some(socket) = socket.as_ref()
        {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
    }

    pub(super) async fn join_lifecycle(&self) -> anyhow::Result<()> {
        let mut done = self.lifecycle_done.subscribe();
        self.interrupt_lifecycle();
        while !*done.borrow_and_update() {
            done.changed().await?;
        }
        Ok(())
    }

    pub(super) fn subscribe(&self) -> tokio::sync::watch::Receiver<()> {
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

    pub(super) fn signal_owned_group(&self, signal: i32) -> anyhow::Result<bool> {
        // Serialize controls through their acknowledged consumption fence, so
        // concurrent cleanup callers cannot send Sweep after helper retirement.
        let supervisor_guard = self
            .supervisor
            .lock()
            .map_err(|_| anyhow::anyhow!("supervisor control lock poisoned"))?;
        if self.terminal_owner_retired.load(Ordering::SeqCst) {
            return Ok(false);
        }
        if let Some(supervisor) = supervisor_guard.as_ref() {
            if self.group_kill_issued.load(Ordering::SeqCst)
                || self.target_stop_issued.load(Ordering::SeqCst)
            {
                return Ok(false);
            }
            let operation = if signal == GROUP_KILL {
                crate::supervisor_protocol::Operation::Sweep {
                    mutation_id: "00000000-0000-4000-8000-000000000002".into(),
                }
            } else if signal == libc::SIGTERM {
                crate::supervisor_protocol::Operation::Term {
                    mutation_id: "00000000-0000-4000-8000-000000000001".into(),
                }
            } else {
                anyhow::bail!("unsupported supervisor signal")
            };
            let response = supervisor.request(operation)?;
            if signal == GROUP_KILL {
                anyhow::ensure!(
                    response.group_consumed || response.target_terminated,
                    "owned target termination not acknowledged"
                );
                // Fence only an acknowledged sweep. Failed/uncertain requests
                // retain their mutation identity and remain retryable.
                // Preserve the authoritative target outcome even if sweep wins
                // the race with the waiter polling its natural exit.
                if let Ok(mut outcome) = self.outcome.lock() {
                    outcome.0 = response.exit_code.and_then(|code| u32::try_from(code).ok());
                    outcome.1 = response.exit_signal.map(|signal| signal.to_string());
                }
                if response.target_terminated && response.terminal_session_ended == Some(true) {
                    self.terminal_session_ended.store(true, Ordering::SeqCst);
                    let retired =
                        supervisor.request(crate::supervisor_protocol::Operation::RetireOwner {
                            mutation_id: "00000000-0000-4000-8000-000000000003".into(),
                        })?;
                    anyhow::ensure!(
                        retired.error.is_none(),
                        "terminal owner retirement rejected"
                    );
                    self.terminal_owner_retired.store(true, Ordering::SeqCst);
                    self.interrupt_lifecycle();
                }
                // Publish stop fences only after the retirement receipt; waiter
                // consumes these without the control lock to reap the guardian.
                self.target_stop_issued
                    .store(response.target_terminated, Ordering::SeqCst);
                self.group_kill_issued
                    .store(response.group_consumed, Ordering::SeqCst);
            }
            self.notify();
            return Ok(true);
        }
        #[cfg(unix)]
        {
            if signal == GROUP_KILL && self.group_kill_issued.load(Ordering::SeqCst) {
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
            let Some(group) = master.process_group_leader().filter(|group| *group > 0) else {
                if self.output_drained.load(Ordering::SeqCst)
                    && !self.managed_program.load(Ordering::SeqCst)
                {
                    return Ok(false);
                }
                anyhow::bail!(
                    "PTY foreground process-group authority unavailable; descendant cleanup requires recovery"
                );
            };
            // The retained PTY, not a saved numeric PGID, supplies current group
            // authority. Keep its ownership locked through the side effect.
            // SAFETY: the live PTY owns this foreground group; signal numbers are
            // selected internally and no saved/recycled PID is used.
            let result = unsafe { libc::kill(-group, signal) };
            if result == 0 {
                if signal == GROUP_KILL {
                    self.group_kill_issued.store(true, Ordering::SeqCst);
                }
                return Ok(true);
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(false);
            }
            Err(error.into())
        }
        #[cfg(not(unix))]
        {
            let _ = signal;
            anyhow::bail!("owned PTY process-group signaling unavailable")
        }
    }

    pub(super) fn record_exit(&self, result: &std::io::Result<portable_pty::ExitStatus>) {
        if let Ok(mut outcome) = self.outcome.lock() {
            match result {
                Ok(status) => {
                    outcome.0 = Some(status.exit_code());
                    outcome.1 = status.signal().map(ToString::to_string);
                }
                Err(error) => outcome.2 = Some(format!("process wait failed: {error}")),
            }
        }
        self.notify();
    }
    pub(super) fn record_cleanup_error(&self, error: String) {
        self.cleanup_failed.store(true, Ordering::SeqCst);
        self.record_error(error);
    }

    pub(super) fn record_error(&self, error: String) {
        if let Ok(mut outcome) = self.outcome.lock() {
            outcome.2.get_or_insert(error);
        }
        self.notify();
    }
}

pub(crate) fn managed_program_ids() -> anyhow::Result<std::collections::BTreeSet<uuid::Uuid>> {
    let handle =
        spawn_runtime_handle().ok_or_else(|| anyhow::anyhow!("execution runtime unavailable"))?;
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

fn failed(error: impl std::fmt::Display) -> ExecutionError {
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
    reference: &ExecutionRef,
    action: impl FnOnce(&PaneRuntimeHandle) -> Result<T, ExecutionError>,
) -> Result<T, ExecutionError> {
    let handle = spawn_runtime_handle().ok_or(ExecutionError::Unavailable)?;
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

pub(crate) fn status(reference: &ExecutionRef) -> Result<ExecutionStatus, ExecutionError> {
    with_pane(reference, |pane| {
        let outcome = pane.execution.outcome.lock().map_err(failed)?;
        tracing::debug!(
            execution_token = %reference.execution_token,
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
            } else if outcome.2.is_some() {
                bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalSessionEnd::RecoveryRequired
            } else if pane.exited.load(Ordering::SeqCst) {
                bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalSessionEnd::Draining
            } else { bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalSessionEnd::Active },
            terminal_session_ended: pane.execution.terminal_session_ended.load(Ordering::SeqCst),
            capture_integrity: if pane.execution.output_drained.load(Ordering::SeqCst) {
                bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalCaptureIntegrity::Complete
            } else if outcome.2.is_some() { bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalCaptureIntegrity::Incomplete
            } else { bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalCaptureIntegrity::Capturing },
            cleanup_complete: pane.execution.group_kill_issued.load(Ordering::SeqCst) && !pane.execution.cleanup_failed.load(Ordering::SeqCst) && matches!(&*pane.cleanup.borrow(), Some(Ok(()))),
            execution: reference.clone(),
            phase: if outcome.2.is_some() {
                ExecutionPhase::Failed
            } else if pane.exited.load(Ordering::SeqCst) {
                ExecutionPhase::Exited
            } else {
                ExecutionPhase::Running
            },
            exit_code: if outcome.1.is_none() { outcome.0 } else { None },
            signal: outcome.1.clone(),
            error: outcome.2.clone(),
            output_drained: pane.execution.output_drained.load(Ordering::SeqCst),
            output_watermark: pane.execution.output_watermark.load(Ordering::SeqCst),
        })
    })
}

pub(crate) fn screen(reference: &ExecutionRef) -> Result<ExecutionScreen, ExecutionError> {
    with_pane(reference, |pane| {
        // Same first lock as the reader and resize path: geometry, grid and
        // watermark describe one committed output boundary, even after exit.
        let _size = pane.last_requested_size.lock().map_err(failed)?;
        let stream = pane.terminal_grid.lock().map_err(failed)?;
        let grid = stream.grid();
        let snapshot = stream
            .try_snapshot(0, grid.height(), super::RESPONSE_OUTPUT_BUDGET)
            .ok_or_else(|| failed("screen exceeds response budget"))?;
        let encoded_grid = serde_json::to_vec(&snapshot).map_err(failed)?;
        let text = bmux_terminal_grid::visible_text(grid, 0, grid.height());
        if encoded_grid.len().saturating_add(text.len()) > super::RESPONSE_OUTPUT_BUDGET {
            return Err(failed("screen exceeds response budget"));
        }
        Ok(ExecutionScreen {
            execution: reference.clone(),
            output_watermark: pane.execution.output_watermark.load(Ordering::SeqCst),
            cols: snapshot.width,
            rows: snapshot.height,
            cursor_col: snapshot.cursor.col,
            cursor_row: snapshot.cursor.row,
            cursor_visible: snapshot.cursor.visible,
            text,
            encoded_grid,
        })
    })
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
        if cwd.contains('\0') || !path.is_absolute() || !path.is_dir() {
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
    name: Option<String>,
    options: ProgramOptions,
    authorize: impl Fn(uuid::Uuid) -> anyhow::Result<()> + Send + Sync + 'static,
) -> Result<ExecutionRef, ExecutionError> {
    let authorize = Arc::new(authorize);
    // Authorization precedes observing the durable store/runtime installation.
    authorize(uuid::Uuid::new_v4()).map_err(|error| ExecutionError::Denied {
        reason: error.to_string(),
    })?;
    let reservation = super::reservations::reserve(uuid::Uuid::new_v4(), options, |id| {
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
        super::reservations::launch_named(&reservation, name, move |id| launch_authorize(id))
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
    name: Option<String>,
    options: ProgramOptions,
    reserved: Option<&ExecutionRef>,
    authorize: impl Fn(uuid::Uuid) -> anyhow::Result<()> + Send + Sync + 'static,
) -> Result<ExecutionRef, ExecutionError> {
    let authorize = Arc::new(authorize);
    let mut session = bmux_session_models::Session::new(name);
    if let Some(reference) = &reserved {
        session.id = SessionId(reference.session_id);
    }
    let id = session.id;
    authorize(id.0).map_err(|error| ExecutionError::Denied {
        reason: error.to_string(),
    })?;
    validate_options(&options)?;
    let handle =
        spawn_runtime_handle().ok_or_else(|| failed("pane execution runtime is not registered"))?;
    let sessions = bmux_plugin::global_plugin_state_registry()
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
        install_reserved_capture(&prepared, reserved)?;
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

fn drive_startup<F>(
    runtime: &tokio::runtime::Handle,
    future: F,
) -> Result<F::Output, ExecutionError>
where
    F: std::future::Future + Send,
    F::Output: Send,
{
    if runtime.runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread {
        return Err(failed(
            "pane owner requires a persistent multi-thread runtime",
        ));
    }
    // Caller runtimes are transient; pane tasks inherit the persistent owner.
    if tokio::runtime::Handle::try_current()
        .is_ok_and(|current| current.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
    {
        Ok(tokio::task::block_in_place(|| runtime.block_on(future)))
    } else {
        std::thread::scope(|scope| scope.spawn(|| runtime.block_on(future)).join())
            .map_err(|_| failed("pane startup bridge thread panicked"))
    }
}

async fn await_startup(prepared: &mut super::PreparedFreshRuntime) -> Result<(), ExecutionError> {
    match tokio::time::timeout(Duration::from_secs(10), prepared.wait_until_ready()).await {
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

pub(crate) fn send(reference: &ExecutionRef, data: Vec<u8>) -> Result<(), ExecutionError> {
    if data.len() > 65_536 {
        return Err(ExecutionError::InvalidOptions {
            reason: "send exceeds 65536 bytes".into(),
        });
    }
    with_pane(reference, |pane| {
        if pane.exited.load(Ordering::SeqCst) {
            return Err(failed("execution has exited"));
        }
        let permit = Arc::clone(&pane.execution.admission)
            .try_acquire_owned()
            .map_err(failed)?;
        let recording = super::pane_input_recording_data(&data);
        pane.input_tx
            .send(PaneRuntimeCommand::GuardedInput(data, permit))
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

pub(crate) fn resize_ordered(
    reference: &ExecutionRef,
    cols: u16,
    rows: u16,
) -> Result<(), ExecutionError> {
    with_pane(reference, |pane| {
        if pane
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
    resize(reference, cols, rows)
}

pub(crate) fn resize(reference: &ExecutionRef, cols: u16, rows: u16) -> Result<(), ExecutionError> {
    validate_size(cols, rows)?;
    with_pane(reference, |pane| {
        if pane.exited.load(Ordering::SeqCst) {
            return Err(failed("execution has exited"));
        }
        let permit = Arc::clone(&pane.execution.admission)
            .try_acquire_owned()
            .map_err(failed)?;
        let geometry_revision = pane.execution.geometry_revision.load(Ordering::SeqCst);
        pane.resize_pty_admitted(rows, cols, Some(permit));
        if pane.execution.geometry_revision.load(Ordering::SeqCst) == geometry_revision
            || *pane.last_requested_size.lock().map_err(failed)? != (rows, cols)
        {
            return Err(failed("terminal geometry could not be committed"));
        }
        Ok(())
    })
}

pub(crate) fn wait(
    reference: &ExecutionRef,
    timeout_ms: u32,
    authorize: impl Fn() -> Result<(), ExecutionError> + Sync,
) -> Result<ExecutionStatus, ExecutionError> {
    if timeout_ms > 30_000 {
        return Err(ExecutionError::InvalidOptions {
            reason: "wait timeout exceeds 30000ms; poll again".into(),
        });
    }
    let owner = spawn_runtime_handle()
        .ok_or_else(|| failed("pane execution runtime is not registered for wait"))?;
    drive_startup(&owner.1, async {
        // Subscribe before inspecting authority: a change racing the initial
        // snapshot remains pending, even before changed() is first awaited.
        let mut changes = with_pane(reference, |pane| Ok(pane.execution.subscribe()))?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(u64::from(timeout_ms));
        loop {
            authorize()?;
            let current = status(reference)?;
            if current.error.is_some()
                || (!matches!(current.phase, ExecutionPhase::Running)
                    && current.output_drained
                    && current.terminal_session_ended)
                || tokio::time::Instant::now() >= deadline
            {
                return Ok(current);
            }
            tokio::time::timeout_at(deadline, changes.changed())
                .await
                .map_or(Ok(()), |result| result.map_err(failed))?;
        }
    })?
}

pub(crate) fn terminate(
    reference: &ExecutionRef,
    grace_ms: u32,
    authorize: impl Fn() -> Result<(), ExecutionError> + Sync,
) -> Result<ExecutionStatus, ExecutionError> {
    let owner = spawn_runtime_handle()
        .ok_or_else(|| failed("pane execution runtime is not registered for termination"))?;
    drive_startup(&owner.1, async {
        if grace_ms > 30_000 {
            return Err(ExecutionError::InvalidOptions {
                reason: "termination grace exceeds 30000ms".into(),
            });
        }
        let mut changes = with_pane(reference, |pane| Ok(pane.execution.subscribe()))?;
        let signal = |signal| {
            authorize()?;
            with_pane(reference, |pane| {
                pane.execution.signal_owned_group(signal).map_err(failed)
            })
        };
        signal(libc::SIGTERM)?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(u64::from(grace_ms));
        loop {
            authorize()?;
            let settled = with_pane(reference, |pane| {
                Ok(pane.exited.load(Ordering::SeqCst)
                    && pane.execution.output_drained.load(Ordering::SeqCst))
            })?;
            if settled || tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::timeout_at(deadline, changes.changed())
                .await
                .map_or(Ok(()), |result| result.map_err(failed))?;
        }
        // Failed phase is not evidence of process death.
        signal(GROUP_KILL)?;
        await_cleanup_acknowledgement(reference, &authorize).await?;
        authorize()?;
        status(reference)
    })?
}

async fn await_cleanup_acknowledgement(
    reference: &ExecutionRef,
    authorize: impl Fn() -> Result<(), ExecutionError>,
) -> Result<(), ExecutionError> {
    let mut cleanup = with_pane(reference, |pane| Ok(pane.cleanup.clone()))?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        authorize()?;
        // Revalidate the generation and session after every wake.
        with_pane(reference, |_| Ok(()))?;
        match cleanup.borrow_and_update().clone() {
            Some(Ok(())) => return Ok(()),
            Some(Err(error)) => return Err(failed(error)),
            None => {}
        }
        if tokio::time::timeout_at(deadline, cleanup.changed())
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

pub(super) fn send_acknowledged(
    reference: &ExecutionRef,
    data: Vec<u8>,
) -> Result<(), ExecutionError> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    with_pane(reference, |pane| {
        if pane.exited.load(Ordering::SeqCst) {
            return Err(failed("execution exited before input admission"));
        }
        let permit = Arc::clone(&pane.execution.admission)
            .try_acquire_owned()
            .map_err(failed)?;
        pane.input_tx
            .send(PaneRuntimeCommand::AdmittedInput(data, sender, permit))
            .map_err(failed)
    })?;
    receiver
        .recv_timeout(Duration::from_secs(5))
        .map_err(failed)?
        .map_err(failed)
}

fn install_reserved_capture(
    prepared: &super::PreparedFreshRuntime,
    reserved: Option<&ExecutionRef>,
) -> Result<(), ExecutionError> {
    if let Some(reference) = reserved {
        let capture = super::reservations::create_capture(reference)
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
    #[test]
    fn concurrent_termination_after_natural_retirement_does_not_dispatch() {
        let capture = std::sync::Arc::new(super::ExecutionCapture::default());
        let fence = capture.supervisor.lock().unwrap();
        let first = std::sync::Arc::clone(&capture);
        let second = std::sync::Arc::clone(&capture);
        let a = std::thread::spawn(move || first.signal_owned_group(super::GROUP_KILL));
        let b = std::thread::spawn(move || second.signal_owned_group(libc::SIGTERM));
        // Same fence used by natural retirement and side-effect authorization.
        capture
            .terminal_owner_retired
            .store(true, super::Ordering::SeqCst);
        drop(fence);
        assert!(!a.join().unwrap().unwrap());
        assert!(!b.join().unwrap().unwrap());
    }

    #[tokio::test]
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

    #[tokio::test]
    async fn simultaneous_waiters_observe_event_before_first_wait() {
        let capture = super::ExecutionCapture::default();
        let mut first = capture.subscribe();
        let mut second = capture.subscribe();
        capture.notify();
        let (a, b) = tokio::join!(first.changed(), second.changed());
        a.unwrap();
        b.unwrap();
        assert!(!first.has_changed().unwrap());
        assert!(!second.has_changed().unwrap());
    }

    #[tokio::test]
    async fn idle_capture_has_only_one_deadline_and_no_lifecycle_wake() {
        let capture = super::ExecutionCapture::default();
        let mut changes = capture.subscribe();
        let deadline = tokio::time::Instant::now() + super::Duration::from_millis(20);
        assert!(
            tokio::time::timeout_at(deadline, changes.changed())
                .await
                .is_err()
        );
        assert!(!changes.has_changed().unwrap());
        assert!(!capture.output_drained.load(super::Ordering::SeqCst));
        assert!(capture.outcome.lock().unwrap().0.is_none());
    }

    #[tokio::test]
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
        capture.record_cleanup_error("cleanup failed".into());
        changes.changed().await.unwrap();
        assert!(capture.cleanup_failed.load(super::Ordering::SeqCst));
        assert_eq!(
            capture.outcome.lock().unwrap().2.as_deref(),
            Some("reader failed")
        );
    }

    #[test]
    fn current_thread_control_preserves_cancellation_error() {
        let owner = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let caller = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let cancelled = std::sync::atomic::AtomicBool::new(true);
        let authorize = || {
            assert!(cancelled.load(super::Ordering::SeqCst));
            Err::<(), _>(super::ExecutionError::OperationFailed {
                reason: "execution request cancelled".into(),
            })
        };
        let result = caller.block_on(async {
            super::drive_startup(owner.handle(), async { authorize() }).unwrap()
        });
        assert!(
            matches!(result, Err(super::ExecutionError::OperationFailed { reason }) if reason == "execution request cancelled")
        );
    }

    #[test]
    fn current_thread_caller_keeps_tasks_on_persistent_owner() {
        let owner = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let caller = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        caller.block_on(async {
            super::drive_startup(owner.handle(), async {
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    sender.send(()).unwrap();
                });
            })
            .unwrap();
        });
        drop(caller);
        owner.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(2), receiver)
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
            session_id: uuid::Uuid::new_v4(),
            pane_id: uuid::Uuid::new_v4(),
            execution_token: uuid::Uuid::new_v4(),
        };
        assert!(validate_token(reference.execution_token, &reference).is_ok());
        assert_eq!(
            validate_token(uuid::Uuid::new_v4(), &reference),
            Err(ExecutionError::StaleExecution)
        );
    }

    #[test]
    fn program_denial_precedes_filesystem_and_runtime_observation() {
        let options = ProgramOptions {
            program: String::new(),
            args: Vec::new(),
            cwd: Some("/not-present".into()),
            env: std::collections::BTreeMap::new(),
            cols: 0,
            rows: 0,
        };
        assert!(matches!(
            create(None, options, |_| Err(anyhow::anyhow!("denied"))),
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
        capture.record_exit(&Ok(portable_pty::ExitStatus::with_exit_code(0)));
        assert!(!capture.output_drained.load(Ordering::SeqCst));
        assert_eq!(capture.outcome.lock().unwrap().0, Some(0));
        capture.output_drained.store(true, Ordering::SeqCst);
        assert!(capture.output_drained.load(Ordering::SeqCst));
    }
}
