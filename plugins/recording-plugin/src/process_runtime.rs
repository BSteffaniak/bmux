//! Typed process orchestration. This module never owns a PTY engine.
use anyhow::{Context, Result, ensure};
use bmux_pane_runtime_plugin_api::pane_runtime_execution_v1 as pane;
use bmux_plugin::{ServiceCallerDispatchClient, TypedServiceCaller};
use bmux_recording_plugin_api::recording_process_v1 as process;
use bmux_sessions_plugin_api::sessions_program_v1 as sessions;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use crate::process_store::ProcessStore;
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
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        Ok::<T, anyhow::Error>(runtime.block_on(operation(service)))
    })
    .join()
    .map_err(|_| anyhow::anyhow!("process service dispatch panicked"))?
}

pub(crate) struct ProcessService {
    caller: Arc<TypedServiceCaller>,
    root: PathBuf,
    runtime_root: String,
    isolated: bool,
    mutation: Arc<Mutex<()>>,
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

    fn load(&self, id: Uuid) -> Result<process::RunStatus> {
        self.require_isolation()?;
        let value = self.store()?.load(id)?;
        ensure!(
            value.runtime_root == self.runtime_root,
            "run belongs to a different isolated runtime"
        );
        Ok(value)
    }

    async fn refresh(&self, id: Uuid) -> Result<process::RunStatus> {
        let mut value = self.load(id)?;
        if value.capture_finalized || value.execution.is_none() {
            return Ok(value);
        }
        let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
        let status = pane::client::status(&mut client, execution(&value)?)
            .await?
            .map_err(|error| anyhow::anyhow!("execution status failed: {error:?}"))?;
        let guard = self
            .mutation
            .lock()
            .map_err(|_| anyhow::anyhow!("process authority lock poisoned"))?;
        let store = self.store()?;
        value = store.load(id)?;
        if !value.capture_finalized {
            let revision = value.revision;
            let phase = match status.phase {
                pane::ExecutionPhase::Running => process::RunPhase::Running,
                pane::ExecutionPhase::Exited => process::RunPhase::Exited,
                pane::ExecutionPhase::Failed => process::RunPhase::Failed,
            };
            if value.phase == phase
                && value.exit_code == status.exit_code
                && value.output_drained == status.output_drained
                && value.output_watermark == status.output_watermark
                && status
                    .error
                    .as_ref()
                    .is_none_or(|error| value.errors.last() == Some(error))
            {
                return Ok(value);
            }
            value.phase = phase;
            value.exit_code = status.exit_code;
            value.output_drained = status.output_drained;
            value.output_watermark = status.output_watermark;
            if let Some(error) = status.error {
                value.errors.push(error);
            }
            store.replace(revision, &mut value)?;
        }
        drop(guard);
        Ok(value)
    }

    async fn launch(
        &self,
        options: process::ProgramOptions,
        runtime_root: String,
    ) -> Result<process::RunStatus> {
        self.require_isolation()?;
        ensure!(
            runtime_root == self.runtime_root,
            "isolated runtime locator mismatch"
        );
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
        let recording = self.prepare_capture(&options)?;
        let id = Uuid::new_v4();
        let mut value = process::RunStatus {
            schema_version: 1,
            run_id: id,
            revision: 1,
            recording_id: recording.id,
            execution: None,
            runtime_root,
            phase: process::RunPhase::Starting,
            exit_code: None,
            output_drained: false,
            output_watermark: 0,
            capture_finalized: false,
            errors: vec![],
        };
        // Acknowledged metadata precedes launch; failure to persist prevents execution.
        if let Err(error) = self.store()?.create(&value) {
            let _ = stop_capture(recording.id);
            return Err(error);
        }
        let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
        let launched = sessions::client::new_program_session(
            &mut client,
            Some(format!("process-{id}")),
            sessions::ProgramOptions {
                program: options.program,
                args: options.args,
                cwd: options.cwd,
                env: options.env,
                cols: options.cols,
                rows: options.rows,
            },
        )
        .await;
        match launched {
            Ok(Ok(reference)) => {
                value.execution = Some(process::ExecutionRef {
                    session_id: reference.session_id,
                    pane_id: reference.pane_id,
                    execution_token: reference.execution_token,
                });
                value.phase = process::RunPhase::Running;
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
        let persisted = self.store()?.replace(1, &mut value);
        drop(guard);
        if let Err(error) = persisted {
            if value.execution.is_some() {
                let cleanup = pane::client::terminate(&mut client, execution(&value)?, 0).await;
                return Err(error.context(format!(
                    "launch persistence failed; execution cleanup: {cleanup:?}"
                )));
            }
            return Err(error);
        }
        Ok(value)
    }

    fn prepare_capture(
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

    async fn finish_capture(
        &self,
        id: Uuid,
        expected: u64,
        terminate_running: bool,
    ) -> Result<process::RunStatus> {
        let initial = self.load(id)?;
        ensure!(initial.revision == expected, "stale process run revision");
        if initial.capture_finalized {
            return Ok(initial);
        }
        let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
        let status = pane::client::status(&mut client, execution(&initial)?)
            .await?
            .map_err(|error| anyhow::anyhow!("status failed: {error:?}"))?;
        if matches!(status.phase, pane::ExecutionPhase::Running) {
            ensure!(
                terminate_running,
                "process still running; explicit termination required"
            );
            pane::client::terminate(&mut client, execution(&initial)?, 1000)
                .await?
                .map_err(|error| anyhow::anyhow!("termination failed: {error:?}"))?;
        }
        let status = pane::client::wait(&mut client, execution(&initial)?, 30_000)
            .await?
            .map_err(|error| anyhow::anyhow!("wait failed: {error:?}"))?;
        ensure!(
            !matches!(status.phase, pane::ExecutionPhase::Running) && status.output_drained,
            "process exit and capture drain are not yet complete"
        );
        ensure!(
            status.error.is_none(),
            "execution capture failed: {:?}",
            status.error
        );
        let final_screen = pane::client::screen(&mut client, execution(&initial)?)
            .await?
            .map_err(|error| anyhow::anyhow!("final screen failed: {error:?}"))?;
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
        // Stop joins the writer; compare its committed output with runtime authority
        // before acknowledging finalization, including interruption-recovery retries.
        ensure!(
            std::fs::metadata(directory.join("output.live"))?.len() == status.output_watermark,
            "canonical capture does not match drained execution output watermark"
        );
        crate::durable::write_atomic(&directory.join("screen.txt"), final_screen.text.as_bytes())?;
        value.phase = process::RunPhase::Finalized;
        value.exit_code = status.exit_code;
        value.output_drained = true;
        value.output_watermark = status.output_watermark;
        value.capture_finalized = true;
        store.replace(expected, &mut value)?;
        drop(guard);
        Ok(value)
    }
}

fn execution(value: &process::RunStatus) -> Result<pane::ExecutionRef> {
    let reference = value.execution.as_ref().context("run has no execution")?;
    Ok(pane::ExecutionRef {
        session_id: reference.session_id,
        pane_id: reference.pane_id,
        execution_token: reference.execution_token,
    })
}

fn stop_capture(id: Uuid) -> Result<()> {
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

fn failure(error: anyhow::Error) -> process::ProcessError {
    process::ProcessError::OperationFailed {
        reason: error.into_boxed_dyn_error().to_string(),
    }
}

impl process::RecordingProcessV1Service for ProcessService {
    fn start(
        &self,
        options: process::ProgramOptions,
        runtime_root: String,
    ) -> Reply<'_, process::RunStatus> {
        Box::pin(async move { self.launch(options, runtime_root).await.map_err(failure) })
    }
    fn status(&self, run_id: Uuid) -> Reply<'_, process::RunStatus> {
        Box::pin(async move { self.refresh(run_id).await.map_err(failure) })
    }
    fn read(&self, run_id: Uuid, since: u64, max_bytes: u32) -> Reply<'_, process::OutputRead> {
        Box::pin(async move {
            let value = self.refresh(run_id).await.map_err(failure)?;
            let directory = self.root.join(value.recording_id.to_string());
            if value.output_drained {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
                while std::fs::metadata(directory.join("output.live"))
                    .map_err(|error| failure(error.into()))?
                    .len()
                    < value.output_watermark
                {
                    if std::time::Instant::now() >= deadline {
                        return Err(failure(anyhow::anyhow!(
                            "capture writer has not reached drained output watermark"
                        )));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            }
            read_output(&directory, &value, since, max_bytes).map_err(failure)
        })
    }
    fn screen(&self, run_id: Uuid) -> Reply<'_, String> {
        Box::pin(async move {
            let value = self.load(run_id).map_err(failure)?;
            let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
            let screen = pane::client::screen(&mut client, execution(&value).map_err(failure)?)
                .await
                .map_err(|error| failure(error.into()))?
                .map_err(|error| failure(anyhow::anyhow!("screen failed: {error:?}")))?;
            Ok(screen.text)
        })
    }
    fn send(&self, run_id: Uuid, data: Vec<u8>, mutation_id: Uuid) -> Reply<'_, ()> {
        Box::pin(async move {
            if data.len() > 1024 * 1024 {
                return Err(process::ProcessError::InvalidOptions {
                    reason: "input exceeds 1 MiB".into(),
                });
            }
            let value = self.load(run_id).map_err(failure)?;
            if value.capture_finalized {
                return Err(process::ProcessError::StillRunning);
            }
            let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
            let receipt = self
                .root
                .join(value.recording_id.to_string())
                .join(format!("send-{mutation_id}.json"));
            {
                let _guard = self
                    .mutation
                    .lock()
                    .map_err(|_| failure(anyhow::anyhow!("send lock poisoned")))?;
                if receipt
                    .try_exists()
                    .map_err(|error| failure(error.into()))?
                {
                    let (previous, accepted): (Vec<u8>, bool) = serde_json::from_slice(
                        &std::fs::read(&receipt).map_err(|error| failure(error.into()))?,
                    )
                    .map_err(|error| failure(error.into()))?;
                    if previous != data {
                        return Err(failure(anyhow::anyhow!(
                            "conflicting send mutation identity"
                        )));
                    }
                    if accepted {
                        return Ok(());
                    }
                    return Err(failure(anyhow::anyhow!(
                        "send outcome ambiguous; refusing duplicate input"
                    )));
                }
                crate::durable::write_atomic(
                    &receipt,
                    &serde_json::to_vec(&(data.clone(), false))
                        .map_err(|error| failure(error.into()))?,
                )
                .map_err(failure)?;
            }
            let saved = data.clone();
            pane::client::send(&mut client, execution(&value).map_err(failure)?, data)
                .await
                .map_err(|error| failure(error.into()))?
                .map_err(|error| failure(anyhow::anyhow!("send failed: {error:?}")))?;
            crate::durable::write_atomic(
                &receipt,
                &serde_json::to_vec(&(saved, true)).map_err(|error| failure(error.into()))?,
            )
            .map_err(failure)
        })
    }
    fn wait(&self, run_id: Uuid, timeout_ms: u32) -> Reply<'_, process::RunStatus> {
        Box::pin(async move {
            if timeout_ms > 30_000 {
                return Err(process::ProcessError::InvalidOptions {
                    reason: "wait exceeds 30 seconds".into(),
                });
            }
            let value = self.load(run_id).map_err(failure)?;
            if value.capture_finalized {
                return Ok(value);
            }
            let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
            pane::client::wait(&mut client, execution(&value).map_err(failure)?, timeout_ms)
                .await
                .map_err(|error| failure(error.into()))?
                .map_err(|error| failure(anyhow::anyhow!("wait failed: {error:?}")))?;
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
            pane::client::terminate(&mut client, execution(&value).map_err(failure)?, grace_ms)
                .await
                .map_err(|error| failure(error.into()))?
                .map_err(|error| failure(anyhow::anyhow!("terminate failed: {error:?}")))?;
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
    use std::io::{Read, Seek, SeekFrom};
    ensure!(
        max_bytes > 0 && max_bytes <= 1024 * 1024,
        "max-bytes must be 1..1048576"
    );
    let mut file = std::fs::File::open(directory.join("output.live"))?;
    let end = file.metadata()?.len();
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

#[cfg(test)]
include!("process_read_tests.rs");
