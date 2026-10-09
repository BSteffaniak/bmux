//! Recording-owned durable process metadata; interaction is execution-token scoped.
use anyhow::{Context, Result, ensure};
use bmux_recording_plugin_api::recording_process_v1::{RunPhase, RunStatus};
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub(crate) struct ProcessStore {
    root: PathBuf,
}

impl ProcessStore {
    pub(crate) fn new(recordings_root: &Path) -> Result<Self> {
        let root = recordings_root.join("process-runs-v1");
        crate::durable::private_directory(&root)?;
        std::fs::File::open(recordings_root)?.sync_all()?;
        Ok(Self { root })
    }

    pub(crate) fn load(&self, run_id: Uuid) -> Result<RunStatus> {
        let bytes = std::fs::read(self.path(run_id)).context("reading process run metadata")?;
        let value: RunStatus =
            serde_json::from_slice(&bytes).context("corrupt process run metadata")?;
        ensure!(
            value.schema_version == 1,
            "unsupported process run schema version"
        );
        ensure!(value.run_id == run_id, "process run identity mismatch");
        ensure!(value.revision > 0, "invalid process run revision");
        Ok(value)
    }

    // Caller serializes authority mutations under the process runtime mutex.
    pub(crate) fn create(&self, value: &RunStatus) -> Result<()> {
        ensure!(
            value.schema_version == 1 && value.revision == 1,
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
            schema_version: 1,
            run_id: Uuid::new_v4(),
            revision: 1,
            recording_id: Uuid::new_v4(),
            execution: None,
            runtime_root: "/isolated/runtime".into(),
            phase: RunPhase::Starting,
            exit_code: None,
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
