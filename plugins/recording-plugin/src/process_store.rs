//! Recording-owned durable process metadata; interaction is execution-token scoped.
use anyhow::{Context, Result, ensure};
use bmux_recording_plugin_api::recording_process_v4::{RunPhase, RunStatus};
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub(crate) struct ProcessStore {
    root: PathBuf,
}

impl ProcessStore {
    pub(crate) fn new(recordings_root: &Path) -> Result<Self> {
        let root = recordings_root.join("process-runs-v4");
        crate::durable::private_directory(&root)?;
        std::fs::File::open(recordings_root)?.sync_all()?;
        Ok(Self { root })
    }

    pub(crate) fn load(&self, run_id: Uuid) -> Result<RunStatus> {
        let bytes = std::fs::read(self.path(run_id)).context("reading process run metadata")?;
        let value: RunStatus =
            serde_json::from_slice(&bytes).context("corrupt process run metadata")?;
        ensure!(
            value.schema_version == 4,
            "unsupported process run schema version"
        );
        ensure!(value.run_id == run_id, "process run identity mismatch");
        ensure!(value.revision > 0, "invalid process run revision");
        Ok(value)
    }

    // Caller serializes authority mutations under the process runtime mutex.
    pub(crate) fn create(&self, value: &RunStatus) -> Result<()> {
        ensure!(
            value.schema_version == 4 && value.revision == 1,
            "invalid initial process metadata"
        );
        ensure!(
            !self.path(value.run_id).exists(),
            "process run already exists"
        );
        self.write(value)
    }

    pub(crate) fn replace(&self, expected_revision: u64, value: &mut RunStatus) -> Result<()> {
        let old = self.load(value.run_id)?;
        ensure!(
            old.revision == expected_revision,
            "stale process run revision"
        );
        ensure!(
            old.recording_id == value.recording_id,
            "recording identity changed"
        );
        ensure!(
            old.runtime_root == value.runtime_root,
            "runtime locator changed"
        );
        if let Some(execution) = &old.execution {
            let new = value
                .execution
                .as_ref()
                .context("execution identity removed")?;
            ensure!(
                execution.session_id == new.session_id
                    && execution.pane_id == new.pane_id
                    && execution.execution_token == new.execution_token,
                "execution identity changed"
            );
        }
        ensure!(
            !old.capture_finalized || value.capture_finalized,
            "capture finalization regressed"
        );
        ensure!(
            !matches!(old.phase, RunPhase::Finalized) || matches!(value.phase, RunPhase::Finalized),
            "finalized run reactivated"
        );
        let next = expected_revision
            .checked_add(1)
            .context("process revision exhausted")?;
        let previous = value.revision;
        value.revision = next;
        if let Err(error) = self.write(value) {
            value.revision = previous;
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn admit_launch(&self, _run: Uuid, mutation: Uuid, intent: &[u8]) -> Result<bool> {
        self.admit(&self.root.join(format!("launch-{mutation}.json")), intent)
    }
    pub(crate) fn admit_input(&self, run: Uuid, mutation: Uuid, data: &[u8]) -> Result<bool> {
        self.admit(
            &self.root.join(format!("input-{run}-{mutation}.json")),
            data,
        )
    }
    fn admit(&self, path: &Path, data: &[u8]) -> Result<bool> {
        use std::io::Write;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut file) => {
                file.write_all(data)?;
                file.sync_all()?;
                std::fs::File::open(&self.root)?.sync_all()?;
                Ok(false)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                ensure!(
                    std::fs::read(path)? == data,
                    "conflicting mutation reuse or incomplete admission"
                );
                Ok(true)
            }
            Err(error) => Err(error.into()),
        }
    }
    pub(crate) fn admit_resolution(&self, run: Uuid, mutation: Uuid, data: &[u8]) -> Result<bool> {
        self.admit(
            &self.root.join(format!("resolution-{run}-{mutation}.json")),
            data,
        )
    }
    pub(crate) fn write_authority(
        &self,
        run: Uuid,
        value: &bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalReconciliation,
    ) -> Result<()> {
        crate::durable::write_atomic(
            &self.root.join(format!("authority-{run}.json")),
            &serde_json::to_vec(value)?,
        )
    }
    pub(crate) fn write_reservation(
        &self,
        run: Uuid,
        value: &bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalLaunchReservation,
    ) -> Result<()> {
        crate::durable::write_atomic(
            &self.root.join(format!("reservation-{run}.json")),
            &serde_json::to_vec(value)?,
        )
    }
    pub(crate) fn reservation(
        &self,
        run: Uuid,
    ) -> Result<bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalLaunchReservation>
    {
        Ok(serde_json::from_slice(&std::fs::read(
            self.root.join(format!("reservation-{run}.json")),
        )?)?)
    }
    pub(crate) fn record_input_once(
        &self,
        run: Uuid,
        mutation: Uuid,
        record: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let path = self.root.join(format!("input-event-{run}-{mutation}.json"));
        if self.admit(&path, b"pending-original-source-event")? {
            let complete = self
                .root
                .join(format!("input-event-complete-{run}-{mutation}.json"));
            ensure!(
                complete.try_exists()?,
                "input source event publication uncertain; original timing cannot be replayed"
            );
            return Ok(());
        }
        record()?;
        crate::durable::write_atomic(
            &self
                .root
                .join(format!("input-event-complete-{run}-{mutation}.json")),
            b"true",
        )
    }

    pub(crate) fn input_receipt(
        &self,
        run: Uuid,
        mutation: Uuid,
    ) -> Result<Option<bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalInputReceipt>>
    {
        let path = self
            .root
            .join(format!("input-receipt-{run}-{mutation}.json"));
        if !path.try_exists()? {
            return Ok(None);
        }
        let receipt: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalInputReceipt =
            serde_json::from_slice(&std::fs::read(path)?)?;
        ensure!(
            receipt.input_mutation_id == mutation
                && receipt.execution == self.reservation(run)?.execution,
            "input receipt identity mismatch"
        );
        Ok(Some(receipt))
    }

    pub(crate) fn write_input_receipt(
        &self,
        run: Uuid,
        mutation: Uuid,
        value: &bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalInputReceipt,
    ) -> Result<()> {
        crate::durable::write_atomic(
            &self
                .root
                .join(format!("input-receipt-{run}-{mutation}.json")),
            &serde_json::to_vec(value)?,
        )
    }

    pub(crate) fn capture_cursor(
        &self,
        run: Uuid,
    ) -> Result<
        Option<bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalCaptureCursor>,
    > {
        let path = self.root.join(format!("capture-cursor-{run}.json"));
        if !path.try_exists()? {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&std::fs::read(path)?)?))
    }
    pub(crate) fn commit_raw_batch(
        &self,
        run: Uuid,
        batch: &bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalCaptureBatch,
    ) -> Result<()> {
        ensure!(
            batch.cursor.execution == batch.continuation.execution
                && batch.cursor.spool_generation == batch.continuation.spool_generation
                && batch
                    .cursor
                    .offset
                    .checked_add(u64::try_from(batch.data.len())?)
                    == Some(batch.continuation.offset),
            "invalid canonical raw batch"
        );
        let reservation = self.reservation(run)?;
        ensure!(
            batch.cursor.execution == reservation.execution
                && batch.cursor.spool_generation == reservation.initial_cursor.spool_generation,
            "canonical raw batch belongs to a different admitted execution"
        );
        let committed = self
            .capture_cursor(run)?
            .unwrap_or(reservation.initial_cursor);
        ensure!(
            committed == batch.cursor || committed == batch.continuation,
            "canonical capture gap or stale batch cursor; no cursor repair by guessing"
        );
        let dir = self.root.join(format!("recovered-raw-{run}"));
        crate::durable::private_directory(&dir)?;
        let path = dir.join(format!(
            "{}-{}.json",
            batch.cursor.offset, batch.continuation.offset
        ));
        let data = serde_json::to_vec(batch)?;
        if path.try_exists()? {
            ensure!(
                std::fs::read(&path)? == data,
                "conflicting replayed canonical raw batch"
            );
        } else {
            crate::durable::write_atomic(&path, &data)?;
        }
        // The raw segment commits before its continuation cursor. Crash replay
        // validates identical bytes; this never manufactures recording events.
        crate::durable::write_atomic(
            &self.root.join(format!("capture-cursor-{run}.json")),
            &serde_json::to_vec(&batch.continuation)?,
        )
    }

    fn path(&self, run_id: Uuid) -> PathBuf {
        self.root.join(format!("{run_id}.json"))
    }

    fn write(&self, value: &RunStatus) -> Result<()> {
        ensure!(
            !value.capture_finalized
                || (value.output_drained
                    && !matches!(value.phase, RunPhase::Running | RunPhase::Starting)),
            "cannot finalize capture before process exit and output drain"
        );
        crate::durable::write_atomic(&self.path(value.run_id), &serde_json::to_vec_pretty(value)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status() -> RunStatus {
        RunStatus {
            schema_version: 4,
            run_id: Uuid::new_v4(),
            revision: 1,
            launch_mutation_id: Uuid::new_v4(),
            recording_id: Uuid::new_v4(),
            execution: None,
            runtime_root: "/isolated/runtime".into(),
            phase: RunPhase::Starting,
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

    #[test]
    fn revisions_and_capture_finalization_are_enforced() {
        let root = std::env::temp_dir().join(format!("bmux-process-store-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let store = ProcessStore::new(&root).unwrap();
        let mut value = status();
        store.create(&value).unwrap();
        assert!(store.create(&value).is_err());
        assert!(store.replace(2, &mut value).is_err());
        value.capture_finalized = true;
        assert!(store.replace(1, &mut value).is_err());
        value.capture_finalized = false;
        value.phase = RunPhase::Running;
        store.replace(1, &mut value).unwrap();
        assert_eq!(store.load(value.run_id).unwrap().revision, 2);
        value.phase = RunPhase::Finalized;
        value.output_drained = true;
        value.capture_finalized = true;
        store.replace(2, &mut value).unwrap();
        value.phase = RunPhase::Running;
        assert!(store.replace(3, &mut value).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn admitted_mutations_replay_identically_and_conflicts_preserve_original_bytes() {
        let root = std::env::temp_dir().join(format!("bmux-process-admission-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let store = ProcessStore::new(&root).unwrap();
        let run = Uuid::new_v4();
        let key = Uuid::new_v4();
        assert!(!store.admit_launch(run, key, b"canonical-launch").unwrap());
        assert!(store.admit_launch(run, key, b"canonical-launch").unwrap());
        assert!(store.admit_launch(run, key, b"different-launch").is_err());
        assert!(!store.admit_input(run, key, b"original-input").unwrap());
        assert!(store.admit_input(run, key, b"original-input").unwrap());
        assert!(store.admit_input(run, key, b"different-input").is_err());
        assert!(store.admit_input(run, key, b"original-input").unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn canonical_raw_commit_rejects_gaps_wrong_owner_and_conflicting_replay() {
        use bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::{
            TerminalCaptureBatch, TerminalCaptureCursor, TerminalExecutionRef,
            TerminalLaunchReservation, TerminalProgramOptions,
        };
        let root = std::env::temp_dir().join(format!("bmux-capture-gap-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let store = ProcessStore::new(&root).unwrap();
        let run = Uuid::new_v4();
        let execution = TerminalExecutionRef {
            session_id: Uuid::new_v4(),
            pane_id: Uuid::new_v4(),
            execution_token: Uuid::new_v4(),
        };
        let cursor = TerminalCaptureCursor {
            execution: execution.clone(),
            spool_generation: Uuid::new_v4(),
            offset: 0,
        };
        let reservation = TerminalLaunchReservation {
            execution,
            launch_mutation_id: Uuid::new_v4(),
            revision: 1,
            options: TerminalProgramOptions {
                program: "/bin/echo".into(),
                args: vec![],
                cwd: None,
                env: std::collections::BTreeMap::new(),
                cols: 80,
                rows: 24,
            },
            initial_cursor: cursor.clone(),
        };
        store.write_reservation(run, &reservation).unwrap();
        let mut continuation = cursor.clone();
        continuation.offset = 3;
        let batch = TerminalCaptureBatch {
            cursor,
            continuation,
            data: vec![0, 255, 7],
            complete: false,
        };
        store.commit_raw_batch(run, &batch).unwrap();
        store.commit_raw_batch(run, &batch).unwrap();
        assert_eq!(
            store.capture_cursor(run).unwrap(),
            Some(batch.continuation.clone())
        );
        let mut conflict = batch.clone();
        conflict.data[0] = 1;
        assert!(store.commit_raw_batch(run, &conflict).is_err());
        let mut gap = batch.clone();
        gap.cursor.offset = 9;
        gap.continuation.offset = 12;
        assert!(store.commit_raw_batch(run, &gap).is_err());
        let mut wrong_owner = batch.clone();
        wrong_owner.cursor.execution.execution_token = Uuid::new_v4();
        wrong_owner.continuation.execution = wrong_owner.cursor.execution.clone();
        assert!(store.commit_raw_batch(run, &wrong_owner).is_err());
        assert_eq!(store.capture_cursor(run).unwrap(), Some(batch.continuation));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unknown_schema_is_not_an_empty_installation() {
        let root = std::env::temp_dir().join(format!("bmux-process-schema-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let store = ProcessStore::new(&root).unwrap();
        let mut value = status();
        store.create(&value).unwrap();
        value.schema_version = 2;
        crate::durable::write_atomic(
            &store.path(value.run_id),
            &serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        assert!(store.load(value.run_id).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
