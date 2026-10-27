//! Concrete `SnapshotOrchestrator` implementation.
//!
//! Iterates the `StatefulPluginRegistry` to build/restore a
//! `CombinedSnapshotEnvelope`, writes it to the configured path with
//! an atomic rename-on-write pattern, and maintains a status record
//! the `status()` method returns.

use std::collections::{BTreeMap, BTreeSet};
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
    writes_allowed: bool,
    restore_started: bool,
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
    managed_catalog: Option<crate::managed_catalog::ManagedCatalog>,
    status: Mutex<Status>,
    // Serializes collection and replacement, not just file IO. Once finalized,
    // no writer may observe host teardown state or reopen persistence.
    finalized: Mutex<bool>,
    debounce: Mutex<Option<(std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>)>>,
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
        let writes_allowed = path.is_none();
        Self {
            managed_catalog: None,
            path,
            dirty_flag,
            stateful_registry,
            finalized: Mutex::new(false),
            debounce: Mutex::new(None),
            status: Mutex::new(Status {
                writes_allowed,
                ..Status::default()
            }),
        }
    }

    pub(crate) fn install_debounce(
        &self,
        stop: std::sync::mpsc::Sender<()>,
        thread: std::thread::JoinHandle<()>,
    ) {
        *self.debounce.lock().expect("debounce owner lock") = Some((stop, thread));
    }

    pub(crate) fn stop_debounce(&self) -> Result<(), String> {
        let owner = self
            .debounce
            .lock()
            .map_err(|_| "debounce owner lock poisoned")?
            .take();
        if let Some((stop, thread)) = owner {
            let _ = stop.send(());
            thread
                .join()
                .map_err(|_| "snapshot debounce thread panicked")?;
        }
        Ok(())
    }

    pub(crate) fn with_managed_catalog(
        mut self,
        catalog: Option<crate::managed_catalog::ManagedCatalog>,
    ) -> Self {
        if let Some(catalog) = &catalog {
            self.path = Some(catalog.path().to_path_buf());
        }
        self.managed_catalog = catalog;
        self
    }

    /// Synchronous variant of `save_now()` for the debounce-flush
    /// background thread. Returns the written path (or `None` if
    /// persistence is disabled) / a `SnapshotOrchestratorError`.
    pub(crate) fn save_now_blocking(&self) -> SnapshotOrchestratorResult<Option<PathBuf>> {
        let finalized = self.finalized.lock().map_err(|_| {
            SnapshotOrchestratorError::Other("snapshot lifecycle lock poisoned".into())
        })?;
        if *finalized {
            return Ok(None);
        }
        self.save_locked()
    }

    // Caller holds the lifecycle lock across collection and atomic replacement.
    fn save_locked(&self) -> SnapshotOrchestratorResult<Option<PathBuf>> {
        let Some(path) = self.path.clone() else {
            return Ok(None);
        };
        if !self.writes_allowed()? {
            return Err(SnapshotOrchestratorError::Other(
                "snapshot writes are blocked until startup restore completes successfully".into(),
            ));
        }
        let sections = self.collect_sections()?;
        let envelope = CombinedSnapshotEnvelope::build(sections)?;
        if let Some(catalog) = &self.managed_catalog {
            catalog.save(&envelope)?;
        } else {
            write_envelope_atomic(&path, &envelope)?;
        }
        if let Err(error) = bmux_plugin::shutdown_progress().advance("snapshot persisted") {
            warn!(%error, "shutdown progress publication failed");
        }
        if let Ok(mut status) = self.status.lock() {
            status.last_write_epoch_ms = Some(epoch_millis_now());
        }
        self.dirty_flag.clear();
        Ok(Some(path))
    }

    pub(crate) fn writes_allowed(&self) -> SnapshotOrchestratorResult<bool> {
        self.status
            .lock()
            .map(|status| status.writes_allowed)
            .map_err(|_| SnapshotOrchestratorError::Other("snapshot status lock poisoned".into()))
    }

    fn begin_restore(&self) -> SnapshotOrchestratorResult<()> {
        let mut status = self.status.lock().map_err(|_| {
            SnapshotOrchestratorError::Other("snapshot status lock poisoned".into())
        })?;
        if status.restore_started {
            return Err(SnapshotOrchestratorError::Other(
                "snapshot restore has already been attempted for this server instance".into(),
            ));
        }
        status.restore_started = true;
        status.writes_allowed = false;
        Ok(())
    }

    fn record_restore_result(&self, summary: RestoreSummary) -> SnapshotOrchestratorResult<()> {
        let mut status = self.status.lock().map_err(|_| {
            SnapshotOrchestratorError::Other("snapshot status lock poisoned".into())
        })?;
        status.last_restore_epoch_ms = Some(epoch_millis_now());
        if summary.failed_plugins == 0 {
            status.last_restore_error = None;
            status.writes_allowed = true;
            return Ok(());
        }
        status.last_restore_error = Some(format!(
            "{} snapshot participant(s) failed to restore; writes remain blocked to preserve the source snapshot",
            summary.failed_plugins
        ));
        Err(SnapshotOrchestratorError::Other(
            status
                .last_restore_error
                .clone()
                .expect("partial restore records an error"),
        ))
    }

    fn record_restore_error(&self, error: &SnapshotOrchestratorError) {
        if let Ok(mut status) = self.status.lock() {
            status.last_restore_error = Some(error.to_string());
            status.writes_allowed = false;
        }
    }

    /// Gather one section per registered participant.
    fn collect_sections(&self) -> SnapshotOrchestratorResult<Vec<SectionV1>> {
        let registry = self.stateful_registry.read().map_err(|_| {
            SnapshotOrchestratorError::Other("stateful plugin registry lock poisoned".into())
        })?;
        let mut sections = Vec::with_capacity(registry.len());
        for handle in registry.as_slice() {
            let payload = handle.as_dyn().snapshot().map_err(|error| {
                SnapshotOrchestratorError::Participant {
                    plugin: handle.as_dyn().id().as_str().to_string(),
                    details: StatefulErrorDisplay(&error).to_string(),
                }
            })?;
            if let Err(error) =
                bmux_plugin::shutdown_progress().advance("snapshot participant collected")
            {
                warn!(%error, "shutdown progress publication failed");
            }
            sections.push(SectionV1 {
                id: payload.id.as_str().to_string(),
                version: payload.version,
                bytes: payload.bytes,
            });
        }
        Ok(sections)
    }

    /// Read + validate the on-disk envelope. Does not apply it.
    fn read_envelope(&self) -> SnapshotOrchestratorResult<Option<CombinedSnapshotEnvelope>> {
        let Some(path) = self.path.as_ref() else {
            return Err(SnapshotOrchestratorError::Disabled);
        };
        if let Some(catalog) = &self.managed_catalog {
            return catalog.load();
        }
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
        let section_count = envelope.sections.len();
        let mut sections = envelope
            .sections
            .into_iter()
            .map(|section| (section.id.clone(), section))
            .collect::<BTreeMap<_, _>>();
        let available_section_ids = sections.keys().cloned().collect::<BTreeSet<_>>();
        let registered = registry
            .as_slice()
            .iter()
            .map(|handle| (handle.as_dyn().id().as_str().to_string(), handle))
            .collect::<BTreeMap<_, _>>();
        if sections.len() != section_count {
            return Err(SnapshotOrchestratorError::Other(
                "snapshot envelope contains duplicate section IDs".into(),
            ));
        }
        if registered.len() != registry.len() {
            return Err(SnapshotOrchestratorError::Other(
                "stateful plugin registry contains duplicate participant IDs".into(),
            ));
        }
        let restore_order = restore_order(&registered)?;
        let mut restored_ids = BTreeSet::new();
        let mut failed_ids = BTreeSet::new();
        let mut restored = 0usize;
        let mut failed = 0usize;
        for id in restore_order {
            // These participants already own shared durable arrangements and
            // catalogs. A historical per-target projection is not authority to
            // replace newer installation-wide state. Keep their live durable
            // authority, while still satisfying dependencies of runtime layout.
            if self.managed_catalog.is_some() && is_shared_durable_section(&id) {
                sections.remove(&id);
                restored_ids.insert(id);
                continue;
            }
            let participant = registered[&id];
            let dependencies = participant
                .as_dyn()
                .restore_dependencies()
                .into_iter()
                .map(|dependency| dependency.as_str().to_string())
                .collect::<Vec<_>>();
            let unavailable = dependencies.iter().find(|dependency| {
                !registered.contains_key(*dependency)
                    || !available_section_ids.contains(*dependency)
                    || failed_ids.contains(*dependency)
                    || !restored_ids.contains(*dependency)
            });
            if let Some(dependency) = unavailable {
                failed += 1;
                failed_ids.insert(id.clone());
                warn!(participant = %id, %dependency, "snapshot participant dependency unavailable; restore skipped");
                continue;
            }
            let Some(section) = sections.remove(&id) else {
                continue;
            };
            let payload = StatefulPluginSnapshot::new(
                participant.as_dyn().id(),
                section.version,
                section.bytes,
            );
            match participant.as_dyn().restore_snapshot(payload) {
                Ok(()) => {
                    restored += 1;
                    restored_ids.insert(id);
                }
                Err(err) => {
                    failed += 1;
                    failed_ids.insert(id.clone());
                    warn!(
                        "stateful plugin restore failed for '{}': {}",
                        id,
                        StatefulErrorDisplay(&err)
                    );
                }
            }
        }
        for id in sections.keys() {
            warn!("snapshot envelope has section '{id}' with no matching participant — skipped");
        }
        Ok(RestoreSummary {
            restored_plugins: restored,
            failed_plugins: failed,
        })
    }
}

fn restore_order(
    registered: &BTreeMap<String, &bmux_plugin_sdk::StatefulPluginHandle>,
) -> SnapshotOrchestratorResult<Vec<String>> {
    fn visit(
        id: &str,
        registered: &BTreeMap<String, &bmux_plugin_sdk::StatefulPluginHandle>,
        visiting: &mut BTreeSet<String>,
        visited: &mut BTreeSet<String>,
        ordered: &mut Vec<String>,
    ) -> SnapshotOrchestratorResult<()> {
        if visited.contains(id) {
            return Ok(());
        }
        if !visiting.insert(id.to_string()) {
            return Err(SnapshotOrchestratorError::Other(format!(
                "stateful restore dependency cycle includes '{id}'"
            )));
        }
        if let Some(participant) = registered.get(id) {
            let mut dependencies = participant
                .as_dyn()
                .restore_dependencies()
                .into_iter()
                .map(|dependency| dependency.as_str().to_string())
                .collect::<Vec<_>>();
            dependencies.sort();
            for dependency in dependencies {
                if registered.contains_key(&dependency) {
                    visit(&dependency, registered, visiting, visited, ordered)?;
                }
            }
        }
        visiting.remove(id);
        visited.insert(id.to_string());
        ordered.push(id.to_string());
        Ok(())
    }

    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    let mut ordered = Vec::with_capacity(registered.len());
    for id in registered.keys() {
        visit(id, registered, &mut visiting, &mut visited, &mut ordered)?;
    }
    Ok(ordered)
}

impl BmuxSnapshotOrchestrator {
    fn restore_if_present_blocking(&self) -> SnapshotOrchestratorResult<Option<RestoreSummary>> {
        if self.path.is_none() {
            return Ok(None);
        }
        self.begin_restore()?;
        let envelope = match self.read_envelope() {
            Ok(Some(envelope)) => envelope,
            Ok(None) => {
                self.status
                    .lock()
                    .map_err(|_| {
                        SnapshotOrchestratorError::Other("snapshot status lock poisoned".into())
                    })?
                    .writes_allowed = true;
                return Ok(None);
            }
            Err(error) => {
                self.record_restore_error(&error);
                return Err(error);
            }
        };
        let summary = match self.apply_envelope(envelope) {
            Ok(summary) => summary,
            Err(error) => {
                self.record_restore_error(&error);
                return Err(error);
            }
        };
        self.record_restore_result(summary)?;
        Ok(Some(summary))
    }
    fn dry_run_blocking(&self) -> SnapshotOrchestratorResult<DryRunReport> {
        if self.path.is_none() {
            return Err(SnapshotOrchestratorError::Disabled);
        }
        match self.read_envelope()? {
            Some(envelope) => {
                let registry = self.stateful_registry.read().map_err(|_| {
                    SnapshotOrchestratorError::Other(
                        "stateful plugin registry lock poisoned".into(),
                    )
                })?;
                let mut validated = 0usize;
                for section in &envelope.sections {
                    let Some(participant) = registry
                        .as_slice()
                        .iter()
                        .find(|handle| handle.as_dyn().id().as_str() == section.id)
                    else {
                        continue;
                    };
                    let payload = StatefulPluginSnapshot::new(
                        participant.as_dyn().id(),
                        section.version,
                        section.bytes.clone(),
                    );
                    participant
                        .as_dyn()
                        .validate_snapshot(&payload)
                        .map_err(|error| SnapshotOrchestratorError::Participant {
                            plugin: section.id.clone(),
                            details: StatefulErrorDisplay(&error).to_string(),
                        })?;
                    validated += 1;
                }
                let section_count = envelope.sections.len();
                let ids = envelope
                    .sections
                    .iter()
                    .map(|s| s.id.clone())
                    .collect::<Vec<_>>()
                    .join(", ");
                Ok(DryRunReport {
                    ok: true,
                    message: format!(
                        "{section_count} sections, {validated} registered payloads validated: [{ids}]{}",
                        if let Some(catalog) = &self.managed_catalog {
                            format!("; {}", catalog.discovery()?)
                        } else {
                            String::new()
                        },
                    ),
                })
            }
            None => Ok(DryRunReport {
                ok: false,
                message: if let Some(catalog) = &self.managed_catalog {
                    catalog.discovery()?
                } else {
                    "no snapshot file on disk".into()
                },
            }),
        }
    }
    fn restore_apply_blocking(&self) -> SnapshotOrchestratorResult<RestoreSummary> {
        if self.path.is_none() {
            return Err(SnapshotOrchestratorError::Disabled);
        }
        // Semantic of `restore_apply` is "replace": each participant's
        // `restore_snapshot` is expected to fully reset its state to
        // what the payload encodes. Stateful plugins already honor
        // this (e.g. `FollowStateWriter::restore_snapshot` overwrites
        // every field).
        let envelope = match self.read_envelope() {
            Ok(Some(envelope)) => envelope,
            Ok(None) => return Ok(RestoreSummary::default()),
            Err(error) => {
                self.record_restore_error(&error);
                return Err(error);
            }
        };
        let summary = match self.apply_envelope(envelope) {
            Ok(summary) => summary,
            Err(error) => {
                self.record_restore_error(&error);
                return Err(error);
            }
        };
        self.record_restore_result(summary)?;
        Ok(summary)
    }
}

impl SnapshotOrchestrator for BmuxSnapshotOrchestrator {
    fn finalize(&self) -> SnapshotOrchestratorResult<Option<PathBuf>> {
        let mut finalized = self.finalized.lock().map_err(|_| {
            SnapshotOrchestratorError::Other("snapshot lifecycle lock poisoned".into())
        })?;
        if *finalized {
            return Ok(None);
        }
        // Seal even on failure: teardown must never replace the last good file.
        *finalized = true;
        {
            let registry = self.stateful_registry.read().map_err(|_| {
                SnapshotOrchestratorError::Other("stateful registry lock poisoned".into())
            })?;
            for handle in registry.as_slice() {
                handle.as_dyn().prepare_final_snapshot().map_err(|error| {
                    SnapshotOrchestratorError::Participant {
                        plugin: handle.as_dyn().id().as_str().to_string(),
                        details: StatefulErrorDisplay(&error).to_string(),
                    }
                })?;
                if let Err(error) =
                    bmux_plugin::shutdown_progress().advance("snapshot participant prepared")
                {
                    warn!(%error, "shutdown progress publication failed");
                }
            }
        }
        self.save_locked()
    }
    fn restore_if_present(
        &self,
    ) -> impl std::future::Future<Output = SnapshotOrchestratorResult<Option<RestoreSummary>>> {
        std::future::poll_fn(move |_| std::task::Poll::Ready(self.restore_if_present_blocking()))
    }

    fn save_now(
        &self,
    ) -> impl std::future::Future<Output = SnapshotOrchestratorResult<Option<PathBuf>>> {
        std::future::poll_fn(move |_| std::task::Poll::Ready(self.save_now_blocking()))
    }

    fn dry_run(
        &self,
    ) -> impl std::future::Future<Output = SnapshotOrchestratorResult<DryRunReport>> {
        std::future::poll_fn(move |_| std::task::Poll::Ready(self.dry_run_blocking()))
    }

    fn restore_apply(
        &self,
    ) -> impl std::future::Future<Output = SnapshotOrchestratorResult<RestoreSummary>> {
        std::future::poll_fn(move |_| std::task::Poll::Ready(self.restore_apply_blocking()))
    }

    fn status(&self) -> SnapshotStatusReport {
        let enabled = self.path.is_some();
        let path_display = self.path.as_ref().map(|p| p.display().to_string());
        let managed_observation = self
            .managed_catalog
            .as_ref()
            .map(crate::managed_catalog::ManagedCatalog::load);
        let snapshot_exists = if let Some(observation) = &managed_observation {
            observation.as_ref().is_ok_and(Option::is_some)
        } else {
            self.path.as_ref().is_some_and(|path| path.exists())
        };
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
            last_restore_error: managed_observation
                .and_then(Result::err)
                .map(|error| format!("snapshot catalog unavailable: {error}"))
                .or(last_err),
        }
    }
}

// Shared durable authorities are read by their owning plugins, not rolled back
// from an older runtime projection during managed startup.
fn is_shared_durable_section(id: &str) -> bool {
    matches!(id, "bmux.tabs/arrangements" | "bmux.workspaces/catalog")
}

fn write_envelope_atomic(
    path: &Path,
    envelope: &CombinedSnapshotEnvelope,
) -> SnapshotOrchestratorResult<()> {
    let bytes = serde_json::to_vec_pretty(envelope)
        .map_err(|error| SnapshotOrchestratorError::Codec(error.to_string()))?;
    write_bytes_atomic(path, &bytes)
}

pub(crate) fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> SnapshotOrchestratorResult<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_WRITE: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .ok_or_else(|| SnapshotOrchestratorError::Other("snapshot path has no parent".into()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| SnapshotOrchestratorError::Io(error.to_string()))?;
    let sequence = NEXT_WRITE.fetch_add(1, Ordering::Relaxed);
    let temp_path = path.with_extension(format!("pending-{}-{sequence}", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp_path)
        .map_err(|error| SnapshotOrchestratorError::Io(error.to_string()))?;
    // Failed publication leaves explicit recovery evidence; never truncate or
    // remove another writer's temporary file.
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| SnapshotOrchestratorError::Io(error.to_string()))?;
    std::fs::rename(&temp_path, path)
        .map_err(|error| SnapshotOrchestratorError::Io(error.to_string()))?;
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| SnapshotOrchestratorError::Io(error.to_string()))
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
        PluginEventKind, StatefulPlugin, StatefulPluginError, StatefulPluginHandle,
        StatefulPluginResult, StatefulPluginSnapshot,
    };
    use bmux_snapshot_runtime::{SnapshotDirtyFlag, SnapshotOrchestrator, StatefulPluginRegistry};
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex, RwLock};

    struct OrderedParticipant {
        id: PluginEventKind,
        dependencies: Vec<PluginEventKind>,
        restored: Arc<Mutex<Vec<String>>>,
    }

    impl StatefulPlugin for OrderedParticipant {
        fn id(&self) -> PluginEventKind {
            self.id.clone()
        }

        fn restore_dependencies(&self) -> Vec<PluginEventKind> {
            self.dependencies.clone()
        }

        fn snapshot(&self) -> StatefulPluginResult<StatefulPluginSnapshot> {
            Ok(StatefulPluginSnapshot::new(self.id.clone(), 1, Vec::new()))
        }

        fn restore_snapshot(&self, _snapshot: StatefulPluginSnapshot) -> StatefulPluginResult<()> {
            self.restored.lock().unwrap().push(self.id.to_string());
            Ok(())
        }
    }

    #[test]
    fn managed_restore_keeps_shared_durable_layout_authority() {
        let temp = tempfile::tempdir().unwrap();
        let restored = Arc::new(Mutex::new(Vec::new()));
        let workspace = PluginEventKind::from_static("bmux.workspaces/catalog");
        let tabs = PluginEventKind::from_static("bmux.tabs/arrangements");
        let runtime = PluginEventKind::from_static("test/runtime-layout");
        let mut registry = StatefulPluginRegistry::new();
        for (id, dependencies) in [
            (workspace.clone(), Vec::new()),
            (tabs.clone(), Vec::new()),
            (runtime.clone(), vec![workspace.clone()]),
        ] {
            registry.push(StatefulPluginHandle::new(OrderedParticipant {
                id,
                dependencies,
                restored: Arc::clone(&restored),
            }));
        }
        let catalog = crate::managed_catalog::ManagedCatalog::new(
            &temp.path().join("legacy.json"),
            "a".repeat(64),
            "{}",
        )
        .unwrap();
        let orchestrator = BmuxSnapshotOrchestrator::new(
            None,
            Arc::new(SnapshotDirtyFlag::new()),
            Arc::new(RwLock::new(registry)),
        )
        .with_managed_catalog(Some(catalog));
        let sections = [workspace, tabs, runtime.clone()]
            .into_iter()
            .map(|id| bmux_snapshot_plugin_api::envelope::SectionV1 {
                id: id.to_string(),
                version: 1,
                bytes: Vec::new(),
            })
            .collect();
        let envelope =
            bmux_snapshot_plugin_api::envelope::CombinedSnapshotEnvelope::build(sections).unwrap();
        let summary = orchestrator.apply_envelope(envelope).unwrap();
        assert_eq!(summary.restored_plugins, 1);
        assert_eq!(summary.failed_plugins, 0);
        assert_eq!(*restored.lock().unwrap(), vec![runtime.to_string()]);
    }

    #[test]
    fn shuffled_envelope_restores_dependencies_first() {
        let restored = Arc::new(Mutex::new(Vec::new()));
        let session_id = PluginEventKind::from_static("test/session-state");
        let pane_id = PluginEventKind::from_static("test/pane-state");
        let mut registry = StatefulPluginRegistry::new();
        registry.push(StatefulPluginHandle::new(OrderedParticipant {
            id: pane_id.clone(),
            dependencies: vec![session_id.clone()],
            restored: Arc::clone(&restored),
        }));
        registry.push(StatefulPluginHandle::new(OrderedParticipant {
            id: session_id.clone(),
            dependencies: Vec::new(),
            restored: Arc::clone(&restored),
        }));
        let orchestrator = BmuxSnapshotOrchestrator::new(
            None,
            Arc::new(SnapshotDirtyFlag::new()),
            Arc::new(RwLock::new(registry)),
        );
        let envelope = bmux_snapshot_plugin_api::envelope::CombinedSnapshotEnvelope::build(vec![
            bmux_snapshot_plugin_api::envelope::SectionV1 {
                id: pane_id.to_string(),
                version: 1,
                bytes: Vec::new(),
            },
            bmux_snapshot_plugin_api::envelope::SectionV1 {
                id: session_id.to_string(),
                version: 1,
                bytes: Vec::new(),
            },
        ])
        .unwrap();
        let summary = orchestrator.apply_envelope(envelope).unwrap();
        assert_eq!(summary.restored_plugins, 2);
        assert_eq!(summary.failed_plugins, 0);
        assert_eq!(
            *restored.lock().unwrap(),
            vec![session_id.to_string(), pane_id.to_string()]
        );
    }

    #[test]
    fn restore_order_honors_dependencies_over_registration_order() {
        let restored = Arc::new(Mutex::new(Vec::new()));
        let session_id = PluginEventKind::from_static("test/sessions");
        let pane_id = PluginEventKind::from_static("test/panes");
        let session = StatefulPluginHandle::new(OrderedParticipant {
            id: session_id.clone(),
            dependencies: Vec::new(),
            restored: Arc::clone(&restored),
        });
        let pane = StatefulPluginHandle::new(OrderedParticipant {
            id: pane_id.clone(),
            dependencies: vec![session_id],
            restored,
        });
        let registered = BTreeMap::from([
            (pane_id.to_string(), &pane),
            (session.as_dyn().id().to_string(), &session),
        ]);
        assert_eq!(
            super::restore_order(&registered).unwrap(),
            vec![session.as_dyn().id().to_string(), pane_id.to_string()]
        );
    }

    #[test]
    fn restore_order_rejects_dependency_cycles() {
        let restored = Arc::new(Mutex::new(Vec::new()));
        let first_id = PluginEventKind::from_static("test/first");
        let second_id = PluginEventKind::from_static("test/second");
        let first = StatefulPluginHandle::new(OrderedParticipant {
            id: first_id.clone(),
            dependencies: vec![second_id.clone()],
            restored: Arc::clone(&restored),
        });
        let second = StatefulPluginHandle::new(OrderedParticipant {
            id: second_id.clone(),
            dependencies: vec![first_id.clone()],
            restored,
        });
        let registered = BTreeMap::from([
            (first_id.to_string(), &first),
            (second_id.to_string(), &second),
        ]);
        assert!(super::restore_order(&registered).is_err());
    }

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

        fn validate_snapshot(&self, snapshot: &StatefulPluginSnapshot) -> StatefulPluginResult<()> {
            decode_counter_snapshot(snapshot).map(|_| ())
        }

        fn restore_snapshot(&self, snapshot: StatefulPluginSnapshot) -> StatefulPluginResult<()> {
            *self.value.lock().unwrap() = decode_counter_snapshot(&snapshot)?;
            Ok(())
        }
    }

    fn decode_counter_snapshot(snapshot: &StatefulPluginSnapshot) -> StatefulPluginResult<u32> {
        let bytes: [u8; 4] = snapshot.bytes.as_slice().try_into().map_err(|_| {
            StatefulPluginError::RestoreFailed {
                plugin: TEST_ID.as_str().to_string(),
                details: "expected 4 bytes".into(),
            }
        })?;
        Ok(u32::from_le_bytes(bytes))
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
        assert!(orchestrator.save_now().await.is_err());
        assert!(
            orchestrator
                .restore_if_present()
                .await
                .expect("initial restore")
                .is_none()
        );

        let saved = orchestrator.save_now().await.expect("save");
        assert_eq!(saved.as_deref(), Some(path.as_path()));
        assert!(path.exists());

        // Mutate, then simulate a new server instance restoring the saved file.
        *counter.value.lock().unwrap() = 999;
        let restoring_orchestrator =
            BmuxSnapshotOrchestrator::new(Some(path), Arc::clone(&dirty), Arc::clone(&registry));
        let summary = restoring_orchestrator
            .restore_if_present()
            .await
            .expect("restore")
            .unwrap();
        assert_eq!(summary.restored_plugins, 1);
        assert_eq!(summary.failed_plugins, 0);
        assert_eq!(*counter.value.lock().unwrap(), 7);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn final_snapshot_survives_dirty_teardown_and_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(DEFAULT_SNAPSHOT_FILENAME);
        let (registry, counter) = new_registry_with_counter(33);
        let dirty = Arc::new(SnapshotDirtyFlag::new());
        let orchestrator =
            BmuxSnapshotOrchestrator::new(Some(path.clone()), Arc::clone(&dirty), registry);
        orchestrator.restore_if_present().await.unwrap();
        orchestrator.finalize().unwrap();
        let saved = std::fs::read(&path).unwrap();

        // Teardown mutations must not reach disk, whether a debounced save,
        // explicit save, or duplicate finalization follows them.
        *counter.value.lock().unwrap() = 0;
        dirty.mark_dirty();
        assert!(orchestrator.save_now_blocking().unwrap().is_none());
        assert!(orchestrator.save_now().await.unwrap().is_none());
        assert!(orchestrator.finalize().unwrap().is_none());
        assert_eq!(std::fs::read(&path).unwrap(), saved);

        let (registry, restored) = new_registry_with_counter(0);
        let restarted = BmuxSnapshotOrchestrator::new(Some(path), dirty, registry);
        restarted.restore_if_present().await.unwrap();
        assert_eq!(*restored.value.lock().unwrap(), 33);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn finalization_waits_for_in_flight_collection() {
        struct BlockingParticipant {
            entered: std::sync::mpsc::Sender<()>,
            release: Mutex<std::sync::mpsc::Receiver<()>>,
            first: std::sync::atomic::AtomicBool,
        }
        impl StatefulPlugin for BlockingParticipant {
            fn id(&self) -> PluginEventKind {
                TEST_ID
            }
            fn snapshot(&self) -> StatefulPluginResult<StatefulPluginSnapshot> {
                if self.first.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    self.entered.send(()).unwrap();
                    self.release.lock().unwrap().recv().unwrap();
                }
                Ok(StatefulPluginSnapshot::new(TEST_ID, 1, vec![33]))
            }
            fn restore_snapshot(&self, _: StatefulPluginSnapshot) -> StatefulPluginResult<()> {
                Ok(())
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let mut registry = StatefulPluginRegistry::new();
        registry.push(StatefulPluginHandle::new(BlockingParticipant {
            entered: entered_tx,
            release: Mutex::new(release_rx),
            first: std::sync::atomic::AtomicBool::new(true),
        }));
        let orchestrator = Arc::new(BmuxSnapshotOrchestrator::new(
            Some(tmp.path().join(DEFAULT_SNAPSHOT_FILENAME)),
            Arc::new(SnapshotDirtyFlag::new()),
            Arc::new(RwLock::new(registry)),
        ));
        orchestrator.restore_if_present().await.unwrap();
        let saving = Arc::clone(&orchestrator);
        let save = std::thread::spawn(move || saving.save_now_blocking());
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        // Collection holds the same gate that finalization must acquire.
        assert!(matches!(
            orchestrator.finalized.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        ));
        let finalizing = Arc::clone(&orchestrator);
        let finalize = std::thread::spawn(move || finalizing.finalize());
        release_tx.send(()).unwrap();
        save.join().unwrap().unwrap();
        finalize.join().unwrap().unwrap();
        assert!(orchestrator.save_now_blocking().unwrap().is_none());
    }

    #[tokio::test]
    async fn failed_final_snapshot_still_seals_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(DEFAULT_SNAPSHOT_FILENAME);
        let (registry, _) = new_registry_with_counter(33);
        let orchestrator = BmuxSnapshotOrchestrator::new(
            Some(path.clone()),
            Arc::new(SnapshotDirtyFlag::new()),
            registry,
        );
        orchestrator.restore_if_present().await.unwrap();
        // A directory at the destination makes atomic replacement fail.
        std::fs::create_dir(&path).unwrap();
        assert!(orchestrator.finalize().is_err());
        assert!(orchestrator.save_now_blocking().unwrap().is_none());
        assert!(orchestrator.finalize().unwrap().is_none());
    }

    #[tokio::test]
    async fn finalization_quiesces_every_participant_before_capture() {
        struct Participant {
            id: &'static str,
            frozen: Arc<std::sync::atomic::AtomicUsize>,
            fail: bool,
        }
        impl StatefulPlugin for Participant {
            fn id(&self) -> PluginEventKind {
                PluginEventKind::from_static(self.id)
            }
            fn prepare_final_snapshot(&self) -> StatefulPluginResult<()> {
                self.frozen
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if self.fail {
                    return Err(StatefulPluginError::SnapshotFailed {
                        plugin: self.id.into(),
                        details: "injected freeze failure".into(),
                    });
                }
                Ok(())
            }
            fn snapshot(&self) -> StatefulPluginResult<StatefulPluginSnapshot> {
                assert_eq!(self.frozen.load(std::sync::atomic::Ordering::SeqCst), 2);
                Ok(StatefulPluginSnapshot::new(self.id(), 1, vec![]))
            }
        }
        for fail in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join(DEFAULT_SNAPSHOT_FILENAME);
            let frozen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let mut registry = StatefulPluginRegistry::new();
            for id in ["test.first", "test.second"] {
                registry.push(StatefulPluginHandle::new(Participant {
                    id,
                    frozen: Arc::clone(&frozen),
                    fail: fail && id == "test.second",
                }));
            }
            let orchestrator = BmuxSnapshotOrchestrator::new(
                Some(path.clone()),
                Arc::new(SnapshotDirtyFlag::new()),
                Arc::new(RwLock::new(registry)),
            );
            orchestrator.restore_if_present().await.unwrap();
            std::fs::write(&path, b"last good snapshot").unwrap();
            assert_eq!(orchestrator.finalize().is_err(), fail);
            if fail {
                assert_eq!(std::fs::read(&path).unwrap(), b"last good snapshot");
            }
            assert!(orchestrator.finalize().unwrap().is_none());
            assert_eq!(frozen.load(std::sync::atomic::Ordering::SeqCst), 2);
        }
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
        assert!(
            orchestrator
                .restore_if_present()
                .await
                .expect("initial restore")
                .is_none()
        );
        orchestrator.save_now().await.expect("save");
        assert!(!dirty.is_dirty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_participant_snapshot_does_not_replace_existing_file() {
        struct FailingSnapshotter;

        impl StatefulPlugin for FailingSnapshotter {
            fn id(&self) -> PluginEventKind {
                PluginEventKind::from_static("bmux.test/failing-snapshot")
            }

            fn snapshot(&self) -> StatefulPluginResult<StatefulPluginSnapshot> {
                Err(StatefulPluginError::SnapshotFailed {
                    plugin: self.id().to_string(),
                    details: "simulated snapshot failure".into(),
                })
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(DEFAULT_SNAPSHOT_FILENAME);
        let original = b"complete previous snapshot";
        std::fs::write(&path, original).unwrap();
        let mut registry = StatefulPluginRegistry::new();
        registry.push(StatefulPluginHandle::new(FailingSnapshotter));
        let orchestrator = BmuxSnapshotOrchestrator::new(
            Some(path.clone()),
            Arc::new(SnapshotDirtyFlag::new()),
            Arc::new(RwLock::new(registry)),
        );
        orchestrator.status.lock().unwrap().writes_allowed = true;

        assert!(orchestrator.save_now().await.is_err());
        assert_eq!(std::fs::read(path).unwrap(), original);
    }

    /// The orchestrator must tolerate snapshot envelopes that
    /// reference participant ids no plugin has registered (e.g. an
    /// older bmux version wrote a section whose owning plugin has
    /// since been removed, or a user disabled one of the bundled
    /// plugins). A malformed registered section still allows independent
    /// participants to restore, but the overall restore fails and snapshot
    /// writes remain blocked so degraded state cannot replace the source.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn partial_restore_blocks_snapshot_replacement() {
        use bmux_snapshot_plugin_api::envelope::{CombinedSnapshotEnvelope, SectionV1};
        use std::io::Write;

        // A participant that always fails to restore, used to
        // exercise the orchestrator's "malformed payload" path
        // without panicking.
        const FAILING_ID: PluginEventKind = PluginEventKind::from_static("bmux.test/failing");
        struct FailingRestorer;
        impl StatefulPlugin for FailingRestorer {
            fn id(&self) -> PluginEventKind {
                FAILING_ID
            }
            fn snapshot(&self) -> StatefulPluginResult<StatefulPluginSnapshot> {
                Ok(StatefulPluginSnapshot::new(FAILING_ID, 1, vec![]))
            }
            fn validate_snapshot(
                &self,
                _snapshot: &StatefulPluginSnapshot,
            ) -> StatefulPluginResult<()> {
                Err(bmux_plugin_sdk::StatefulPluginError::RestoreFailed {
                    plugin: FAILING_ID.as_str().to_string(),
                    details: "simulated malformed payload".into(),
                })
            }
            fn restore_snapshot(
                &self,
                _snapshot: StatefulPluginSnapshot,
            ) -> StatefulPluginResult<()> {
                Err(bmux_plugin_sdk::StatefulPluginError::RestoreFailed {
                    plugin: FAILING_ID.as_str().to_string(),
                    details: "simulated malformed payload".into(),
                })
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(DEFAULT_SNAPSHOT_FILENAME);
        let (registry, counter) = new_registry_with_counter(5);
        let dirty = Arc::new(SnapshotDirtyFlag::new());

        // Register FailingRestorer into the shared registry alongside
        // the existing Counter participant.
        registry
            .write()
            .unwrap()
            .push(StatefulPluginHandle::new(FailingRestorer));

        // Construct an envelope by hand with three sections:
        //   1. TEST_ID (known id, valid payload: u32::7 little-endian).
        //   2. "bmux.missing/ghost" (no matching participant registered).
        //   3. FAILING_ID (matching participant but its
        //      `restore_snapshot` returns `Err`).
        let envelope = CombinedSnapshotEnvelope::build(vec![
            SectionV1 {
                id: TEST_ID.as_str().to_string(),
                version: 1,
                bytes: 7u32.to_le_bytes().to_vec(),
            },
            SectionV1 {
                id: "bmux.missing/ghost".to_string(),
                version: 42,
                bytes: b"arbitrary-bytes".to_vec(),
            },
            SectionV1 {
                id: FAILING_ID.as_str().to_string(),
                version: 1,
                bytes: b"anything".to_vec(),
            },
        ])
        .expect("envelope builds");

        // Write the envelope to disk directly (bypass the
        // orchestrator's atomic save so we fully control the file
        // contents).
        let bytes = serde_json::to_vec_pretty(&envelope).expect("encode");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(&bytes).unwrap();
        file.sync_all().unwrap();

        let original_bytes = std::fs::read(&path).unwrap();
        let orchestrator = BmuxSnapshotOrchestrator::new(
            Some(path.clone()),
            Arc::clone(&dirty),
            Arc::clone(&registry),
        );

        let error = orchestrator
            .restore_if_present()
            .await
            .expect_err("partial restore must fail");
        assert!(
            error
                .to_string()
                .contains("1 snapshot participant(s) failed to restore")
        );

        // Independent participants still restore before the failure is
        // reported, but no subsequent save may replace the source snapshot.
        assert_eq!(*counter.value.lock().unwrap(), 7);
        assert!(orchestrator.dry_run().await.is_err());
        assert!(orchestrator.save_now().await.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original_bytes);
        let status = orchestrator.status();
        assert!(status.last_restore_error.is_some());
    }
}
