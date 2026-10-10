//! Durable v3 intent and immutable capture batches, distinct from v1/v2 authority.
use anyhow::{Context, Result, ensure};
use bmux_pane_runtime_plugin_api::pane_runtime_execution_v2 as pane;
use bmux_recording_plugin_api::{
    recording_process_v1 as process, recording_process_v3 as contract,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Serialize, Deserialize)]
pub(crate) struct ProcessRecord {
    pub(crate) status: contract::ProcessStatus,
    pub(crate) options: process::ProgramOptions,
    pub(crate) capture_owner_epoch: Option<Uuid>,
}

#[derive(Serialize, Deserialize)]
struct InputIntent {
    schema_version: u32,
    data: Vec<u8>,
    receipt: Option<pane::InputReceipt>,
}

pub(crate) struct ProcessStore {
    root: PathBuf,
}
impl ProcessStore {
    pub(crate) fn new(root: &Path) -> Result<Self> {
        let root = root.join("process-v3");
        crate::durable::private_directory(&root)?;
        Ok(Self { root })
    }
    fn path(&self, run: Uuid) -> PathBuf {
        self.root.join(format!("{run}.json"))
    }
    pub(crate) fn load(&self, run: Uuid) -> Result<ProcessRecord> {
        let path = self.path(run);
        ensure!(
            std::fs::metadata(&path)?.len() <= 1024 * 1024,
            "process authority exceeds bound"
        );
        let value: ProcessRecord = serde_json::from_slice(&std::fs::read(path)?)
            .context("corrupt process-v3 authority")?;
        ensure!(
            value.status.schema_version == 3 && value.status.run_id == run,
            "unsupported process authority schema or identity"
        );
        Ok(value)
    }
    pub(crate) fn admit(&self, value: &ProcessRecord) -> Result<ProcessRecord> {
        if self.path(value.status.run_id).try_exists()? {
            let old = self.load(value.status.run_id)?;
            Self::same_intent(&old, value)?;
            return Ok(old);
        }
        ensure!(
            value.status.schema_version == 3 && value.status.revision == 1,
            "invalid initial process authority"
        );
        self.write(value)?;
        self.load(value.status.run_id)
    }
    fn same_intent(old: &ProcessRecord, value: &ProcessRecord) -> Result<()> {
        ensure!(
            old.status.launch_mutation_id == value.status.launch_mutation_id
                && old.status.runtime_root == value.status.runtime_root
                && serde_json::to_vec(&old.status.policy)?
                    == serde_json::to_vec(&value.status.policy)?
                && serde_json::to_vec(&old.options)? == serde_json::to_vec(&value.options)?,
            "conflicting process start identity, options or policy"
        );
        Ok(())
    }
    pub(crate) fn replace(&self, expected: u64, value: &mut ProcessRecord) -> Result<()> {
        let old = self.load(value.status.run_id)?;
        ensure!(old.status.revision == expected, "stale process revision");
        Self::same_intent(&old, value)?;
        ensure!(
            !matches!(
                old.status.capture,
                bmux_recording_plugin_api::recording_process_recovery_v2::CaptureState::Gap
            ) || matches!(
                value.status.capture,
                bmux_recording_plugin_api::recording_process_recovery_v2::CaptureState::Gap
            ),
            "capture gap cannot be cleared"
        );
        ensure!(
            !matches!(
                old.status.capture,
                bmux_recording_plugin_api::recording_process_recovery_v2::CaptureState::Finalized
            ) || matches!(
                value.status.capture,
                bmux_recording_plugin_api::recording_process_recovery_v2::CaptureState::Finalized
            ),
            "finalized capture cannot reactivate"
        );
        value.status.revision = expected
            .checked_add(1)
            .context("process revision exhausted")?;
        if let Err(error) = self.write(value) {
            value.status.revision = expected;
            return Err(error);
        }
        Ok(())
    }
    fn write(&self, value: &ProcessRecord) -> Result<()> {
        let bytes = serde_json::to_vec(value)?;
        ensure!(
            bytes.len() <= 1024 * 1024,
            "process authority exceeds bound"
        );
        crate::durable::write_atomic(&self.path(value.status.run_id), &bytes)
    }
    pub(crate) fn require_finalizable(value: &ProcessRecord) -> Result<()> {
        let authority = value
            .status
            .authority
            .as_ref()
            .context("execution authority unavailable")?;
        let cursor = value
            .status
            .cursor
            .as_ref()
            .context("capture cursor unavailable")?;
        ensure!(
            value.status.terminal_session_ended,
            "terminal session has not ended"
        );
        ensure!(
            value.status.source_evidence_complete,
            "source timing/profile evidence incomplete; raw recovery is not full recording finalization"
        );
        ensure!(!authority.recovery_required && !value.status.recovery_required && authority.capture_complete
            && matches!(value.status.capture, bmux_recording_plugin_api::recording_process_recovery_v2::CaptureState::Contiguous | bmux_recording_plugin_api::recording_process_recovery_v2::CaptureState::Finalized)
            && serde_json::to_vec(cursor)? == serde_json::to_vec(&authority.continuation_cursor)?, "raw capture incomplete, gap or recovery required");
        Ok(())
    }
    fn mutation_path(&self, run: Uuid, id: Uuid) -> PathBuf {
        self.root.join(format!("{run}-input-{id}.json"))
    }
    pub(crate) fn admit_input(
        &self,
        run: Uuid,
        id: Uuid,
        data: &[u8],
    ) -> Result<Option<pane::InputReceipt>> {
        ensure!(data.len() <= 65536, "input exceeds admission bound");
        self.load(run)?;
        let path = self.mutation_path(run, id);
        if path.try_exists()? {
            let intent = Self::read_input(&path)?;
            ensure!(
                intent.schema_version == 3 && intent.data == data,
                "conflicting input identity"
            );
            return Ok(intent.receipt);
        }
        let count = std::fs::read_dir(&self.root)?
            .filter_map(std::result::Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{run}-input-"))
            })
            .count();
        ensure!(count < 1024, "input intent capacity reached");
        crate::durable::write_atomic(
            &path,
            &serde_json::to_vec(&InputIntent {
                schema_version: 3,
                data: data.to_vec(),
                receipt: None,
            })?,
        )?;
        Ok(None)
    }
    fn read_input(path: &Path) -> Result<InputIntent> {
        ensure!(
            std::fs::metadata(path)?.len() <= 512 * 1024,
            "input receipt exceeds bound"
        );
        let value: InputIntent =
            serde_json::from_slice(&std::fs::read(path)?).context("corrupt input intent")?;
        ensure!(value.schema_version == 3, "unsupported input intent schema");
        Ok(value)
    }
    pub(crate) fn complete_input(
        &self,
        run: Uuid,
        id: Uuid,
        receipt: pane::InputReceipt,
    ) -> Result<()> {
        let path = self.mutation_path(run, id);
        let mut value = Self::read_input(&path)?;
        ensure!(
            receipt.input_mutation_id == id,
            "input receipt identity mismatch"
        );
        if let Some(previous) = &value.receipt {
            ensure!(
                serde_json::to_vec(previous)? == serde_json::to_vec(&receipt)?,
                "input outcome changed"
            );
            return Ok(());
        }
        value.receipt = Some(receipt);
        crate::durable::write_atomic(&path, &serde_json::to_vec(&value)?)
    }
    fn batch_path(&self, run: Uuid, cursor: &pane::CaptureCursor) -> PathBuf {
        self.root.join(format!(
            "{run}-{}-{}.batch",
            cursor.spool_generation, cursor.offset
        ))
    }
    pub(crate) fn batch(
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
        let batch: pane::CaptureBatch = serde_json::from_slice(&std::fs::read(path)?)?;
        ensure!(
            serde_json::to_vec(&batch.cursor)? == serde_json::to_vec(cursor)?
                && batch.data.len() <= 65536,
            "committed capture identity mismatch"
        );
        Ok(Some(batch))
    }
    pub(crate) fn commit(
        &self,
        value: &mut ProcessRecord,
        batch: &pane::CaptureBatch,
    ) -> Result<()> {
        let cursor = value
            .status
            .cursor
            .as_ref()
            .context("capture cursor unavailable")?;
        ensure!(
            serde_json::to_vec(cursor)? == serde_json::to_vec(&batch.cursor)?
                && batch.continuation.spool_generation == cursor.spool_generation
                && serde_json::to_vec(&batch.continuation.execution)?
                    == serde_json::to_vec(&cursor.execution)?
                && batch.data.len() <= 65536
                && batch.continuation.offset
                    == cursor
                        .offset
                        .checked_add(u64::try_from(batch.data.len())?)
                        .context("capture cursor exhausted")?,
            "capture discontinuity"
        );
        let path = self.batch_path(value.status.run_id, cursor);
        let bytes = serde_json::to_vec(batch)?;
        if path.try_exists()? {
            ensure!(
                std::fs::read(&path)? == bytes,
                "conflicting committed capture"
            );
        } else {
            crate::durable::write_atomic(&path, &bytes)?;
        }
        let expected = value.status.revision;
        let previous = value.status.cursor.replace(batch.continuation.clone());
        if let Err(error) = self.replace(expected, value) {
            value.status.cursor = previous;
            return Err(error);
        }
        Ok(())
    }
}

#[cfg(test)]
include!("process_v3_store_tests.rs");
