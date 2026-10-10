//! Recording-owned recovery authority. Immutable capture batches commit bytes and
//! their cursor together; the state cursor advances only after that commit.
use anyhow::{Context, Result, ensure};
use bmux_pane_runtime_plugin_api::pane_runtime_execution_v2 as pane;
use bmux_recording_plugin_api::{
    recording_process_recovery_v2 as recovery, recording_process_v1 as process,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const MAX_STATE_BYTES: u64 = 1024 * 1024;
const MAX_BATCH_BYTES: usize = 65_536;

#[derive(Serialize, Deserialize)]
pub(crate) struct RecoveryRecord {
    pub(crate) status: recovery::RecoveryStatus,
    pub(crate) options: process::ProgramOptions,
}

#[derive(Serialize, Deserialize)]
struct MutationRecord {
    schema_version: u32,
    request: serde_json::Value,
    receipt: Option<serde_json::Value>,
}

#[derive(Clone)]
pub(crate) struct RecoveryStore {
    root: PathBuf,
}

impl RecoveryStore {
    pub(crate) fn new(recordings_root: &Path) -> Result<Self> {
        let root = recordings_root.join("process-recovery-v2");
        crate::durable::private_directory(&root)?;
        Ok(Self { root })
    }

    fn path(&self, id: Uuid) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }

    pub(crate) fn load(&self, id: Uuid) -> Result<RecoveryRecord> {
        let path = self.path(id);
        ensure!(
            std::fs::metadata(&path)?.len() <= MAX_STATE_BYTES,
            "recovery state exceeds bound"
        );
        let record: RecoveryRecord = serde_json::from_slice(&std::fs::read(path)?)
            .context("corrupt recording recovery authority")?;
        ensure!(
            record.status.schema_version == 2
                && record.status.run_id == id
                && record.status.revision > 0,
            "unsupported or inconsistent recording recovery authority"
        );
        Ok(record)
    }

    pub(crate) fn admit(&self, record: &RecoveryRecord) -> Result<RecoveryRecord> {
        if self.path(record.status.run_id).try_exists()? {
            let previous = self.load(record.status.run_id)?;
            ensure!(
                previous.status.launch_mutation_id == record.status.launch_mutation_id
                    && previous.status.runtime_root == record.status.runtime_root
                    && serde_json::to_vec(&previous.options)?
                        == serde_json::to_vec(&record.options)?,
                "conflicting reuse of recording launch identity"
            );
            return Ok(previous);
        }
        ensure!(
            record.status.schema_version == 2 && record.status.revision == 1,
            "invalid initial recovery authority"
        );
        self.write(record)?;
        self.load(record.status.run_id)
    }

    pub(crate) fn replace(&self, expected: u64, record: &mut RecoveryRecord) -> Result<()> {
        let old = self.load(record.status.run_id)?;
        ensure!(
            old.status.revision == expected,
            "stale recording recovery revision"
        );
        ensure!(
            old.status.launch_mutation_id == record.status.launch_mutation_id
                && old.status.runtime_root == record.status.runtime_root
                && serde_json::to_vec(&old.options)? == serde_json::to_vec(&record.options)?,
            "recording recovery identity changed"
        );
        ensure!(
            !matches!(old.status.capture, recovery::CaptureState::Gap)
                || matches!(record.status.capture, recovery::CaptureState::Gap),
            "canonical raw evidence gap cannot be repaired by a screen snapshot"
        );
        ensure!(
            !matches!(old.status.capture, recovery::CaptureState::Finalized)
                || matches!(record.status.capture, recovery::CaptureState::Finalized),
            "finalized capture reactivated"
        );
        record.status.revision = expected
            .checked_add(1)
            .context("recovery revision exhausted")?;
        if let Err(error) = self.write(record) {
            record.status.revision = expected;
            return Err(error);
        }
        Ok(())
    }

    fn write(&self, record: &RecoveryRecord) -> Result<()> {
        let bytes = serde_json::to_vec(record)?;
        ensure!(
            u64::try_from(bytes.len())? <= MAX_STATE_BYTES,
            "recovery state exceeds bound"
        );
        crate::durable::write_atomic(&self.path(record.status.run_id), &bytes)
    }

    fn batch_path(&self, run: Uuid, cursor: &pane::CaptureCursor) -> PathBuf {
        self.root.join(format!(
            "{run}-{}-{}.batch",
            cursor.spool_generation, cursor.offset
        ))
    }

    pub(crate) fn committed_batch(
        &self,
        run: Uuid,
        cursor: &pane::CaptureCursor,
    ) -> Result<Option<pane::CaptureBatch>> {
        let path = self.batch_path(run, cursor);
        if !path.try_exists()? {
            return Ok(None);
        }
        ensure!(
            std::fs::metadata(&path)?.len() <= 512 * 1024,
            "capture batch exceeds bound"
        );
        let batch: pane::CaptureBatch = serde_json::from_slice(&std::fs::read(path)?)
            .context("corrupt committed capture batch")?;
        ensure!(
            serde_json::to_vec(&batch.cursor)? == serde_json::to_vec(cursor)?,
            "committed batch identity mismatch"
        );
        ensure!(
            batch.data.len() <= MAX_BATCH_BYTES,
            "committed capture batch exceeds bound"
        );
        Ok(Some(batch))
    }

    pub(crate) fn commit_batch(
        &self,
        record: &mut RecoveryRecord,
        batch: &pane::CaptureBatch,
    ) -> Result<()> {
        let cursor = record
            .status
            .cursor
            .as_ref()
            .context("capture has no authoritative cursor")?;
        ensure!(
            serde_json::to_vec(cursor)? == serde_json::to_vec(&batch.cursor)?,
            "capture cursor discontinuity"
        );
        ensure!(
            batch.data.len() <= MAX_BATCH_BYTES
                && batch.continuation.spool_generation == cursor.spool_generation
                && serde_json::to_vec(&batch.continuation.execution)?
                    == serde_json::to_vec(&cursor.execution)?
                && batch.continuation.offset
                    == cursor
                        .offset
                        .checked_add(u64::try_from(batch.data.len())?)
                        .context("capture cursor exhausted")?,
            "invalid capture continuation"
        );
        let path = self.batch_path(record.status.run_id, cursor);
        let encoded = serde_json::to_vec(batch)?;
        if path.try_exists()? {
            // Retry after batch commit but before state commit must deduplicate the
            // exact bytes/cursor, never append output twice or accept replacement.
            ensure!(
                std::fs::metadata(&path)?.len() <= 512 * 1024 && std::fs::read(&path)? == encoded,
                "conflicting committed capture batch"
            );
        } else {
            crate::durable::write_atomic(&path, &encoded)?;
        }
        let expected = record.status.revision;
        let previous = record.status.cursor.replace(batch.continuation.clone());
        if let Err(error) = self.replace(expected, record) {
            record.status.cursor = previous;
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn admit_mutation(
        &self,
        run: Uuid,
        id: Uuid,
        kind: &str,
        request: &serde_json::Value,
        limit: usize,
    ) -> Result<Option<serde_json::Value>> {
        let directory = self.root.join(format!("{run}-mutations"));
        crate::durable::private_directory(&directory)?;
        let path = directory.join(format!("{kind}-{id}.json"));
        if path.try_exists()? {
            let previous = Self::read_mutation(&path)?;
            ensure!(
                previous.request == *request,
                "conflicting mutation identity reuse"
            );
            return Ok(previous.receipt);
        }
        let mut count = 0_usize;
        let mut bytes = 0_u64;
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            ensure!(
                entry.file_type()?.is_file(),
                "unexpected mutation authority entry"
            );
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(&format!("{kind}-"))
            {
                count = count.checked_add(1).context("mutation count exhausted")?;
            }
            bytes = bytes
                .checked_add(entry.metadata()?.len())
                .context("mutation budget exhausted")?;
            ensure!(
                count < limit && bytes <= 8 * 1024 * 1024,
                "mutation receipt admission capacity reached"
            );
        }
        let intent = MutationRecord {
            schema_version: 2,
            request: request.clone(),
            receipt: None,
        };
        let encoded = serde_json::to_vec(&intent)?;
        ensure!(
            bytes.saturating_add(u64::try_from(encoded.len())?) <= 8 * 1024 * 1024,
            "mutation receipt admission byte capacity reached"
        );
        crate::durable::write_atomic(&path, &encoded)?;
        Ok(None)
    }

    fn read_mutation(path: &Path) -> Result<MutationRecord> {
        ensure!(
            std::fs::metadata(path)?.len() <= MAX_STATE_BYTES,
            "mutation receipt exceeds bound"
        );
        let record: MutationRecord =
            serde_json::from_slice(&std::fs::read(path)?).context("corrupt mutation authority")?;
        ensure!(
            record.schema_version == 2,
            "unsupported mutation authority schema"
        );
        Ok(record)
    }

    pub(crate) fn complete_mutation(
        &self,
        run: Uuid,
        id: Uuid,
        kind: &str,
        receipt: serde_json::Value,
    ) -> Result<()> {
        let path = self
            .root
            .join(format!("{run}-mutations"))
            .join(format!("{kind}-{id}.json"));
        let mut record = Self::read_mutation(&path)?;
        if let Some(previous) = &record.receipt {
            ensure!(*previous == receipt, "mutation outcome changed");
            return Ok(());
        }
        record.receipt = Some(receipt);
        crate::durable::write_atomic(&path, &serde_json::to_vec(&record)?)
    }

    pub(crate) fn require_finalizable(record: &RecoveryRecord) -> Result<()> {
        let authority = record
            .status
            .authority
            .as_ref()
            .context("execution authority unavailable")?;
        let cursor = record
            .status
            .cursor
            .as_ref()
            .context("capture cursor unavailable")?;
        ensure!(
            authority.launch.cleanup_complete,
            "explicit owned-execution cleanup required; target exit alone is insufficient"
        );
        ensure!(
            !authority.recovery_required
                && !record.status.recovery_required
                && authority.capture_complete
                && !matches!(
                    record.status.capture,
                    recovery::CaptureState::Gap | recovery::CaptureState::Failed
                )
                && serde_json::to_vec(cursor)?
                    == serde_json::to_vec(&authority.continuation_cursor)?,
            "capture incomplete or recovery required"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> RecoveryRecord {
        RecoveryRecord {
            status: recovery::RecoveryStatus {
                schema_version: 2,
                run_id: Uuid::new_v4(),
                revision: 1,
                launch_mutation_id: Uuid::new_v4(),
                recording_id: None,
                runtime_root: "/isolated/runtime".into(),
                reservation: None,
                authority: None,
                cursor: None,
                capture: recovery::CaptureState::Pending,
                recovery_required: true,
                errors: vec![],
            },
            options: process::ProgramOptions {
                program: "/bin/echo".into(),
                args: vec!["ready".into()],
                cwd: None,
                env: std::collections::BTreeMap::new(),
                cols: 80,
                rows: 24,
                profile: bmux_recording_plugin_api::recording_types::RecordingProfile::Full,
            },
        }
    }

    #[test]
    fn restart_retains_identity_and_conflicting_admission_is_rejected() {
        let root = std::env::temp_dir().join(format!("bmux-recovery-test-{}", Uuid::new_v4()));
        let store = RecoveryStore::new(&root).unwrap();
        let mut original = record();
        store.admit(&original).unwrap();
        let reopened = RecoveryStore::new(&root).unwrap();
        assert_eq!(
            reopened.admit(&original).unwrap().status.launch_mutation_id,
            original.status.launch_mutation_id
        );
        original.options.args.push("conflict".into());
        assert!(reopened.admit(&original).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn committed_batch_survives_cursor_commit_interruption_without_replacement() {
        let root = std::env::temp_dir().join(format!("bmux-recovery-batch-{}", Uuid::new_v4()));
        let store = RecoveryStore::new(&root).unwrap();
        let mut value = record();
        let execution = pane::ExecutionRef {
            session_id: Uuid::new_v4(),
            pane_id: Uuid::new_v4(),
            execution_token: Uuid::new_v4(),
        };
        let cursor = pane::CaptureCursor {
            execution,
            spool_generation: Uuid::new_v4(),
            offset: 0,
        };
        value.status.cursor = Some(cursor.clone());
        let mut value = store.admit(&value).unwrap();
        let mut continuation = cursor.clone();
        continuation.offset = 4;
        let batch = pane::CaptureBatch {
            cursor: cursor.clone(),
            continuation,
            data: vec![0, 255, 1, 2],
            complete: true,
        };
        // Simulate a crash immediately after the immutable batch commit.
        crate::durable::write_atomic(
            &store.batch_path(value.status.run_id, &cursor),
            &serde_json::to_vec(&batch).unwrap(),
        )
        .unwrap();
        let reopened = RecoveryStore::new(&root).unwrap();
        let committed = reopened
            .committed_batch(value.status.run_id, &cursor)
            .unwrap()
            .unwrap();
        assert_eq!(committed.data, batch.data);
        reopened.commit_batch(&mut value, &committed).unwrap();
        assert_eq!(
            reopened
                .load(value.status.run_id)
                .unwrap()
                .status
                .cursor
                .unwrap()
                .offset,
            4
        );
        let mut conflicting = batch.clone();
        conflicting.data[0] = 99;
        value.status.cursor = Some(cursor);
        assert!(reopened.commit_batch(&mut value, &conflicting).is_err());
        assert_eq!(
            reopened
                .committed_batch(value.status.run_id, &batch.cursor)
                .unwrap()
                .unwrap()
                .data,
            batch.data
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn input_intent_and_receipt_survive_restart_and_reject_conflicting_reuse() {
        let root = std::env::temp_dir().join(format!("bmux-recovery-input-{}", Uuid::new_v4()));
        let store = RecoveryStore::new(&root).unwrap();
        let value = record();
        store.admit(&value).unwrap();
        let id = Uuid::new_v4();
        let request = serde_json::json!({"data":[0,255]});
        assert!(
            store
                .admit_mutation(value.status.run_id, id, "input", &request, 10)
                .unwrap()
                .is_none()
        );
        let reopened = RecoveryStore::new(&root).unwrap();
        assert!(
            reopened
                .admit_mutation(value.status.run_id, id, "input", &request, 10)
                .unwrap()
                .is_none()
        );
        assert!(
            reopened
                .admit_mutation(
                    value.status.run_id,
                    id,
                    "input",
                    &serde_json::json!({"data":[1]}),
                    10
                )
                .is_err()
        );
        let receipt = serde_json::json!({"completed":true});
        reopened
            .complete_mutation(value.status.run_id, id, "input", receipt.clone())
            .unwrap();
        assert_eq!(
            store
                .admit_mutation(value.status.run_id, id, "input", &request, 10)
                .unwrap(),
            Some(receipt.clone())
        );
        assert!(
            store
                .complete_mutation(
                    value.status.run_id,
                    id,
                    "input",
                    serde_json::json!({"completed":false})
                )
                .is_err()
        );
        assert_eq!(
            store
                .admit_mutation(value.status.run_id, id, "input", &request, 10)
                .unwrap(),
            Some(receipt)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn capture_gap_survives_restart_and_cannot_be_marked_contiguous() {
        let root = std::env::temp_dir().join(format!("bmux-recovery-gap-{}", Uuid::new_v4()));
        let store = RecoveryStore::new(&root).unwrap();
        let mut value = store.admit(&record()).unwrap();
        value.status.capture = recovery::CaptureState::Gap;
        store.replace(1, &mut value).unwrap();
        let reopened = RecoveryStore::new(&root).unwrap();
        let mut value = reopened.load(value.status.run_id).unwrap();
        value.status.capture = recovery::CaptureState::Contiguous;
        assert!(reopened.replace(2, &mut value).is_err());
        assert!(RecoveryStore::require_finalizable(&value).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
