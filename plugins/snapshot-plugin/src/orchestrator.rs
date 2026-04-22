//! Concrete `SnapshotOrchestrator` implementation.
//!
//! Iterates the `StatefulPluginRegistry` to build/restore a
//! `CombinedSnapshotEnvelope`, writes it to the configured path with
//! an atomic rename-on-write pattern, and maintains a status record
//! the `status()` method returns.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use bmux_plugin_sdk::{StatefulPluginError, StatefulPluginSnapshot};
use bmux_snapshot_runtime::{
    DryRunReport, RestoreSummary, SnapshotDirtyFlag, SnapshotOrchestrator,
    SnapshotOrchestratorError, SnapshotOrchestratorResult, SnapshotStatusReport,
    StatefulPluginRegistry,
};
use tracing::warn;

use bmux_snapshot_plugin_api::envelope::{CombinedSnapshotEnvelope, SectionV1};

/// Internal status bookkeeping.
#[derive(Debug, Default)]
#[allow(clippy::struct_field_names)]
struct Status {
    last_write_epoch_ms: Option<u64>,
    last_restore_epoch_ms: Option<u64>,
    last_restore_error: Option<String>,
}

/// Default file name used when a parent dir is given but no explicit
/// file — not used today, reserved for future CLI paths.
#[cfg(test)]
pub(crate) const DEFAULT_SNAPSHOT_FILENAME: &str = "bmux-snapshot-v1.json";

/// The concrete orchestrator implementation. Holds:
///
/// - `path`: snapshot file path (or `None` when persistence is disabled).
/// - `dirty_flag`: shared dirty flag the server flips on state mutations.
/// - `stateful_registry`: append-only registry of participants, populated
///   by each plugin (and the server's pane-runtime) during activate.
/// - `status`: interior-mutable status record.
pub struct BmuxSnapshotOrchestrator {
    path: Option<PathBuf>,
    dirty_flag: Arc<SnapshotDirtyFlag>,
    stateful_registry: Arc<RwLock<StatefulPluginRegistry>>,
    status: Mutex<Status>,
}

impl BmuxSnapshotOrchestrator {
    /// Construct an orchestrator. `path` is `None` when snapshot
    /// persistence is disabled (no CLI config registered).
    #[must_use]
    pub fn new(
        path: Option<PathBuf>,
        dirty_flag: Arc<SnapshotDirtyFlag>,
        stateful_registry: Arc<RwLock<StatefulPluginRegistry>>,
    ) -> Self {
        Self {
            path,
            dirty_flag,
            stateful_registry,
            status: Mutex::new(Status::default()),
        }
    }

    /// Synchronous variant of `save_now()` for the debounce-flush
    /// background thread. Returns the written path (or `None` if
    /// persistence is disabled) / a `SnapshotOrchestratorError`.
    pub(crate) fn save_now_blocking(&self) -> SnapshotOrchestratorResult<Option<PathBuf>> {
        let Some(path) = self.path.clone() else {
            return Ok(None);
        };
        let sections = self.collect_sections()?;
        let envelope = CombinedSnapshotEnvelope::build(sections)?;
        write_envelope_atomic(&path, &envelope)?;
        if let Ok(mut status) = self.status.lock() {
            status.last_write_epoch_ms = Some(epoch_millis_now());
        }
        self.dirty_flag.clear();
        Ok(Some(path))
    }

    /// Gather one section per registered participant.
    fn collect_sections(&self) -> SnapshotOrchestratorResult<Vec<SectionV1>> {
        let registry = self.stateful_registry.read().map_err(|_| {
            SnapshotOrchestratorError::Other("stateful plugin registry lock poisoned".into())
        })?;
        let mut sections = Vec::with_capacity(registry.len());
        for handle in registry.as_slice() {
            match handle.as_dyn().snapshot() {
                Ok(payload) => {
                    sections.push(SectionV1 {
                        id: payload.id.as_str().to_string(),
                        version: payload.version,
                        bytes: payload.bytes,
                    });
                }
                Err(err) => {
                    warn!("stateful plugin snapshot failed (skipped in envelope): {err}");
                }
            }
        }
        Ok(sections)
    }

    /// Read + validate the on-disk envelope. Does not apply it.
    fn read_envelope(&self) -> SnapshotOrchestratorResult<Option<CombinedSnapshotEnvelope>> {
        let Some(path) = self.path.as_ref() else {
            return Err(SnapshotOrchestratorError::Disabled);
        };
        match std::fs::read(path) {
            Ok(bytes) => {
                let envelope: CombinedSnapshotEnvelope = serde_json::from_slice(&bytes)
                    .map_err(|e| SnapshotOrchestratorError::Codec(e.to_string()))?;
                envelope.validate()?;
                Ok(Some(envelope))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(SnapshotOrchestratorError::Io(err.to_string())),
        }
    }

    /// Apply every section of an envelope to its matching participant
    /// by routing on `section.id`. Sections without a matching
    /// participant are logged + skipped.
    fn apply_envelope(
        &self,
        envelope: CombinedSnapshotEnvelope,
    ) -> SnapshotOrchestratorResult<RestoreSummary> {
        let registry = self.stateful_registry.read().map_err(|_| {
            SnapshotOrchestratorError::Other("stateful plugin registry lock poisoned".into())
        })?;
        let mut restored = 0usize;
        let mut failed = 0usize;
        for section in envelope.sections {
            let Some(participant) = registry
                .as_slice()
                .iter()
                .find(|h| h.as_dyn().id().as_str() == section.id)
            else {
                warn!(
                    "snapshot envelope has section '{id}' with no matching participant — skipped",
                    id = section.id
                );
                continue;
            };
            let id = participant.as_dyn().id();
            let payload = StatefulPluginSnapshot::new(id, section.version, section.bytes);
            match participant.as_dyn().restore_snapshot(payload) {
                Ok(()) => restored += 1,
                Err(err) => {
                    failed += 1;
                    warn!(
                        "stateful plugin restore failed for '{}': {}",
                        section.id,
                        StatefulErrorDisplay(&err)
                    );
                }
            }
        }
        Ok(RestoreSummary {
            restored_plugins: restored,
            failed_plugins: failed,
        })
    }
}

impl SnapshotOrchestrator for BmuxSnapshotOrchestrator {
    async fn restore_if_present(&self) -> SnapshotOrchestratorResult<Option<RestoreSummary>> {
        if self.path.is_none() {
            return Ok(None);
        }
        let Some(envelope) = self.read_envelope()? else {
            return Ok(None);
        };
        match self.apply_envelope(envelope) {
            Ok(summary) => {
                if let Ok(mut status) = self.status.lock() {
                    status.last_restore_epoch_ms = Some(epoch_millis_now());
                    status.last_restore_error = None;
                }
                Ok(Some(summary))
            }
            Err(err) => {
                if let Ok(mut status) = self.status.lock() {
                    status.last_restore_error = Some(err.to_string());
                }
                Err(err)
            }
        }
    }

    async fn save_now(&self) -> SnapshotOrchestratorResult<Option<PathBuf>> {
        self.save_now_blocking()
    }

    async fn dry_run(&self) -> SnapshotOrchestratorResult<DryRunReport> {
        if self.path.is_none() {
            return Err(SnapshotOrchestratorError::Disabled);
        }
        match self.read_envelope()? {
            Some(envelope) => {
                let section_count = envelope.sections.len();
                let ids = envelope
                    .sections
                    .iter()
                    .map(|s| s.id.clone())
                    .collect::<Vec<_>>()
                    .join(", ");
                Ok(DryRunReport {
                    ok: true,
                    message: format!("{section_count} sections: [{ids}]"),
                })
            }
            None => Ok(DryRunReport {
                ok: false,
                message: "no snapshot file on disk".into(),
            }),
        }
    }

    async fn restore_apply(&self) -> SnapshotOrchestratorResult<RestoreSummary> {
        if self.path.is_none() {
            return Err(SnapshotOrchestratorError::Disabled);
        }
        // Semantic of `restore_apply` is "replace": each participant's
        // `restore_snapshot` is expected to fully reset its state to
        // what the payload encodes. Stateful plugins already honor
        // this (e.g. `FollowStateWriter::restore_snapshot` overwrites
        // every field).
        let Some(envelope) = self.read_envelope()? else {
            return Ok(RestoreSummary::default());
        };
        match self.apply_envelope(envelope) {
            Ok(summary) => {
                if let Ok(mut status) = self.status.lock() {
                    status.last_restore_epoch_ms = Some(epoch_millis_now());
                    status.last_restore_error = None;
                }
                Ok(summary)
            }
            Err(err) => {
                if let Ok(mut status) = self.status.lock() {
                    status.last_restore_error = Some(err.to_string());
                }
                Err(err)
            }
        }
    }

    fn status(&self) -> SnapshotStatusReport {
        let enabled = self.path.is_some();
        let path_display = self.path.as_ref().map(|p| p.display().to_string());
        let snapshot_exists = self.path.as_ref().is_some_and(|p| p.exists());
        let (last_write, last_restore, last_err) = self
            .status
            .lock()
            .map(|s| {
                (
                    s.last_write_epoch_ms,
                    s.last_restore_epoch_ms,
                    s.last_restore_error.clone(),
                )
            })
            .unwrap_or_default();
        SnapshotStatusReport {
            enabled,
            path: path_display,
            snapshot_exists,
            last_write_epoch_ms: last_write,
            last_restore_epoch_ms: last_restore,
            last_restore_error: last_err,
        }
    }
}

/// Atomic file write: write to `.tmp`, fsync, rename over `path`,
/// fsync parent dir. Same pattern the legacy `SnapshotManager` used.
fn write_envelope_atomic(
    path: &Path,
    envelope: &CombinedSnapshotEnvelope,
) -> SnapshotOrchestratorResult<()> {
    let bytes = serde_json::to_vec_pretty(envelope)
        .map_err(|e| SnapshotOrchestratorError::Codec(e.to_string()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| SnapshotOrchestratorError::Io(e.to_string()))?;
    }
    let mut temp_path = path.to_path_buf();
    let temp_name = path.file_name().map_or_else(
        || "bmux-snapshot.tmp".to_string(),
        |name| format!("{}.tmp", name.to_string_lossy()),
    );
    temp_path.set_file_name(temp_name);

    let mut temp_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&temp_path)
        .map_err(|e| SnapshotOrchestratorError::Io(e.to_string()))?;
    temp_file
        .write_all(&bytes)
        .map_err(|e| SnapshotOrchestratorError::Io(e.to_string()))?;
    temp_file
        .sync_all()
        .map_err(|e| SnapshotOrchestratorError::Io(e.to_string()))?;
    std::fs::rename(&temp_path, path).map_err(|e| SnapshotOrchestratorError::Io(e.to_string()))?;
    if let Some(parent) = path.parent()
        && let Ok(parent_dir) = std::fs::File::open(parent)
    {
        let _ = parent_dir.sync_all();
    }
    Ok(())
}

#[allow(clippy::cast_possible_truncation)]
fn epoch_millis_now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Small wrapper that renders a `StatefulPluginError` without pulling
/// its full `Display` impl into the warn! format string (we want the
/// verbose form).
struct StatefulErrorDisplay<'a>(&'a StatefulPluginError);

impl core::fmt::Display for StatefulErrorDisplay<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::{BmuxSnapshotOrchestrator, DEFAULT_SNAPSHOT_FILENAME};
    use bmux_plugin_sdk::{
        PluginEventKind, StatefulPlugin, StatefulPluginHandle, StatefulPluginResult,
        StatefulPluginSnapshot,
    };
    use bmux_snapshot_runtime::{SnapshotDirtyFlag, SnapshotOrchestrator, StatefulPluginRegistry};
    use std::sync::{Arc, Mutex, RwLock};

    const TEST_ID: PluginEventKind = PluginEventKind::from_static("bmux.test/snap");

    struct Counter {
        value: Mutex<u32>,
    }

    impl StatefulPlugin for Counter {
        fn id(&self) -> PluginEventKind {
            TEST_ID
        }

        fn snapshot(&self) -> StatefulPluginResult<StatefulPluginSnapshot> {
            let v = *self.value.lock().unwrap();
            Ok(StatefulPluginSnapshot::new(
                TEST_ID,
                1,
                v.to_le_bytes().to_vec(),
            ))
        }

        fn restore_snapshot(&self, snapshot: StatefulPluginSnapshot) -> StatefulPluginResult<()> {
            let bytes: [u8; 4] = snapshot.bytes.try_into().unwrap();
            *self.value.lock().unwrap() = u32::from_le_bytes(bytes);
            Ok(())
        }
    }

    fn new_registry_with_counter(
        value: u32,
    ) -> (Arc<RwLock<StatefulPluginRegistry>>, Arc<Counter>) {
        let counter = Arc::new(Counter {
            value: Mutex::new(value),
        });
        let mut registry = StatefulPluginRegistry::new();
        registry.push(StatefulPluginHandle::from_arc(counter.clone()));
        (Arc::new(RwLock::new(registry)), counter)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn save_and_restore_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(DEFAULT_SNAPSHOT_FILENAME);
        let (registry, counter) = new_registry_with_counter(7);
        let dirty = Arc::new(SnapshotDirtyFlag::new());

        let orchestrator = BmuxSnapshotOrchestrator::new(
            Some(path.clone()),
            Arc::clone(&dirty),
            Arc::clone(&registry),
        );

        let saved = orchestrator.save_now().await.expect("save");
        assert_eq!(saved.as_deref(), Some(path.as_path()));
        assert!(path.exists());

        // Mutate, then restore.
        *counter.value.lock().unwrap() = 999;
        let summary = orchestrator
            .restore_if_present()
            .await
            .expect("restore")
            .unwrap();
        assert_eq!(summary.restored_plugins, 1);
        assert_eq!(summary.failed_plugins, 0);
        assert_eq!(*counter.value.lock().unwrap(), 7);
    }

    #[tokio::test]
    async fn disabled_orchestrator_is_noop() {
        let (registry, _) = new_registry_with_counter(0);
        let dirty = Arc::new(SnapshotDirtyFlag::new());
        let orchestrator = BmuxSnapshotOrchestrator::new(None, dirty, registry);
        assert!(orchestrator.save_now().await.expect("save").is_none());
        assert!(
            orchestrator
                .restore_if_present()
                .await
                .expect("restore")
                .is_none()
        );
        assert!(orchestrator.dry_run().await.is_err());
        assert!(orchestrator.restore_apply().await.is_err());
        let status = orchestrator.status();
        assert!(!status.enabled);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn save_clears_dirty_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(DEFAULT_SNAPSHOT_FILENAME);
        let (registry, _) = new_registry_with_counter(0);
        let dirty = Arc::new(SnapshotDirtyFlag::new());
        dirty.mark_dirty();
        assert!(dirty.is_dirty());

        let orchestrator = BmuxSnapshotOrchestrator::new(Some(path), Arc::clone(&dirty), registry);
        orchestrator.save_now().await.expect("save");
        assert!(!dirty.is_dirty());
    }
}
