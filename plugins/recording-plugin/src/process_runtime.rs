//! Typed process orchestration. This module never owns a PTY engine.
use anyhow::{Context, Result, ensure};
use bmux_pane_runtime_plugin_api::pane_runtime_execution_v4 as pane;
use bmux_plugin::{ServiceCallerDispatchClient, TypedServiceCaller};
use bmux_recording_plugin_api::recording_process_v4 as process;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use crate::process_store::ProcessStore;
// pane-runtime-execution-v4 durable capture rejects requests above 256 KiB.
// Keep repair and verification on the same bounded request path.
const CAPTURE_READ_MAX_BYTES: u32 = 262_144;
async fn capture_resume(
    client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
    cursor: pane::TerminalCaptureCursor,
) -> Result<pane::TerminalCaptureResponse> {
    pane::client::capture_resume(client, cursor, CAPTURE_READ_MAX_BYTES)
        .await?
        .map_err(|error| anyhow::anyhow!("canonical capture read: {error:?}"))
}
async fn reserve_program(
    client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
    environment: Option<
        Vec<bmux_pane_runtime_plugin_api::pane_runtime_environment_v1::EnvironmentEntry>,
    >,
    mutation: Uuid,
    options: pane::TerminalProgramOptions,
) -> bmux_plugin_sdk::TypedServiceClientResult<
    Result<pane::TerminalLaunchReservation, pane::TerminalRecoveryError>,
> {
    if let Some(environment) = environment {
        bmux_pane_runtime_plugin_api::pane_runtime_environment_v1::client::reserve(
            client,
            mutation,
            options,
            environment,
        )
        .await
    } else {
        pane::client::reserve(client, mutation, options).await
    }
}
fn launch_intent(
    id: Uuid,
    mutation: Uuid,
    options: &process::ProgramOptions,
    root: &str,
    environment: Option<
        &[bmux_pane_runtime_plugin_api::pane_runtime_environment_v1::EnvironmentEntry],
    >,
) -> Result<Vec<u8>> {
    if environment.is_some() {
        use sha2::{Digest, Sha256};
        Ok(Sha256::digest(serde_json::to_vec(&(
            "complete-environment-v1",
            id,
            mutation,
            options,
            root,
            environment,
        ))?)
        .to_vec())
    } else {
        Ok(serde_json::to_vec(&(id, mutation, options, root))?)
    }
}

type Reply<'a, T> = Pin<Box<dyn Future<Output = Result<T, process::ProcessError>> + Send + 'a>>;

static PROCESS_SERVICE: std::sync::OnceLock<Arc<ProcessService>> = std::sync::OnceLock::new();

pub(crate) fn register(service: Arc<ProcessService>) {
    let _ = PROCESS_SERVICE.set(service);
}

pub(crate) fn dispatch<T: Send + 'static>(
    context: &bmux_plugin_sdk::NativeServiceContext,
    operation: impl FnOnce(Arc<ProcessService>) -> Pin<Box<dyn Future<Output = T> + Send>>
    + Send
    + 'static,
) -> Result<T> {
    let service = Arc::clone(
        PROCESS_SERVICE
            .get()
            .context("process service unavailable")?,
    );
    let service = Arc::new(ProcessService {
        caller: Arc::new(TypedServiceCaller::from_service_context(context)),
        root: service.root.clone(),
        runtime_root: service.runtime_root.clone(),
        isolated: service.isolated,
        mutation: Arc::clone(&service.mutation),
    });
    drive_process_operation(move || operation(service))
}

/// Native dispatch is synchronous, but may be entered on the server's async
/// worker. Hand its scheduler core off before joining the private driver thread
/// so timers and I/O on the original runtime can still wake nested service waits.
fn drive_process_operation<T, F, Fut>(operation: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = T> + Send + 'static,
{
    let drive = move || {
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            Ok::<T, anyhow::Error>(runtime.block_on(operation()))
        })
        .join()
        .map_err(|_| anyhow::anyhow!("process service dispatch panicked"))?
    };
    if tokio::runtime::Handle::try_current()
        .is_ok_and(|runtime| runtime.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
    {
        tokio::task::block_in_place(drive)
    } else {
        // No-runtime and current-thread callers cannot use block_in_place.
        // The owned driver still has its own timer/I/O context; it never relies
        // on the blocked current-thread caller to drive its operation.
        drive()
    }
}

pub(crate) struct ProcessService {
    pub(super) caller: Arc<TypedServiceCaller>,
    root: PathBuf,
    runtime_root: String,
    isolated: bool,
    mutation: Arc<Mutex<()>>,
}

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq)]
struct OriginalWriterClosure {
    version: u32,
    run_id: Uuid,
    recording_id: Uuid,
    execution: process::ExecutionRef,
    watermark: u64,
    manifest_hash: Vec<u8>,
    segments: Vec<(String, Vec<u8>)>,
}

fn closed_original_events(
    directory: &std::path::Path,
    value: &process::RunStatus,
    watermark: u64,
) -> Result<OriginalWriterClosure> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let bytes = std::fs::read(directory.join("manifest.json"))?;
    let manifest: crate::recording_runtime::Manifest = serde_json::from_slice(&bytes)?;
    ensure!(
        manifest.summary.id == value.recording_id && manifest.summary.ended_epoch_ms.is_some(),
        "original writer manifest is not closed for this recording"
    );
    let mut count = 0u64;
    let mut seq = 0u64;
    let mut timestamp = 0u64;
    let mut segments = Vec::new();
    for name in &manifest.summary.segments {
        ensure!(
            std::path::Path::new(name).components().count() == 1 && !name.starts_with('.'),
            "invalid original event segment"
        );
        let mut file = std::fs::File::open(directory.join(name))?;
        let mut digest = Sha256::new();
        loop {
            let mut prefix = [0; 4];
            if file.read(&mut prefix[..1])? == 0 {
                break;
            }
            file.read_exact(&mut prefix[1..])?;
            let length = usize::try_from(u32::from_le_bytes(prefix))?;
            ensure!(
                length <= 16 * 1024 * 1024,
                "original event frame exceeds bounded limit"
            );
            let mut payload = vec![0; length];
            file.read_exact(&mut payload)?;
            let event: bmux_recording_protocol::RecordingEventEnvelope<
                bmux_ipc::Event,
                bmux_ipc::ErrorCode,
            > = bmux_codec::from_bytes(&payload)?;
            ensure!(
                event.seq == seq + 1 && event.mono_ns >= timestamp,
                "original event sequence/timing integrity failure"
            );
            seq = event.seq;
            timestamp = event.mono_ns;
            count += 1;
            digest.update(prefix);
            digest.update(payload);
        }
        segments.push((name.clone(), digest.finalize().to_vec()));
    }
    ensure!(
        count == manifest.summary.event_count,
        "original event count differs from closed manifest"
    );
    Ok(OriginalWriterClosure {
        version: 1,
        run_id: value.run_id,
        recording_id: value.recording_id,
        execution: value
            .execution
            .clone()
            .context("original closure lacks execution identity")?,
        watermark,
        manifest_hash: Sha256::digest(bytes).to_vec(),
        segments,
    })
}

fn validate_projected_output(directory: &Path) -> Result<()> {
    use std::io::Read;
    let manifest: crate::recording_runtime::Manifest =
        serde_json::from_slice(&std::fs::read(directory.join("manifest.json"))?)?;
    let mut projected = std::fs::File::open(directory.join("output.live"))?;
    for name in &manifest.summary.segments {
        ensure!(
            Path::new(name).components().count() == 1 && !name.starts_with('.'),
            "invalid original event segment"
        );
        let mut segment = std::fs::File::open(directory.join(name))?;
        loop {
            let mut prefix = [0; 4];
            if segment.read(&mut prefix[..1])? == 0 {
                break;
            }
            segment.read_exact(&mut prefix[1..])?;
            let length = usize::try_from(u32::from_le_bytes(prefix))?;
            ensure!(
                length <= 16 * 1024 * 1024,
                "original event frame exceeds bounded limit"
            );
            let mut bytes = vec![0; length];
            segment.read_exact(&mut bytes)?;
            let event: crate::process_artifacts::ProcessRecordingEvent =
                bmux_codec::from_bytes(&bytes)?;
            if event.kind == bmux_recording_protocol::RecordingEventKind::PaneOutputRaw
                && let bmux_recording_protocol::RecordingPayload::Bytes { data } = event.payload
            {
                let mut actual = vec![0; data.len()];
                projected.read_exact(&mut actual).context("recording raw boundary differs from execution authority: projected event stream truncated")?;
                ensure!(
                    actual == data,
                    "recording raw boundary differs from execution authority: projected event bytes differ"
                );
            }
        }
    }
    let mut extra = [0];
    ensure!(
        projected.read(&mut extra)? == 0,
        "recording raw boundary differs from execution authority: projected stream has extra bytes"
    );
    Ok(())
}

fn original_writer_closed(directory: &std::path::Path, value: &process::RunStatus) -> Result<bool> {
    let path = directory.join("original-writer-closure.json");
    if !path.try_exists()? {
        return Ok(false);
    }
    let receipt: OriginalWriterClosure = serde_json::from_slice(&std::fs::read(path)?)?;
    ensure!(
        receipt.version == 1,
        "unsupported original writer closure version"
    );
    ensure!(
        receipt.watermark == value.output_watermark,
        "original writer closure watermark mismatch"
    );
    ensure!(
        receipt == closed_original_events(directory, value, receipt.watermark)?,
        "original writer closure identity/event integrity mismatch"
    );
    Ok(true)
}

pub(crate) fn verified_launch_root(request: &str, owner: &str) -> Result<String> {
    // The admitted spelling is stable request identity; the filesystem locator
    // may have an alias (e.g. macOS /tmp). Verify the same existing owned root,
    // then retain the original owner spelling in the durable intent.
    let requested = std::fs::canonicalize(request).context("resolving requested runtime root")?;
    let canonical_owner = std::fs::canonicalize(owner).context("resolving owned runtime root")?;
    ensure!(
        requested == canonical_owner,
        "isolated runtime locator mismatch"
    );
    Ok(owner.to_owned())
}

impl ProcessService {
    pub(crate) fn new(
        caller: Arc<TypedServiceCaller>,
        root: PathBuf,
        runtime_root: String,
        isolated: bool,
    ) -> Self {
        Self {
            caller,
            root,
            runtime_root,
            isolated,
            mutation: Arc::new(Mutex::new(())),
        }
    }

    async fn execution_status(
        &self,
        client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
        reference: pane::TerminalExecutionRef,
        wait: Option<u32>,
    ) -> Result<pane::TerminalExecutionStatus> {
        let result = match wait {
            Some(timeout) => pane::client::wait(client, reference, timeout).await?,
            None => pane::client::status(client, reference).await?,
        };
        result.map_err(|error| anyhow::anyhow!("execution status: {error:?}"))
    }
    async fn execution_screen(
        &self,
        client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
        reference: pane::TerminalExecutionRef,
    ) -> Result<String> {
        bmux_pane_runtime_plugin_api::pane_runtime_pointer_grid_v1::client::screen(
            client, 1, reference,
        )
        .await?
        .map(|value| value.text)
        .map_err(|error| anyhow::anyhow!("screen: {error:?}"))
    }
    async fn execution_terminate(
        &self,
        client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
        reference: pane::TerminalExecutionRef,
        grace: u32,
    ) -> Result<()> {
        pane::client::terminate(client, reference, grace)
            .await?
            .map_err(|error| anyhow::anyhow!("termination: {error:?}"))?;
        Ok(())
    }
    fn store(&self) -> Result<ProcessStore> {
        ProcessStore::new(&self.root)
    }

    fn require_isolation(&self) -> Result<()> {
        ensure!(
            self.isolated,
            "process operations require an explicitly dedicated isolated runtime"
        );
        Ok(())
    }

    pub(super) fn load(&self, id: Uuid) -> Result<process::RunStatus> {
        self.require_isolation()?;
        let value = ProcessStore::existing(&self.root).load(id)?;
        ensure!(
            value.runtime_root == self.runtime_root,
            "run belongs to a different isolated runtime"
        );
        Ok(value)
    }

    pub(crate) async fn human_capabilities(
        &self,
    ) -> Result<bmux_pane_runtime_plugin_api::pane_runtime_human_input_v1::HumanInputCapabilities>
    {
        let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
        Ok(
            bmux_pane_runtime_plugin_api::pane_runtime_human_input_v1::client::capabilities(
                &mut client,
            )
            .await?,
        )
    }

    fn human_reference(
        &self,
        run_id: Uuid,
    ) -> Result<
        pane::TerminalExecutionRef,
        bmux_pane_runtime_plugin_api::pane_runtime_human_input_v1::HumanInputError,
    > {
        self.load(run_id)
            .and_then(|value| execution(&value))
            .map_err(|error| {
                bmux_pane_runtime_plugin_api::pane_runtime_human_input_v1::HumanInputError::Closed {
                    reason: error.to_string(),
                }
            })
    }

    pub(crate) async fn human_open(
        &self,
        run_id: Uuid,
    ) -> Result<
        bmux_pane_runtime_plugin_api::pane_runtime_human_input_v1::HumanInputStream,
        bmux_pane_runtime_plugin_api::pane_runtime_human_input_v1::HumanInputError,
    > {
        use bmux_pane_runtime_plugin_api::pane_runtime_human_input_v1 as human;
        let reference = self.human_reference(run_id)?;
        let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
        human::client::open(&mut client, reference)
            .await
            .map_err(|error| human::HumanInputError::Closed {
                reason: error.to_string(),
            })?
    }

    pub(crate) async fn human_push(
        &self,
        run_id: Uuid,
        stream: bmux_pane_runtime_plugin_api::pane_runtime_human_input_v1::HumanInputStream,
        sequence: u64,
        data: Vec<u8>,
    ) -> Result<
        bmux_pane_runtime_plugin_api::pane_runtime_human_input_v1::HumanInputAck,
        bmux_pane_runtime_plugin_api::pane_runtime_human_input_v1::HumanInputError,
    > {
        use bmux_pane_runtime_plugin_api::pane_runtime_human_input_v1 as human;
        if self.human_reference(run_id)? != stream.execution {
            return Err(human::HumanInputError::Authority {
                error: pane::TerminalExecutionError::StaleExecution,
            });
        }
        let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
        human::client::push(&mut client, stream, sequence, data)
            .await
            .map_err(|error| human::HumanInputError::Uncertain {
                reason: error.to_string(),
            })?
    }

    pub(crate) async fn human_close(
        &self,
        run_id: Uuid,
        stream: bmux_pane_runtime_plugin_api::pane_runtime_human_input_v1::HumanInputStream,
    ) -> Result<(), bmux_pane_runtime_plugin_api::pane_runtime_human_input_v1::HumanInputError>
    {
        use bmux_pane_runtime_plugin_api::pane_runtime_human_input_v1 as human;
        if self.human_reference(run_id)? != stream.execution {
            return Err(human::HumanInputError::Authority {
                error: pane::TerminalExecutionError::StaleExecution,
            });
        }
        let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
        human::client::close(&mut client, stream)
            .await
            .map_err(|error| human::HumanInputError::Closed {
                reason: error.to_string(),
            })?
    }

    pub(crate) async fn wait_capabilities(
        &self,
    ) -> Result<bmux_pane_runtime_plugin_api::pane_runtime_presentation_wait_v1::WaitCapabilities>
    {
        let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
        Ok(
            bmux_pane_runtime_plugin_api::pane_runtime_presentation_wait_v1::client::capabilities(
                &mut client,
            )
            .await?,
        )
    }

    pub(crate) async fn wait_present(
        &self,
        run_id: Uuid,
        after: Option<Vec<u8>>,
        timeout_ms: u32,
        max_bytes: u32,
    ) -> Result<
        Option<bmux_pane_runtime_plugin_api::pane_runtime_presentation_wait_v1::PresentationFrame>,
        bmux_pane_runtime_plugin_api::pane_runtime_presentation_wait_v1::WaitError,
    > {
        use bmux_pane_runtime_plugin_api::pane_runtime_presentation_wait_v1::WaitError;
        let unavailable = |error: anyhow::Error| WaitError::Authority {
            error: pane::TerminalExecutionError::OperationFailed {
                reason: error.to_string(),
            },
        };
        // Resolve durable ownership only; never refresh capture/status in this lane.
        let value = self.load(run_id).map_err(unavailable)?;
        let reference = execution(&value).map_err(unavailable)?;
        let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
        let frame =
            bmux_pane_runtime_plugin_api::pane_runtime_pointer_grid_v1::client::wait_present(
                &mut client,
                1,
                reference.clone(),
                after,
                timeout_ms,
                max_bytes,
            )
            .await
            .map_err(|error| unavailable(error.into()))??;
        if frame
            .as_ref()
            .is_some_and(|frame| frame.screen.execution != reference)
        {
            return Err(WaitError::Authority {
                error: pane::TerminalExecutionError::StaleExecution,
            });
        }
        Ok(frame)
    }

    fn retain_launch_outcome(
        &self,
        id: Uuid,
        receipt: &pane::TerminalLaunchReceipt,
    ) -> Result<process::RunStatus> {
        let guard = self
            .mutation
            .lock()
            .map_err(|_| anyhow::anyhow!("process authority lock poisoned"))?;
        let store = self.store()?;
        let mut value = store.load(id)?;
        let previous = serde_json::to_vec(&value)?;
        if !value.capture_finalized {
            retain_incomplete_launch(&mut value, receipt);
        }
        if serde_json::to_vec(&value)? != previous {
            store.replace(value.revision, &mut value)?;
        }
        drop(guard);
        Ok(value)
    }

    async fn refresh(&self, id: Uuid) -> Result<process::RunStatus> {
        let mut value = self.load(id)?;
        if value.capture_finalized {
            let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
            return refresh_finalized_cleanup(&mut client, &self.store()?, &self.mutation, value)
                .await;
        }
        if value.execution.is_none() {
            let reservation = self.store()?.reservation(id).context("launch admission retained but reservation outcome unknown; recovery required; refusing relaunch")?;
            value.execution = Some(process::ExecutionRef {
                session_id: reservation.execution.session_id,
                pane_id: reservation.execution.pane_id,
                execution_token: reservation.execution.execution_token,
            });
            self.store()?.replace(value.revision, &mut value)?;
        }
        let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
        let mut authority = pane::client::reconcile(&mut client, execution(&value)?)
            .await?
            .map_err(|error| anyhow::anyhow!("reconcile: {error:?}"))?;
        if matches!(authority.launch.phase, pane::TerminalLaunchPhase::Reserved) {
            let reservation = self.store()?.reservation(id)?;
            pane::client::launch(&mut client, reservation)
                .await?
                .map_err(|error| anyhow::anyhow!("reserved launch: {error:?}"))?;
            authority = pane::client::reconcile(&mut client, execution(&value)?)
                .await?
                .map_err(|error| anyhow::anyhow!("reconcile: {error:?}"))?;
        }
        self.store()?.write_authority(id, &authority)?;
        if retain_incomplete_launch(&mut value, &authority.launch) {
            return self.retain_launch_outcome(id, &authority.launch);
        }
        let capture_active = crate::manual_handle()
            .and_then(|handle| {
                handle.read().ok().and_then(|handle| {
                    handle
                        .0
                        .lock()
                        .ok()
                        .and_then(|runtime| runtime.active_capture_target())
                })
            })
            .is_some_and(|(recording, _)| recording == value.recording_id);
        let original_closed =
            original_writer_closed(&self.root.join(value.recording_id.to_string()), &value)?;
        if !capture_active && !original_closed {
            self.recover_raw(&value, &authority).await?;
        }
        let status = self
            .execution_status(&mut client, execution(&value)?, None)
            .await?;
        let guard = self
            .mutation
            .lock()
            .map_err(|_| anyhow::anyhow!("process authority lock poisoned"))?;
        let store = self.store()?;
        value = store.load(id)?;
        let previous = serde_json::to_vec(&value)?;
        if !value.capture_finalized {
            let revision = value.revision;
            let phase = match status.phase {
                pane::TerminalExecutionPhase::Running => process::RunPhase::Running,
                pane::TerminalExecutionPhase::Exited => process::RunPhase::Exited,
                pane::TerminalExecutionPhase::Failed => process::RunPhase::Failed,
            };
            value.phase = phase;
            value.exit_code = status.exit_code;
            value.target_exited = target_exit_observed(&status);
            value.terminal_session_ended = status.terminal_session_ended;
            value.capture_eof = authority.capture_complete;
            value.recovery_required = authority.recovery_required;
            if !capture_active && !original_closed {
                value.timing_complete = false;
                value.recovery_required = true;
            }
            value.cleanup_complete = status.cleanup_complete;
            value.capture_watermark = authority.continuation_cursor.offset;
            value.output_drained = status.output_drained
                && status.terminal_session_ended
                && matches!(
                    status.capture_integrity,
                    pane::TerminalCaptureIntegrity::Complete
                );
            value.output_watermark = status.output_watermark;
            if let Some(error) = status.error {
                value.errors.push(error);
            }
            if serde_json::to_vec(&value)? != previous {
                store.replace(revision, &mut value)?;
            }
        }
        drop(guard);
        Ok(value)
    }

    async fn recover_raw(
        &self,
        value: &process::RunStatus,
        authority: &pane::TerminalReconciliation,
    ) -> Result<()> {
        let store = self.store()?;
        let reservation = store.reservation(value.run_id)?;
        let mut cursor = store
            .capture_cursor(value.run_id)?
            .unwrap_or(reservation.initial_cursor);
        ensure!(
            cursor.offset <= authority.continuation_cursor.offset,
            "capture cursor beyond authority"
        );
        let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
        for _ in 0..64 {
            if cursor.offset == authority.continuation_cursor.offset {
                return Ok(());
            }
            let response = capture_resume(&mut client, cursor.clone()).await?;
            match response {
                pane::TerminalCaptureResponse::Batch { value: batch } => {
                    ensure!(
                        batch.cursor == cursor && batch.continuation.offset > cursor.offset,
                        "capture batch did not advance exact cursor"
                    );
                    store.commit_raw_batch(value.run_id, &batch)?;
                    cursor = batch.continuation;
                }
                pane::TerminalCaptureResponse::Gap { value: gap } => anyhow::bail!(
                    "canonical capture gap: {}; requested={} oldest={}; cannot fabricate timing/events",
                    gap.reason,
                    gap.requested.offset,
                    gap.oldest.offset
                ),
            }
        }
        anyhow::bail!(
            "raw recovery bounded batch budget exhausted; retry to continue committed cursor"
        )
    }

    async fn retry_launch(
        &self,
        id: Uuid,
        launch_mutation_id: Uuid,
        options: process::ProgramOptions,
        environment: Option<
            Vec<bmux_pane_runtime_plugin_api::pane_runtime_environment_v1::EnvironmentEntry>,
        >,
    ) -> Result<process::RunStatus> {
        let store = self.store()?;
        let mut existing = store.load(id).context("admitted launch has no committed run metadata; recovery required; refusing blind relaunch")?;
        if existing.execution.is_none() {
            let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
            // Reserve is idempotent against the exact persisted mutation/options;
            // it cannot spawn. Lost reserve replies recover the same identity.
            let reservation = reserve_program(
                &mut client,
                environment,
                launch_mutation_id,
                pane::TerminalProgramOptions {
                    program: options.program,
                    args: options.args,
                    cwd: options.cwd,
                    env: options.env,
                    cols: options.cols,
                    rows: options.rows,
                },
            )
            .await?
            .map_err(|error| anyhow::anyhow!("reserve reconciliation: {error:?}"))?;
            store.write_reservation(id, &reservation)?;
            existing.execution = Some(process::ExecutionRef {
                session_id: reservation.execution.session_id,
                pane_id: reservation.execution.pane_id,
                execution_token: reservation.execution.execution_token,
            });
            store.replace(existing.revision, &mut existing)?;
        }
        self.refresh(id).await
    }

    fn initial_status(
        id: Uuid,
        launch_mutation_id: Uuid,
        recording_id: Uuid,
        runtime_root: String,
    ) -> process::RunStatus {
        process::RunStatus {
            schema_version: 4,
            run_id: id,
            revision: 1,
            launch_mutation_id,
            recording_id,
            execution: None,
            runtime_root,
            phase: process::RunPhase::Starting,
            exit_code: None,
            target_exited: false,
            terminal_session_ended: false,
            capture_eof: false,
            capture_watermark: 0,
            recording_watermark: 0,
            durable_recording_complete: false,
            timing_complete: true,
            cleanup_complete: false,
            recovery_required: false,
            output_drained: false,
            output_watermark: 0,
            capture_finalized: false,
            errors: vec![],
        }
    }

    fn validate_options(options: &process::ProgramOptions) -> Result<()> {
        ensure!(
            !options.program.is_empty() && options.cols > 0 && options.rows > 0,
            "invalid program or viewport"
        );
        ensure!(
            u32::from(options.cols) * u32::from(options.rows) <= 262_144,
            "viewport exceeds bounded capture geometry"
        );
        ensure!(
            options.args.len() <= 4096 && options.env.len() <= 4096,
            "program options exceed bounds"
        );
        Ok(())
    }

    pub(crate) async fn environment_capabilities(&self) -> Result<u32> {
        let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
        Ok(
            bmux_pane_runtime_plugin_api::pane_runtime_environment_v1::client::capabilities(
                &mut client,
            )
            .await?,
        )
    }
    pub(crate) async fn start_environment(
        &self,
        mutation: Uuid,
        options: process::ProgramOptions,
        environment: Vec<
            bmux_pane_runtime_plugin_api::pane_runtime_environment_v1::EnvironmentEntry,
        >,
    ) -> Result<process::RunStatus> {
        ensure!(
            self.environment_capabilities().await? == 2,
            "unsupported complete environment capability"
        );
        self.launch_environment(
            mutation,
            mutation,
            options,
            self.runtime_root.clone(),
            Some(environment),
        )
        .await
    }

    async fn launch(
        &self,
        id: Uuid,
        launch_mutation_id: Uuid,
        options: process::ProgramOptions,
        runtime_root: String,
    ) -> Result<process::RunStatus> {
        self.launch_environment(id, launch_mutation_id, options, runtime_root, None)
            .await
    }

    async fn launch_environment(
        &self,
        id: Uuid,
        launch_mutation_id: Uuid,
        options: process::ProgramOptions,
        runtime_root: String,
        environment: Option<
            Vec<bmux_pane_runtime_plugin_api::pane_runtime_environment_v1::EnvironmentEntry>,
        >,
    ) -> Result<process::RunStatus> {
        self.require_isolation()?;
        let runtime_root = verified_launch_root(&runtime_root, &self.runtime_root)?;
        Self::validate_options(&options)?;
        let store = self.store()?;
        let intent = launch_intent(
            id,
            launch_mutation_id,
            &options,
            &runtime_root,
            environment.as_deref(),
        )?;
        if store.admit_launch(id, launch_mutation_id, &intent)? {
            return self
                .retry_launch(id, launch_mutation_id, options, environment)
                .await;
        }
        let recording = self.prepare_capture(&options)?;
        let mut value = Self::initial_status(id, launch_mutation_id, recording.id, runtime_root);
        // Acknowledged metadata precedes launch; failure to persist prevents execution.
        if let Err(error) = self.store()?.create(&value) {
            let _ = stop_capture(recording.id);
            return Err(error);
        }
        let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
        let reservation = reserve_program(
            &mut client,
            environment,
            launch_mutation_id,
            pane::TerminalProgramOptions {
                program: options.program,
                args: options.args,
                cwd: options.cwd,
                env: options.env,
                cols: options.cols,
                rows: options.rows,
            },
        )
        .await?
        .map_err(|error| anyhow::anyhow!("reserve: {error:?}"))?;
        store.write_reservation(id, &reservation)?;
        value.execution = Some(process::ExecutionRef {
            session_id: reservation.execution.session_id,
            pane_id: reservation.execution.pane_id,
            execution_token: reservation.execution.execution_token,
        });
        store.replace(1, &mut value)?;
        let launched = pane::client::launch(&mut client, reservation).await;
        match launched {
            Ok(Ok(receipt)) => {
                let reference = receipt.reservation.execution.clone();
                value.execution = Some(process::ExecutionRef {
                    session_id: reference.session_id,
                    pane_id: reference.pane_id,
                    execution_token: reference.execution_token,
                });
                value.phase = process::RunPhase::Running;
                retain_incomplete_launch(&mut value, &receipt);
            }
            Err(error) => {
                value.phase = process::RunPhase::RecoveryRequired;
                value.recovery_required = true;
                value.errors.push(format!(
                    "native launch outcome uncertain; recovery required; refusing relaunch: {error}"
                ));
                let guard = self
                    .mutation
                    .lock()
                    .map_err(|_| anyhow::anyhow!("process authority lock poisoned"))?;
                self.store()?.replace(value.revision, &mut value)?;
                drop(guard);
                return Err(anyhow::anyhow!(
                    "native launch outcome uncertain; retained recording {}; recovery required",
                    value.recording_id
                ));
            }
            result => {
                value.phase = process::RunPhase::Failed;
                value
                    .errors
                    .push(format!("program launch failed: {result:?}"));
                match stop_capture(recording.id) {
                    Ok(()) => value.capture_finalized = true,
                    Err(error) => value.errors.push(error.to_string()),
                }
                value.output_drained = true;
            }
        }
        let guard = self
            .mutation
            .lock()
            .map_err(|_| anyhow::anyhow!("process authority lock poisoned"))?;
        let persisted = self.store()?.replace(value.revision, &mut value);
        drop(guard);
        persisted.context("launch outcome metadata could not commit; retained execution authority requires reconciliation, never implicit termination")?;
        Ok(value)
    }

    pub(crate) fn prepare_capture(
        &self,
        options: &process::ProgramOptions,
    ) -> Result<bmux_recording_protocol::RecordingSummary> {
        // Recording start is unfiltered and fully ready BEFORE invoking session creation.
        let recording = {
            let guard = self
                .mutation
                .lock()
                .map_err(|_| anyhow::anyhow!("process authority lock poisoned"))?;
            let handle = crate::manual_handle().context("manual recording runtime unavailable")?;
            let handle = handle
                .read()
                .map_err(|_| anyhow::anyhow!("recording handle poisoned"))?;
            let mut runtime = handle
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("recording runtime poisoned"))?;
            runtime.enable_process_capture();
            let summary = runtime.start(
                None,
                true,
                Some("process".into()),
                options.profile.into(),
                crate::commands::default_event_kinds_for_profile(options.profile, true)
                    .into_iter()
                    .map(Into::into)
                    .collect(),
            )?;
            drop(guard);
            summary
        };
        if let Err(error) = crate::durable::write_atomic(
            &Path::new(&recording.path).join("viewport.json"),
            &serde_json::to_vec(&(options.cols, options.rows))?,
        ) {
            let _ = stop_capture(recording.id);
            return Err(error);
        }
        Ok(recording)
    }

    async fn persist_ordered_capture(&self, value: &process::RunStatus) -> Result<()> {
        use bmux_pane_runtime_plugin_api::pane_runtime_presentation_v1 as presentation;
        let directory = self.root.join(value.recording_id.to_string());
        if !directory.join("capture-format.json").try_exists()? {
            return Ok(());
        }
        let format: presentation::TerminalCaptureFormat =
            serde_json::from_slice(&std::fs::read(directory.join("capture-format.json"))?)?;
        let reference = execution(value)?;
        let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
        let mut offset = 0;
        let mut bytes = Vec::new();
        loop {
            let batch = presentation::client::capture_events(
                &mut client,
                reference.clone(),
                format,
                offset,
                CAPTURE_READ_MAX_BYTES,
            )
            .await?
            .map_err(|error| anyhow::anyhow!("ordered capture: {error:?}"))?;
            ensure!(
                batch.execution == reference
                    && batch.offset == offset
                    && batch.continuation == offset + u64::try_from(batch.data.len())?,
                "ordered capture identity or cursor mismatch"
            );
            ensure!(
                bytes.len().saturating_add(batch.data.len()) <= 256 * 1024 * 1024,
                "ordered capture exceeds bound"
            );
            bytes.extend_from_slice(&batch.data);
            offset = batch.continuation;
            if batch.eof {
                break;
            }
            ensure!(!batch.data.is_empty(), "ordered capture EOF not committed");
        }
        crate::durable::write_atomic(&directory.join("capture.events"), &bytes)?;
        Ok(())
    }

    async fn persist_raw_source(&self, value: &process::RunStatus, watermark: u64) -> Result<()> {
        let reservation = self.store()?.reservation(value.run_id)?;
        let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
        let bytes = collect_raw_source(&mut client, reservation.initial_cursor, watermark).await?;
        let path = self
            .root
            .join(value.recording_id.to_string())
            .join("output.raw");
        if path.try_exists()? {
            ensure!(
                std::fs::read(&path)? == bytes,
                "persisted raw source differs from execution authority"
            );
        } else {
            crate::durable::write_atomic(&path, &bytes)?;
        }
        Ok(())
    }

    fn close_original_writer(&self, value: &process::RunStatus, watermark: u64) -> Result<()> {
        let directory = self.root.join(value.recording_id.to_string());
        let manifest: crate::recording_runtime::Manifest =
            serde_json::from_slice(&std::fs::read(directory.join("manifest.json"))?)?;
        if manifest.summary.ended_epoch_ms.is_none() {
            stop_capture(value.recording_id)?;
            // Persist original writer disposition independently of derived raw
            // verification: a later repair must not lose genuine event timing.
            let closure = closed_original_events(&directory, value, watermark)?;
            crate::durable::write_atomic(
                &directory.join("original-writer-closure.json"),
                &serde_json::to_vec(&closure)?,
            )?;
        } else {
            ensure!(
                original_writer_closed(&directory, value)?,
                "original writer closure receipt unavailable; original timing cannot be inferred"
            );
        }
        Ok(())
    }

    async fn finish_capture(
        &self,
        id: Uuid,
        expected: u64,
        terminate_running: bool,
    ) -> Result<process::RunStatus> {
        let admitted = self.load(id)?;
        ensure!(admitted.revision == expected, "stale process run revision");
        let initial = self.refresh(id).await?;
        let expected = initial.revision;
        ensure!(
            initial.timing_complete,
            "raw-only recovery has no original timing/events; capture remains incomplete and GIF/full telemetry finalization is unavailable"
        );
        ensure!(initial.revision == expected, "stale process run revision");
        if initial.capture_finalized {
            return Ok(initial);
        }
        let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
        let status = self
            .execution_status(&mut client, execution(&initial)?, None)
            .await?;
        if matches!(status.phase, pane::TerminalExecutionPhase::Running) {
            ensure!(
                terminate_running,
                "process still running; explicit termination required"
            );
            self.execution_terminate(&mut client, execution(&initial)?, 1000)
                .await?;
        }
        let status = self
            .execution_status(&mut client, execution(&initial)?, Some(30_000))
            .await?;
        ensure!(
            !matches!(status.phase, pane::TerminalExecutionPhase::Running)
                && status.output_drained
                && status.terminal_session_ended
                && matches!(
                    status.capture_integrity,
                    pane::TerminalCaptureIntegrity::Complete
                ),
            "process exit and capture drain are not yet complete"
        );
        ensure!(
            status.error.is_none(),
            "execution capture failed: {:?}",
            status.error
        );
        let final_screen = self
            .execution_screen(&mut client, execution(&initial)?)
            .await?;
        // Join the original writer before asynchronous independent spool verification;
        // never hold recording authority's mutex across a service round trip.
        self.close_original_writer(&initial, status.output_watermark)?;
        validate_projected_output(&self.root.join(initial.recording_id.to_string()))?;
        self.persist_raw_source(&initial, status.output_watermark)
            .await?;
        self.persist_ordered_capture(&initial).await?;
        let guard = self
            .mutation
            .lock()
            .map_err(|_| anyhow::anyhow!("process authority lock poisoned"))?;
        let store = self.store()?;
        let mut value = store.load(id)?;
        ensure!(value.revision == expected, "stale process run revision");
        // Runtime stop joins the bounded writer and durably publishes its manifest.
        let directory = self.root.join(value.recording_id.to_string());
        let manifest: crate::recording_runtime::Manifest =
            serde_json::from_slice(&std::fs::read(directory.join("manifest.json"))?)?;
        // Recovery after a durable writer stop but interrupted derived artifact publication.
        if manifest.summary.ended_epoch_ms.is_none()
            && let Err(error) = stop_capture(value.recording_id)
        {
            value.errors.push(error.to_string());
            store.replace(expected, &mut value)?;
            return Err(error);
        }
        // A drained PTY is not proof that canonical capture persisted every byte.
        // Stop joins the projected event writer; compare the independent raw copy with runtime authority
        // before acknowledging finalization, including interruption-recovery retries.
        ensure!(
            std::fs::metadata(directory.join("output.raw"))?.len() == status.output_watermark,
            "canonical capture does not match drained execution output watermark"
        );
        crate::durable::write_atomic(&directory.join("screen.txt"), final_screen.as_bytes())?;
        value.phase = process::RunPhase::Finalized;
        value.exit_code = status.exit_code;
        value.output_drained = true;
        value.output_watermark = status.output_watermark;
        value.capture_finalized = true;
        value.target_exited = true;
        value.terminal_session_ended = status.terminal_session_ended;
        value.capture_eof = status.output_drained;
        value.capture_watermark = status.output_watermark;
        value.recording_watermark = status.output_watermark;
        value.durable_recording_complete = true;
        value.cleanup_complete = status.cleanup_complete;
        store.replace(expected, &mut value)?;
        drop(guard);
        Ok(value)
    }
}

fn target_exit_observed(status: &pane::TerminalExecutionStatus) -> bool {
    matches!(status.phase, pane::TerminalExecutionPhase::Exited)
        || status.exit_code.is_some()
        || status.signal.is_some()
}

pub(crate) async fn refresh_finalized_cleanup(
    client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
    store: &ProcessStore,
    mutation: &Mutex<()>,
    value: process::RunStatus,
) -> Result<process::RunStatus> {
    // Finalized evidence remains authoritative and readable offline. Query only
    // this exact execution's cleanup receipt; never resume capture or relaunch.
    if value.execution.is_none() {
        return Ok(value);
    }
    let reference = execution(&value)?;
    let Ok(Ok(status)) = pane::client::status(client, reference.clone()).await else {
        // The owned endpoint may already have gone away after finalization.
        // Unavailability does not invalidate durable evidence or prove cleanup.
        return Ok(value);
    };
    ensure!(
        status.execution == reference,
        "cleanup execution identity mismatch"
    );
    let _guard = mutation
        .lock()
        .map_err(|_| anyhow::anyhow!("process authority lock poisoned"))?;
    // Reload after the await so a concurrent refresh cannot overwrite a newer
    // revision. Finalized exit, bytes, timing and capture state are immutable here.
    let mut current = store.load(value.run_id)?;
    ensure!(current.capture_finalized, "finalized capture regressed");
    ensure!(
        execution(&current)? == reference,
        "cleanup execution identity changed"
    );
    if current.cleanup_complete != status.cleanup_complete {
        current.cleanup_complete = status.cleanup_complete;
        store.replace(current.revision, &mut current)?;
    }
    Ok(current)
}

fn execution(value: &process::RunStatus) -> Result<pane::TerminalExecutionRef> {
    let reference = value.execution.as_ref().context("run has no execution")?;
    Ok(pane::TerminalExecutionRef {
        session_id: reference.session_id,
        pane_id: reference.pane_id,
        execution_token: reference.execution_token,
    })
}

pub(crate) fn stop_capture(id: Uuid) -> Result<()> {
    let handle = crate::manual_handle().context("manual recording runtime unavailable")?;
    let handle = handle
        .read()
        .map_err(|_| anyhow::anyhow!("recording handle poisoned"))?;
    handle
        .0
        .lock()
        .map_err(|_| anyhow::anyhow!("recording runtime poisoned"))?
        .stop(Some(id))?;
    Ok(())
}

fn capture_writer_ack(id: Uuid) -> Result<Option<crate::recording_runtime::writer_ack::WriterAck>> {
    let Some(handle) = crate::manual_handle() else {
        return Ok(None);
    };
    let handle = handle
        .read()
        .map_err(|_| anyhow::anyhow!("recording handle poisoned"))?;
    let runtime = handle
        .0
        .lock()
        .map_err(|_| anyhow::anyhow!("recording runtime poisoned"))?;
    Ok(runtime.writer_ack(id))
}

fn failure(error: anyhow::Error) -> process::ProcessError {
    process::ProcessError::OperationFailed {
        reason: error.into_boxed_dyn_error().to_string(),
    }
}

impl bmux_recording_plugin_api::recording_process_presentation_v1::RecordingProcessPresentationV1Service
    for ProcessService
{
    fn capabilities(&self) -> Reply<'_, bmux_pane_runtime_plugin_api::pane_runtime_presentation_v1::PresentationCapabilities> {
        Box::pin(async move {
            let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
            bmux_pane_runtime_plugin_api::pane_runtime_presentation_v1::client::capabilities(&mut client)
                .await.map_err(|error| failure(error.into()))?
                .map_err(|error| failure(anyhow::anyhow!("presentation negotiation: {error:?}")))
        })
    }
    fn resize(
        &self,
        run_id: Uuid,
        format: bmux_pane_runtime_plugin_api::pane_runtime_presentation_v1::TerminalCaptureFormat,
        cols: u16,
        rows: u16,
    ) -> Reply<'_, ()> {
        Box::pin(async move {
            let value = self.load(run_id).map_err(failure)?;
            let directory = self.root.join(value.recording_id.to_string());
            // Persist negotiated representation before the side effect. This is
            // selection metadata, never a second resize order or event journal.
            crate::durable::write_atomic(&directory.join("capture-format.json"), &serde_json::to_vec(&format).map_err(|error| failure(error.into()))?).map_err(failure)?;
            let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
            bmux_pane_runtime_plugin_api::pane_runtime_presentation_v1::client::resize(
                &mut client, execution(&value).map_err(failure)?, format, cols, rows,
            ).await.map_err(|error| failure(error.into()))?
                .map_err(|error| failure(anyhow::anyhow!("ordered resize: {error:?}")))
        })
    }
    fn present(&self, run_id: Uuid) -> Reply<'_, pane::TerminalExecutionScreen> {
        Box::pin(async move {
            // Durable run ownership is checked before dispatch. The pane service
            // independently authorizes read access and validates the exact token.
            let value = self.load(run_id).map_err(failure)?;
            let reference = execution(&value).map_err(failure)?;
            let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
            let screen = bmux_pane_runtime_plugin_api::pane_runtime_pointer_grid_v1::client::screen(
                &mut client,
                1,
                reference.clone(),
            )
            .await
            .map_err(|error| failure(error.into()))?
            .map_err(|error| failure(anyhow::anyhow!("presentation: {error:?}")))?;
            if screen.execution != reference {
                return Err(failure(anyhow::anyhow!("presentation execution mismatch")));
            }
            Ok(screen)
        })
    }
}

impl process::RecordingProcessV4Service for ProcessService {
    fn start(
        &self,
        run_id: Uuid,
        launch_mutation_id: Uuid,
        options: process::ProgramOptions,
        runtime_root: String,
    ) -> Reply<'_, process::RunStatus> {
        Box::pin(async move {
            self.launch(run_id, launch_mutation_id, options, runtime_root)
                .await
                .map_err(failure)
        })
    }
    fn status(&self, run_id: Uuid) -> Reply<'_, process::RunStatus> {
        Box::pin(async move { self.refresh(run_id).await.map_err(failure) })
    }
    fn read(&self, run_id: Uuid, since: u64, max_bytes: u32) -> Reply<'_, process::OutputRead> {
        Box::pin(async move {
            let value = self.refresh(run_id).await.map_err(failure)?;
            let directory = self.root.join(value.recording_id.to_string());
            if value.capture_finalized {
                return read_output_through(
                    &directory,
                    &value,
                    since,
                    max_bytes,
                    Some(value.output_watermark),
                )
                .map_err(failure);
            }
            if let Some(ack) = capture_writer_ack(value.recording_id).map_err(failure)? {
                crate::recording_runtime::writer_ack::wait_for_watermark(
                    ack,
                    0,
                    tokio::time::Instant::now() + std::time::Duration::from_secs(1),
                )
                .await
                .map_err(failure)?;
            }
            ensure_read_limit(since, max_bytes, value.output_watermark).map_err(failure)?;
            let mut cursor = self
                .store()
                .map_err(failure)?
                .reservation(run_id)
                .map_err(failure)?
                .initial_cursor;
            cursor.offset = since;
            let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
            let response = pane::client::capture_resume(
                &mut client,
                cursor.clone(),
                max_bytes.min(CAPTURE_READ_MAX_BYTES),
            )
            .await
            .map_err(|error| failure(error.into()))?
            .map_err(|error| failure(anyhow::anyhow!("raw capture read: {error:?}")))?;
            match response {
                pane::TerminalCaptureResponse::Batch { value: batch } => {
                    validate_raw_batch(&cursor, &batch).map_err(failure)?;
                    let mut data = batch.data;
                    data.truncate(
                        usize::try_from(value.output_watermark - since)
                            .map_err(|error| failure(error.into()))?,
                    );
                    let next_cursor =
                        since + u64::try_from(data.len()).map_err(|error| failure(error.into()))?;
                    Ok(process::OutputRead {
                        run_id,
                        since,
                        next_cursor,
                        data,
                        complete: value.output_drained && next_cursor == value.output_watermark,
                    })
                }
                pane::TerminalCaptureResponse::Gap { value: gap } => Err(failure(anyhow::anyhow!(
                    "canonical raw read GAP: {}",
                    gap.reason
                ))),
            }
        })
    }
    fn screen(&self, run_id: Uuid) -> Reply<'_, String> {
        Box::pin(async move {
            let value = self.load(run_id).map_err(failure)?;
            let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
            self.execution_screen(&mut client, execution(&value).map_err(failure)?)
                .await
                .map_err(failure)
        })
    }
    fn reconcile(&self, run_id: Uuid) -> Reply<'_, process::RunStatus> {
        Box::pin(async move { self.refresh(run_id).await.map_err(failure) })
    }
    fn send(
        &self,
        run_id: Uuid,
        data: Vec<u8>,
        mutation_id: Uuid,
    ) -> Reply<'_, pane::TerminalInputReceipt> {
        Box::pin(async move {
            let value = self.load(run_id).map_err(failure)?;
            let replayed_input = self
                .store()
                .map_err(failure)?
                .admit_input(run_id, mutation_id, &data)
                .map_err(failure)?;
            if let Some(receipt) = self
                .store()
                .map_err(failure)?
                .input_receipt(run_id, mutation_id)
                .map_err(failure)?
            {
                return Ok(receipt);
            }
            let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
            // send_admitted validates exact-token launch/runtime/capture
            // authority at fresh-effect admission without durable refresh.
            // Mutation admission and uncertain receipts remain durable.
            self.store()
                .map_err(failure)?
                .admit_input(run_id, mutation_id, &data)
                .map_err(failure)?;
            let receipt = pane::client::send_admitted(
                &mut client,
                execution(&value).map_err(failure)?,
                mutation_id,
                data.clone(),
            )
            .await
            .map_err(|error| failure(error.into()))?
            .map_err(|error| match error {
                pane::TerminalRecoveryError::RecoveryRequired { reason } => {
                    process::ProcessError::RecoveryRequired { reason }
                }
                error => failure(anyhow::anyhow!("input admission: {error:?}")),
            })?;
            if matches!(receipt.phase, pane::TerminalInputPhase::Completed) {
                self.store()
                    .map_err(failure)?
                    .record_input_once(run_id, mutation_id, || {
                        ensure!(
                            !replayed_input,
                            "original input event timing unavailable; cannot invent recovery events"
                        );
                        let reference = execution(&value)?;
                        let handle = crate::manual_handle().context(
                            "input capture owner unavailable; cannot invent original timing",
                        )?;
                        let handle = handle
                            .read()
                            .map_err(|_| anyhow::anyhow!("recording handle poisoned"))?;
                        let runtime = handle
                            .0
                            .lock()
                            .map_err(|_| anyhow::anyhow!("recording runtime poisoned"))?;
                        ensure!(
                            runtime
                                .active_capture_target()
                                .is_some_and(|(id, _)| id == value.recording_id),
                            "input original capture owner unavailable"
                        );
                        runtime.record(
                            bmux_recording_protocol::RecordingEventKind::PaneInputRaw,
                            bmux_recording_protocol::RecordingPayload::Bytes { data: data.clone() },
                            bmux_recording_runtime::RecordMeta {
                                session_id: Some(reference.session_id),
                                pane_id: Some(reference.pane_id),
                                client_id: None,
                            },
                        )?;
                        Ok(())
                    })
                    .map_err(failure)?;
            }
            self.store()
                .map_err(failure)?
                .write_input_receipt(run_id, mutation_id, &receipt)
                .map_err(failure)?;
            Ok(receipt)
        })
    }
    fn input_receipt(
        &self,
        run_id: Uuid,
        mutation_id: Uuid,
    ) -> Reply<'_, pane::TerminalInputReceipt> {
        Box::pin(async move {
            let value = self.load(run_id).map_err(failure)?;
            let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
            pane::client::input_receipt(
                &mut client,
                execution(&value).map_err(failure)?,
                mutation_id,
            )
            .await
            .map_err(|error| failure(error.into()))?
            .map_err(|error| failure(anyhow::anyhow!("input receipt: {error:?}")))
        })
    }
    fn resolve(
        &self,
        run_id: Uuid,
        mutation_id: Uuid,
        expected_revision: u64,
        action: pane::TerminalResolutionAction,
        input_mutation_id: Option<Uuid>,
        grace_ms: u32,
    ) -> Reply<'_, pane::TerminalResolutionReceipt> {
        Box::pin(async move {
            let value = self.load(run_id).map_err(failure)?;
            let intent =
                serde_json::to_vec(&(expected_revision, &action, input_mutation_id, grace_ms))
                    .map_err(|error| failure(error.into()))?;
            self.store()
                .map_err(failure)?
                .admit_resolution(run_id, mutation_id, &intent)
                .map_err(failure)?;
            let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
            pane::client::resolve(
                &mut client,
                execution(&value).map_err(failure)?,
                mutation_id,
                expected_revision,
                action,
                input_mutation_id,
                grace_ms,
            )
            .await
            .map_err(|error| failure(error.into()))?
            .map_err(|error| failure(anyhow::anyhow!("resolution: {error:?}")))
        })
    }
    fn wait(&self, run_id: Uuid, timeout_ms: u32) -> Reply<'_, process::RunStatus> {
        Box::pin(async move {
            if timeout_ms > 30_000 {
                return Err(process::ProcessError::InvalidOptions {
                    reason: "wait exceeds 30 seconds".into(),
                });
            }
            let value = self.refresh(run_id).await.map_err(failure)?;
            if value.capture_finalized || value.recovery_required {
                return Ok(value);
            }
            let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
            self.execution_status(
                &mut client,
                execution(&value).map_err(failure)?,
                Some(timeout_ms),
            )
            .await
            .map_err(failure)?;
            self.refresh(run_id).await.map_err(failure)
        })
    }
    fn terminate(&self, run_id: Uuid, grace_ms: u32) -> Reply<'_, process::RunStatus> {
        Box::pin(async move {
            if grace_ms > 30_000 {
                return Err(process::ProcessError::InvalidOptions {
                    reason: "grace period exceeds 30 seconds".into(),
                });
            }
            let value = self.load(run_id).map_err(failure)?;
            if value.capture_finalized {
                return Ok(value);
            }
            let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
            self.execution_terminate(&mut client, execution(&value).map_err(failure)?, grace_ms)
                .await
                .map_err(failure)?;
            self.refresh(run_id).await.map_err(failure)
        })
    }
    fn finish(
        &self,
        run_id: Uuid,
        expected_revision: u64,
        terminate_running: bool,
    ) -> Reply<'_, process::RunStatus> {
        Box::pin(async move {
            self.finish_capture(run_id, expected_revision, terminate_running)
                .await
                .map_err(failure)
        })
    }
}

pub(crate) fn read_output(
    directory: &Path,
    status: &process::RunStatus,
    since: u64,
    max_bytes: u32,
) -> Result<process::OutputRead> {
    read_output_through(directory, status, since, max_bytes, None)
}

fn read_output_through(
    directory: &Path,
    status: &process::RunStatus,
    since: u64,
    max_bytes: u32,
    durable_end: Option<u64>,
) -> Result<process::OutputRead> {
    use std::io::{Read, Seek, SeekFrom};
    ensure!(
        max_bytes > 0 && max_bytes <= 1024 * 1024,
        "max-bytes must be 1..1048576"
    );
    let source = if status.capture_finalized && directory.join("output.raw").try_exists()? {
        directory.join("output.raw")
    } else {
        directory.join("output.live")
    };
    let mut file = std::fs::File::open(source)?;
    let length = file.metadata()?.len();
    ensure!(
        !status.output_drained || length <= status.output_watermark,
        "capture exceeds drained execution output watermark"
    );
    let end = durable_end.unwrap_or(length);
    ensure!(
        end <= length,
        "durable capture acknowledgment exceeds disk output"
    );
    ensure!(
        !status.output_drained || end <= status.output_watermark,
        "capture exceeds drained execution output watermark"
    );
    ensure!(since <= end, "cursor beyond captured output");
    file.seek(SeekFrom::Start(since))?;
    let count = u64::from(max_bytes).min(end - since);
    let mut data = vec![0; usize::try_from(count)?];
    file.read_exact(&mut data)?;
    Ok(process::OutputRead {
        run_id: status.run_id,
        since,
        next_cursor: since + count,
        data,
        complete: status.output_drained && since + count == end && end >= status.output_watermark,
    })
}

fn ensure_read_limit(since: u64, max_bytes: u32, watermark: u64) -> Result<()> {
    ensure!(
        max_bytes > 0 && max_bytes <= 1024 * 1024,
        "max-bytes must be 1..1048576"
    );
    ensure!(since <= watermark, "cursor beyond captured output");
    Ok(())
}

fn validate_raw_batch(
    cursor: &pane::TerminalCaptureCursor,
    batch: &pane::TerminalCaptureBatch,
) -> Result<()> {
    ensure!(
        batch.cursor == *cursor
            && batch.continuation.execution == cursor.execution
            && batch.continuation.spool_generation == cursor.spool_generation
            && cursor.offset.checked_add(u64::try_from(batch.data.len())?)
                == Some(batch.continuation.offset),
        "canonical raw verification identity/boundary mismatch"
    );
    Ok(())
}

async fn collect_raw_source(
    client: &mut impl bmux_plugin_sdk::TypedDispatchClient,
    mut cursor: pane::TerminalCaptureCursor,
    watermark: u64,
) -> Result<Vec<u8>> {
    ensure!(
        cursor.offset == 0 && watermark <= 64 * 1024 * 1024,
        "invalid canonical raw boundary"
    );
    let mut bytes = Vec::new();
    while cursor.offset < watermark {
        match capture_resume(client, cursor.clone()).await? {
            pane::TerminalCaptureResponse::Batch { value: batch } => {
                ensure!(
                    batch.cursor == cursor
                        && batch.continuation.execution == cursor.execution
                        && batch.continuation.spool_generation == cursor.spool_generation
                        && batch.continuation.offset > cursor.offset
                        && batch.continuation.offset <= watermark
                        && cursor.offset.checked_add(u64::try_from(batch.data.len())?)
                            == Some(batch.continuation.offset),
                    "canonical raw verification identity/boundary mismatch"
                );
                bytes.extend_from_slice(&batch.data);
                cursor = batch.continuation;
                tokio::task::yield_now().await;
            }
            pane::TerminalCaptureResponse::Gap { value: gap } => anyhow::bail!(
                "canonical source verification GAP: {}; cannot finalize incomplete recording",
                gap.reason
            ),
        }
    }
    Ok(bytes)
}

/// A successful service envelope is not proof that a native target launched.
/// Keep the execution owner's durable failure/uncertainty instead of replacing
/// it with a secondary `NotFound` from a pane that was never published.
fn retain_incomplete_launch(
    value: &mut process::RunStatus,
    receipt: &pane::TerminalLaunchReceipt,
) -> bool {
    if matches!(
        receipt.phase,
        pane::TerminalLaunchPhase::Running | pane::TerminalLaunchPhase::Exited
    ) {
        return false;
    }
    value.phase = process::RunPhase::RecoveryRequired;
    value.recovery_required = true;
    let error = format!(
        "native launch {:?}; recovery required; refusing relaunch: {}",
        receipt.phase,
        receipt
            .error
            .as_deref()
            .unwrap_or("no authoritative running execution")
    );
    if !value.errors.contains(&error) {
        value.errors.push(error);
    }
    // Neither launch retirement nor failed startup proves capture EOF, target
    // exit, or recording finalization. Those retain their independent authority.
    true
}

#[cfg(test)]
mod lifecycle_consumer_tests {
    use super::*;

    #[test]
    fn finalized_read_uses_raw_source_without_overwriting_projected_events() {
        let root = std::env::temp_dir().join(format!("bmux-raw-read-{}", Uuid::new_v4()));
        crate::durable::private_directory(&root).unwrap();
        let raw = b"\x1b[6nvisible";
        crate::durable::write_atomic(&root.join("output.raw"), raw).unwrap();
        crate::durable::write_atomic(&root.join("output.live"), b"visible").unwrap();
        let mut value = ProcessService::initial_status(
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "/owned".into(),
        );
        value.capture_finalized = true;
        value.output_drained = true;
        value.output_watermark = u64::try_from(raw.len()).unwrap();
        let read = read_output(&root, &value, 0, 1024).unwrap();
        assert_eq!(read.data, raw);
        assert!(read.complete);
        assert_eq!(std::fs::read(root.join("output.live")).unwrap(), b"visible");
        std::fs::remove_dir_all(root).unwrap();
    }

    struct RawSourceClient(pane::TerminalCaptureBatch);
    impl bmux_plugin_sdk::TypedDispatchClient for RawSourceClient {
        fn invoke_service_raw(
            &mut self,
            _capability: &str,
            _kind: bmux_ipc::InvokeServiceKind,
            _interface: &str,
            operation: &str,
            payload: Vec<u8>,
        ) -> impl std::future::Future<Output = bmux_plugin_sdk::TypedDispatchClientResult<Vec<u8>>>
        {
            std::future::poll_fn(move |_| {
                assert_eq!(operation, "capture-resume");
                let request: pane::client::CaptureResumeRequest =
                    bmux_plugin_sdk::decode_service_message(&payload).unwrap();
                assert_eq!(request.cursor, self.0.cursor);
                let response: Result<pane::TerminalCaptureResponse, pane::TerminalRecoveryError> =
                    Ok(pane::TerminalCaptureResponse::Batch {
                        value: self.0.clone(),
                    });
                std::task::Poll::Ready(Ok(
                    bmux_plugin_sdk::encode_service_message(&response).unwrap()
                ))
            })
        }
    }

    #[tokio::test]
    async fn raw_source_preserves_bootstrap_query_separately_from_presentation() {
        let cursor = pane::TerminalCaptureCursor {
            execution: pane::TerminalExecutionRef {
                session_id: Uuid::new_v4(),
                pane_id: Uuid::new_v4(),
                execution_token: Uuid::new_v4(),
            },
            spool_generation: Uuid::new_v4(),
            offset: 0,
        };
        let raw = b"\x1b[6nvisible target output";
        let batch: pane::TerminalCaptureBatch = serde_json::from_value(serde_json::json!({
            "cursor": cursor, "continuation": {"execution": cursor.execution, "spool_generation": cursor.spool_generation, "offset": raw.len()},
            "data": raw, "eof": true
        })).unwrap();
        let mut client = RawSourceClient(batch.clone());
        assert_eq!(
            collect_raw_source(&mut client, cursor.clone(), raw.len() as u64)
                .await
                .unwrap(),
            raw
        );
        let mut wrong = batch.clone();
        wrong.continuation.spool_generation = Uuid::new_v4();
        assert!(
            collect_raw_source(
                &mut RawSourceClient(wrong),
                cursor.clone(),
                raw.len() as u64
            )
            .await
            .is_err()
        );
        assert!(
            collect_raw_source(&mut RawSourceClient(batch), cursor, raw.len() as u64 - 1)
                .await
                .is_err()
        );
    }

    #[test]
    fn uncertain_native_launch_retains_startup_error_without_claiming_completion() {
        let mut value = ProcessService::initial_status(
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "/owned/runtime".into(),
        );
        let receipt: pane::TerminalLaunchReceipt = serde_json::from_value(serde_json::json!({
            "reservation": {
                "execution": {"session_id": Uuid::new_v4(), "pane_id": Uuid::new_v4(), "execution_token": Uuid::new_v4()},
                "launch_mutation_id": value.launch_mutation_id, "revision": 1,
                "options": {"program": "cmd.exe", "args": [], "cwd": null, "env": {}, "cols": 80, "rows": 24},
                "initial_cursor": {"execution": {"session_id": Uuid::new_v4(), "pane_id": Uuid::new_v4(), "execution_token": Uuid::new_v4()}, "spool_generation": Uuid::new_v4(), "offset": 0}
            },
            "phase": "uncertain", "revision": 3, "exit_code": null, "exit_signal": null,
            "cleanup_complete": false,
            "error": "program startup timed out; unpublished pane cleanup timed out"
        })).unwrap();
        assert!(retain_incomplete_launch(&mut value, &receipt));
        assert!(matches!(value.phase, process::RunPhase::RecoveryRequired));
        assert!(value.recovery_required);
        assert!(value.errors[0].contains("program startup timed out"));
        assert!(value.errors[0].contains("unpublished pane cleanup timed out"));
        assert!(
            !value.target_exited
                && !value.capture_eof
                && !value.capture_finalized
                && !value.cleanup_complete
        );
        assert!(retain_incomplete_launch(&mut value, &receipt));
        assert_eq!(
            value.errors.len(),
            1,
            "reconciliation must not duplicate diagnostics"
        );
        for phase in [
            pane::TerminalLaunchPhase::Reserved,
            pane::TerminalLaunchPhase::Launching,
            pane::TerminalLaunchPhase::Failed,
            pane::TerminalLaunchPhase::Retired,
        ] {
            let mut receipt = receipt.clone();
            receipt.phase = phase;
            let mut observed = value.clone();
            assert!(retain_incomplete_launch(&mut observed, &receipt));
            assert!(matches!(
                observed.phase,
                process::RunPhase::RecoveryRequired
            ));
            assert!(observed.recovery_required);
            assert!(
                !observed.target_exited
                    && !observed.capture_finalized
                    && !observed.cleanup_complete
            );
        }
        for phase in [
            pane::TerminalLaunchPhase::Running,
            pane::TerminalLaunchPhase::Exited,
        ] {
            let mut receipt = receipt.clone();
            receipt.phase = phase;
            assert!(!retain_incomplete_launch(&mut value, &receipt));
        }
    }

    fn execution_status(phase: pane::TerminalExecutionPhase) -> pane::TerminalExecutionStatus {
        pane::TerminalExecutionStatus {
            execution: pane::TerminalExecutionRef {
                session_id: Uuid::new_v4(),
                pane_id: Uuid::new_v4(),
                execution_token: Uuid::new_v4(),
            },
            lifecycle: pane::TerminalLifecycle::ConventionalTerminalSession,
            phase,
            session_end: pane::TerminalSessionEnd::Ended,
            terminal_session_ended: true,
            capture_integrity: pane::TerminalCaptureIntegrity::Capturing,
            exit_code: None,
            signal: None,
            output_drained: false,
            output_watermark: 0,
            cleanup_complete: false,
            error: Some("cleanup failed without target outcome".into()),
        }
    }

    #[test]
    fn failed_execution_requires_observed_exit_or_signal() {
        let mut status = execution_status(pane::TerminalExecutionPhase::Failed);
        assert!(!target_exit_observed(&status));
        status.exit_code = Some(7);
        assert!(target_exit_observed(&status));
        status.exit_code = None;
        status.signal = Some("SIGTERM".into());
        assert!(target_exit_observed(&status));
        status.signal = None;
        status.phase = pane::TerminalExecutionPhase::Exited;
        assert!(target_exit_observed(&status));
        status.phase = pane::TerminalExecutionPhase::Running;
        assert!(!target_exit_observed(&status));
    }

    struct CleanupClient {
        expected: pane::TerminalExecutionRef,
        response: Option<Result<pane::TerminalExecutionStatus, pane::TerminalExecutionError>>,
        calls: usize,
    }

    impl bmux_plugin_sdk::TypedDispatchClient for CleanupClient {
        fn invoke_service_raw(
            &mut self,
            _capability: &str,
            _kind: bmux_ipc::InvokeServiceKind,
            _interface: &str,
            operation: &str,
            payload: Vec<u8>,
        ) -> impl std::future::Future<Output = bmux_plugin_sdk::TypedDispatchClientResult<Vec<u8>>>
        {
            let mut payload = Some(payload);
            std::future::poll_fn(move |_| {
                let payload = payload.take().expect("transport polled after completion");
                std::task::Poll::Ready((|| {
                    assert_eq!(operation, "status", "must not reopen capture or launch");
                    let request = bmux_plugin_sdk::decode_service_message::<
                        pane::client::StatusRequest,
                    >(&payload)
                    .unwrap();
                    assert_eq!(request.execution, self.expected);
                    self.calls += 1;
                    let Some(response) = &self.response else {
                        return Err(bmux_plugin_sdk::TypedDispatchClientError::transport(
                            "pane-runtime-execution-v4",
                            operation,
                            "owned endpoint absent",
                        ));
                    };
                    Ok(bmux_plugin_sdk::encode_service_message(response).unwrap())
                })())
            })
        }
    }

    fn finalized_status(reference: &pane::TerminalExecutionRef) -> process::RunStatus {
        let mut value = ProcessService::initial_status(
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            "/owned/runtime".into(),
        );
        value.execution = Some(process::ExecutionRef {
            session_id: reference.session_id,
            pane_id: reference.pane_id,
            execution_token: reference.execution_token,
        });
        value.phase = process::RunPhase::Finalized;
        value.exit_code = Some(7);
        value.target_exited = true;
        value.terminal_session_ended = true;
        value.capture_eof = true;
        value.output_drained = true;
        value.capture_finalized = true;
        value.durable_recording_complete = true;
        value.output_watermark = 123;
        value.capture_watermark = 123;
        value.recording_watermark = 123;
        value
    }

    fn fixture(value: &process::RunStatus) -> (PathBuf, ProcessStore) {
        let root = std::env::temp_dir().join(format!("bmux-finalized-cleanup-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let store = ProcessStore::new(&root).unwrap();
        store.create(value).unwrap();
        (root, store)
    }

    #[tokio::test]
    async fn finalized_refresh_updates_only_cleanup_and_is_idempotent() {
        // Contradictory live exit/capture fields must never replace finalized evidence.
        let mut status = execution_status(pane::TerminalExecutionPhase::Failed);
        status.cleanup_complete = true;
        let original = finalized_status(&status.execution);
        let (root, store) = fixture(&original);
        let mutation = Mutex::new(());
        let mut client = CleanupClient {
            expected: status.execution.clone(),
            response: Some(Ok(status)),
            calls: 0,
        };
        let refreshed = refresh_finalized_cleanup(&mut client, &store, &mutation, original.clone())
            .await
            .unwrap();
        let mut expected = original.clone();
        expected.cleanup_complete = true;
        expected.revision += 1;
        assert_eq!(
            serde_json::to_value(&refreshed).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        assert_eq!(
            serde_json::to_value(store.load(expected.run_id).unwrap()).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        // A caller holding the old revision must reload rather than overwrite it.
        let repeated = refresh_finalized_cleanup(&mut client, &store, &mutation, original)
            .await
            .unwrap();
        assert_eq!(repeated.revision, expected.revision);
        assert_eq!(client.calls, 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn finalized_refresh_preserves_offline_status_when_endpoint_absent() {
        let status = execution_status(pane::TerminalExecutionPhase::Exited);
        let original = finalized_status(&status.execution);
        let (root, store) = fixture(&original);
        let mutation = Mutex::new(());
        let mut client = CleanupClient {
            expected: status.execution,
            response: None,
            calls: 0,
        };
        // Both transport absence and an unavailable exact execution preserve evidence.
        for response in [
            None,
            Some(Err(pane::TerminalExecutionError::NotFound)),
            Some(Err(pane::TerminalExecutionError::Unavailable)),
        ] {
            client.response = response;
            let refreshed =
                refresh_finalized_cleanup(&mut client, &store, &mutation, original.clone())
                    .await
                    .unwrap();
            assert_eq!(
                serde_json::to_value(refreshed).unwrap(),
                serde_json::to_value(&original).unwrap()
            );
        }
        assert_eq!(
            store.load(original.run_id).unwrap().revision,
            original.revision
        );
        assert_eq!(client.calls, 3);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn finalized_refresh_without_execution_does_not_dispatch() {
        let status = execution_status(pane::TerminalExecutionPhase::Exited);
        let mut original = finalized_status(&status.execution);
        original.execution = None;
        let (root, store) = fixture(&original);
        let mut client = CleanupClient {
            expected: status.execution,
            response: None,
            calls: 0,
        };
        let mutation = Mutex::new(());
        let refreshed = refresh_finalized_cleanup(&mut client, &store, &mutation, original.clone())
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(refreshed).unwrap(),
            serde_json::to_value(original).unwrap()
        );
        assert_eq!(client.calls, 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn finalized_refresh_rejects_wrong_execution_without_mutation() {
        let status = execution_status(pane::TerminalExecutionPhase::Exited);
        let original = finalized_status(&status.execution);
        let (root, store) = fixture(&original);
        let mut wrong = status.clone();
        wrong.execution.execution_token = Uuid::new_v4();
        wrong.cleanup_complete = true;
        let mut client = CleanupClient {
            expected: status.execution,
            response: Some(Ok(wrong)),
            calls: 0,
        };
        let mutation = Mutex::new(());
        assert!(
            refresh_finalized_cleanup(&mut client, &store, &mutation, original.clone())
                .await
                .is_err()
        );
        assert_eq!(
            serde_json::to_value(store.load(original.run_id).unwrap()).unwrap(),
            serde_json::to_value(original).unwrap()
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
include!("process_read_tests.rs");
