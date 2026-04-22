//! Neutral primitive crate for snapshot orchestration.
//!
//! Hosts the shared registry + dirty flag + orchestrator trait that
//! the `bmux.snapshot` plugin, the server, and stateful state plugins
//! all depend on. Keeps snapshot-domain primitives out of both
//! `bmux_plugin_sdk` (which must stay domain-agnostic) and
//! `packages/server` (which must not own snapshot orchestration).

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use bmux_plugin_sdk::StatefulPluginHandle;

// ── Stateful-plugin registry ─────────────────────────────────────────

/// Append-only registry of `StatefulPluginHandle`s registered by
/// participating plugins.
///
/// Slot-keyed by `TypeId` inside
/// [`bmux_plugin::PluginStateRegistry`]: a single
/// `Arc<RwLock<StatefulPluginRegistry>>` lives in the registry, and
/// each participating plugin (clients, contexts, sessions) plus the
/// server (for its pane-runtime slice) `push`es its handle into the
/// inner vec during `activate` / early `run()`.
///
/// The snapshot plugin iterates the vec in registration order when
/// building a combined snapshot and when dispatching restore.
#[derive(Default)]
pub struct StatefulPluginRegistry {
    entries: Vec<StatefulPluginHandle>,
}

impl core::fmt::Debug for StatefulPluginRegistry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StatefulPluginRegistry")
            .field("entries", &self.entries.len())
            .finish()
    }
}

impl StatefulPluginRegistry {
    /// Create an empty registry.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Append a handle. Order matters: the snapshot plugin iterates in
    /// push order when building the combined envelope.
    pub fn push(&mut self, handle: StatefulPluginHandle) {
        self.entries.push(handle);
    }

    /// Borrow the registered handles as a slice.
    #[must_use]
    pub fn as_slice(&self) -> &[StatefulPluginHandle] {
        &self.entries
    }

    /// Returns the number of registered participants.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no participants are registered yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Idempotent helper: return the shared `Arc<RwLock<StatefulPluginRegistry>>`
/// registered in the provided plugin state registry, creating and
/// registering a new one if absent.
///
/// Safe to call from every plugin's `activate` regardless of order:
/// the first caller installs an empty registry; subsequent callers
/// clone the same handle and push their own `StatefulPluginHandle` on
/// top.
///
/// `R` is typically `&bmux_plugin::PluginStateRegistry`; we take it
/// generically so this primitive crate does not have to depend on
/// `bmux_plugin` (which would create a dependency direction that runs
/// counter to the one-way rule — `bmux_plugin` already depends on
/// plugin-facing primitives).
///
/// Callers pass a pair of closures for registry access:
///
/// - `get`: look up `Arc<RwLock<StatefulPluginRegistry>>` by type.
/// - `register`: install a fresh handle when none exists.
pub fn get_or_init_stateful_registry<G, R>(
    get: G,
    register: R,
) -> std::sync::Arc<std::sync::RwLock<StatefulPluginRegistry>>
where
    G: FnOnce() -> Option<std::sync::Arc<std::sync::RwLock<StatefulPluginRegistry>>>,
    R: FnOnce(&std::sync::Arc<std::sync::RwLock<StatefulPluginRegistry>>),
{
    if let Some(existing) = get() {
        return existing;
    }
    let fresh = std::sync::Arc::new(std::sync::RwLock::new(StatefulPluginRegistry::new()));
    register(&fresh);
    fresh
}

/// Idempotent helper: return the shared
/// `Arc<RwLock<SnapshotDirtyFlagHandle>>` registered in the plugin
/// state registry, creating and registering a new one if absent.
///
/// Like [`get_or_init_stateful_registry`], callers pass closures for
/// the actual get/register so this primitive crate stays
/// `bmux_plugin`-independent.
pub fn get_or_init_stateful_dirty_flag<G, R>(
    get: G,
    register: R,
) -> std::sync::Arc<std::sync::RwLock<SnapshotDirtyFlagHandle>>
where
    G: FnOnce() -> Option<std::sync::Arc<std::sync::RwLock<SnapshotDirtyFlagHandle>>>,
    R: FnOnce(&std::sync::Arc<std::sync::RwLock<SnapshotDirtyFlagHandle>>),
{
    if let Some(existing) = get() {
        return existing;
    }
    let fresh = std::sync::Arc::new(std::sync::RwLock::new(SnapshotDirtyFlagHandle::new()));
    register(&fresh);
    fresh
}

// ── Dirty flag ──────────────────────────────────────────────────────

/// Atomic dirty-mark + last-marked timestamp.
///
/// Server flips the flag whenever state that should be persisted
/// changes (session create/remove, context select, follow change,
/// etc.). The snapshot plugin's background task polls
/// [`take_dirty_at`](SnapshotDirtyFlag::take_dirty_at) and runs a
/// debounced save when the flag has been set long enough ago.
#[derive(Debug, Default)]
pub struct SnapshotDirtyFlag {
    dirty: AtomicBool,
    last_marked_epoch_ms: AtomicU64,
}

impl SnapshotDirtyFlag {
    /// Create a fresh flag in the clean state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            dirty: AtomicBool::new(false),
            last_marked_epoch_ms: AtomicU64::new(0),
        }
    }

    /// Mark state as dirty. Records the current epoch-ms timestamp
    /// for the debounce window.
    pub fn mark_dirty(&self) {
        let now = epoch_millis_now();
        self.last_marked_epoch_ms.store(now, Ordering::SeqCst);
        self.dirty.store(true, Ordering::SeqCst);
    }

    /// Whether the flag is currently dirty.
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::SeqCst)
    }

    /// Clear the flag. Idempotent.
    pub fn clear(&self) {
        self.dirty.store(false, Ordering::SeqCst);
    }

    /// The epoch-ms timestamp when the flag was most recently marked
    /// dirty. Returns `0` if the flag has never been marked.
    #[must_use]
    pub fn last_marked_epoch_ms(&self) -> u64 {
        self.last_marked_epoch_ms.load(Ordering::SeqCst)
    }

    /// If the flag is dirty and the last-marked timestamp is at least
    /// `debounce_ms` in the past, atomically clear the flag and
    /// return the previous timestamp. Otherwise return `None`.
    ///
    /// Used by the snapshot plugin's background task to decide when
    /// to flush.
    pub fn take_dirty_after_debounce(&self, debounce_ms: u64) -> Option<u64> {
        if !self.is_dirty() {
            return None;
        }
        let last = self.last_marked_epoch_ms();
        let now = epoch_millis_now();
        if now.saturating_sub(last) < debounce_ms {
            return None;
        }
        if self
            .dirty
            .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            Some(last)
        } else {
            None
        }
    }
}

/// Registry newtype wrapping `Arc<SnapshotDirtyFlag>`.
///
/// Stored once in `PluginStateRegistry`. Server reads it to
/// `mark_dirty`; the snapshot plugin reads it to drive its
/// debounced flush task.
#[derive(Clone, Debug)]
pub struct SnapshotDirtyFlagHandle(pub Arc<SnapshotDirtyFlag>);

impl SnapshotDirtyFlagHandle {
    /// Construct a handle wrapping a newly-allocated flag.
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(SnapshotDirtyFlag::new()))
    }

    /// Construct a handle wrapping an existing `Arc`.
    #[must_use]
    pub fn from_arc(flag: Arc<SnapshotDirtyFlag>) -> Self {
        Self(flag)
    }

    /// Borrow the underlying flag.
    #[must_use]
    pub fn flag(&self) -> &SnapshotDirtyFlag {
        &self.0
    }
}

impl Default for SnapshotDirtyFlagHandle {
    fn default() -> Self {
        Self::new()
    }
}

// ── Orchestrator trait + handle ─────────────────────────────────────

/// Errors returned by [`SnapshotOrchestrator`] operations.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotOrchestratorError {
    /// Snapshot persistence is disabled (no file path configured).
    #[error("snapshot persistence is disabled")]
    Disabled,
    /// I/O failure reading or writing the snapshot file.
    #[error("snapshot I/O failure: {0}")]
    Io(String),
    /// Encode/decode failure for the combined envelope.
    #[error("snapshot codec failure: {0}")]
    Codec(String),
    /// Failure propagated from a `StatefulPlugin` participant.
    #[error("stateful plugin '{plugin}' failed: {details}")]
    Participant { plugin: String, details: String },
    /// Generic catch-all.
    #[error("snapshot orchestrator error: {0}")]
    Other(String),
}

/// Result alias for snapshot-orchestrator operations.
pub type SnapshotOrchestratorResult<T> = core::result::Result<T, SnapshotOrchestratorError>;

/// Report produced by [`SnapshotOrchestrator::restore_if_present`] and
/// [`SnapshotOrchestrator::restore_apply`]. Counts are aggregates over
/// every participating plugin.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RestoreSummary {
    /// Number of participating plugins that restored successfully.
    pub restored_plugins: usize,
    /// Number of participants that failed with a
    /// per-plugin error (orchestrator continues past per-plugin
    /// failures).
    pub failed_plugins: usize,
}

/// Report produced by [`SnapshotOrchestrator::dry_run`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DryRunReport {
    /// Whether the snapshot on disk decoded cleanly.
    pub ok: bool,
    /// Human-readable summary (section count, participant ids, etc.).
    pub message: String,
}

/// Status report produced by [`SnapshotOrchestrator::status`]. Maps to
/// the wire `bmux_ipc::ServerSnapshotStatus` shape on the server side.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SnapshotStatusReport {
    /// Whether a snapshot path is configured (vs disabled).
    pub enabled: bool,
    /// Path to the snapshot file when enabled.
    pub path: Option<String>,
    /// Whether the snapshot file currently exists on disk.
    pub snapshot_exists: bool,
    /// Epoch-ms timestamp of the last successful write.
    pub last_write_epoch_ms: Option<u64>,
    /// Epoch-ms timestamp of the last successful restore.
    pub last_restore_epoch_ms: Option<u64>,
    /// Last restore or write error, cleared on next success.
    pub last_restore_error: Option<String>,
}

/// The snapshot orchestrator — implemented by the `bmux.snapshot`
/// plugin, consumed by server IPC handlers and plugin activation
/// code.
///
/// Methods returning futures use native async-fn-in-trait
/// (`impl Future + Send`), matching the `TypedDispatchClient` pattern
/// already in [`bmux_plugin_sdk`]. The server awaits these from its
/// IPC handlers so pane-runtime shutdowns (which are inherently
/// async) can be driven by the restore path.
pub trait SnapshotOrchestrator: Send + Sync {
    /// Restore from the on-disk snapshot if one exists. Returns
    /// `Ok(None)` when persistence is disabled or no snapshot file is
    /// present.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotOrchestratorError::Io`] /
    /// [`SnapshotOrchestratorError::Codec`] on file-level failures.
    /// Per-participant failures are absorbed into
    /// [`RestoreSummary::failed_plugins`]; orchestrator continues
    /// past them.
    fn restore_if_present(
        &self,
    ) -> impl Future<Output = SnapshotOrchestratorResult<Option<RestoreSummary>>> + Send;

    /// Force an immediate save of the combined snapshot, bypassing
    /// the debounce. Returns the snapshot path on success, or `None`
    /// when persistence is disabled.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotOrchestratorError::Io`] /
    /// [`SnapshotOrchestratorError::Codec`] on file-level failures.
    fn save_now(&self) -> impl Future<Output = SnapshotOrchestratorResult<Option<PathBuf>>> + Send;

    /// Decode the on-disk snapshot without applying it; report
    /// whether it is valid and a short summary.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotOrchestratorError::Disabled`] when
    /// persistence is disabled.
    fn dry_run(&self) -> impl Future<Output = SnapshotOrchestratorResult<DryRunReport>> + Send;

    /// Apply the on-disk snapshot as a **replace** operation: clear
    /// all participant state, then restore. Called by
    /// `Request::ServerRestoreApply`.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotOrchestratorError::Disabled`] when
    /// persistence is disabled; otherwise same error semantics as
    /// [`restore_if_present`](SnapshotOrchestrator::restore_if_present).
    fn restore_apply(
        &self,
    ) -> impl Future<Output = SnapshotOrchestratorResult<RestoreSummary>> + Send;

    /// Return the orchestrator's current status.
    fn status(&self) -> SnapshotStatusReport;
}

/// Type-erased registry handle for a [`SnapshotOrchestrator`].
///
/// Because `SnapshotOrchestrator` uses native async-fn-in-trait,
/// this handle wraps the concrete orchestrator with a thin adapter
/// that boxes the returned futures so callers can hold
/// `Arc<dyn SnapshotOrchestratorApi>` in registry slots.
#[derive(Clone)]
pub struct SnapshotOrchestratorHandle(Arc<dyn SnapshotOrchestratorApi>);

impl SnapshotOrchestratorHandle {
    /// Wrap a concrete orchestrator. The adapter boxes the returned
    /// futures to keep the registry slot object-safe.
    #[must_use]
    pub fn new<O>(orchestrator: O) -> Self
    where
        O: SnapshotOrchestrator + 'static,
    {
        Self(Arc::new(OrchestratorAdapter {
            inner: orchestrator,
        }))
    }

    /// Wrap an `Arc`-shared concrete orchestrator. Useful when the
    /// plugin needs to retain the concrete type (e.g. to drive a
    /// background debounce thread) while simultaneously exposing the
    /// trait-object façade to the rest of the host.
    #[must_use]
    pub fn from_shared<O>(orchestrator: Arc<O>) -> Self
    where
        O: SnapshotOrchestrator + 'static,
    {
        Self(Arc::new(SharedOrchestratorAdapter {
            inner: orchestrator,
        }))
    }

    /// Borrow the underlying dyn-safe API surface.
    #[must_use]
    pub fn as_dyn(&self) -> &(dyn SnapshotOrchestratorApi + 'static) {
        self.0.as_ref()
    }

    /// Construct a handle backed by [`NoopSnapshotOrchestrator`].
    #[must_use]
    pub fn noop() -> Self {
        Self::new(NoopSnapshotOrchestrator)
    }
}

/// Dyn-safe façade over [`SnapshotOrchestrator`] with boxed futures.
///
/// This is the shape stored in the plugin state registry. Callers
/// reach the async methods by awaiting the returned
/// `Pin<Box<dyn Future + Send>>`.
pub trait SnapshotOrchestratorApi: Send + Sync {
    /// Boxed counterpart of [`SnapshotOrchestrator::restore_if_present`].
    fn restore_if_present_boxed(
        &self,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = SnapshotOrchestratorResult<Option<RestoreSummary>>> + Send + '_>,
    >;

    /// Boxed counterpart of [`SnapshotOrchestrator::save_now`].
    fn save_now_boxed(
        &self,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = SnapshotOrchestratorResult<Option<PathBuf>>> + Send + '_>,
    >;

    /// Boxed counterpart of [`SnapshotOrchestrator::dry_run`].
    fn dry_run_boxed(
        &self,
    ) -> std::pin::Pin<Box<dyn Future<Output = SnapshotOrchestratorResult<DryRunReport>> + Send + '_>>;

    /// Boxed counterpart of [`SnapshotOrchestrator::restore_apply`].
    fn restore_apply_boxed(
        &self,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = SnapshotOrchestratorResult<RestoreSummary>> + Send + '_>,
    >;

    /// Pass-through to [`SnapshotOrchestrator::status`].
    fn status(&self) -> SnapshotStatusReport;
}

struct OrchestratorAdapter<O: SnapshotOrchestrator> {
    inner: O,
}

impl<O: SnapshotOrchestrator> SnapshotOrchestratorApi for OrchestratorAdapter<O> {
    fn restore_if_present_boxed(
        &self,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = SnapshotOrchestratorResult<Option<RestoreSummary>>> + Send + '_>,
    > {
        Box::pin(self.inner.restore_if_present())
    }

    fn save_now_boxed(
        &self,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = SnapshotOrchestratorResult<Option<PathBuf>>> + Send + '_>,
    > {
        Box::pin(self.inner.save_now())
    }

    fn dry_run_boxed(
        &self,
    ) -> std::pin::Pin<Box<dyn Future<Output = SnapshotOrchestratorResult<DryRunReport>> + Send + '_>>
    {
        Box::pin(self.inner.dry_run())
    }

    fn restore_apply_boxed(
        &self,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = SnapshotOrchestratorResult<RestoreSummary>> + Send + '_>,
    > {
        Box::pin(self.inner.restore_apply())
    }

    fn status(&self) -> SnapshotStatusReport {
        self.inner.status()
    }
}

struct SharedOrchestratorAdapter<O: SnapshotOrchestrator> {
    inner: Arc<O>,
}

impl<O: SnapshotOrchestrator> SnapshotOrchestratorApi for SharedOrchestratorAdapter<O> {
    fn restore_if_present_boxed(
        &self,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = SnapshotOrchestratorResult<Option<RestoreSummary>>> + Send + '_>,
    > {
        Box::pin(self.inner.restore_if_present())
    }

    fn save_now_boxed(
        &self,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = SnapshotOrchestratorResult<Option<PathBuf>>> + Send + '_>,
    > {
        Box::pin(self.inner.save_now())
    }

    fn dry_run_boxed(
        &self,
    ) -> std::pin::Pin<Box<dyn Future<Output = SnapshotOrchestratorResult<DryRunReport>> + Send + '_>>
    {
        Box::pin(self.inner.dry_run())
    }

    fn restore_apply_boxed(
        &self,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = SnapshotOrchestratorResult<RestoreSummary>> + Send + '_>,
    > {
        Box::pin(self.inner.restore_apply())
    }

    fn status(&self) -> SnapshotStatusReport {
        self.inner.status()
    }
}

// ── Defaults ────────────────────────────────────────────────────────

/// No-op default orchestrator. Registered by the server at startup so
/// lookups never fail; the snapshot plugin overwrites it during
/// `activate`.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopSnapshotOrchestrator;

impl SnapshotOrchestrator for NoopSnapshotOrchestrator {
    async fn restore_if_present(&self) -> SnapshotOrchestratorResult<Option<RestoreSummary>> {
        Ok(None)
    }

    async fn save_now(&self) -> SnapshotOrchestratorResult<Option<PathBuf>> {
        Ok(None)
    }

    async fn dry_run(&self) -> SnapshotOrchestratorResult<DryRunReport> {
        Err(SnapshotOrchestratorError::Disabled)
    }

    async fn restore_apply(&self) -> SnapshotOrchestratorResult<RestoreSummary> {
        Err(SnapshotOrchestratorError::Disabled)
    }

    fn status(&self) -> SnapshotStatusReport {
        SnapshotStatusReport::default()
    }
}

// ── Utilities ───────────────────────────────────────────────────────

#[allow(clippy::cast_possible_truncation)]
fn epoch_millis_now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_plugin_sdk::{PluginEventKind, StatefulPlugin, StatefulPluginSnapshot};
    use std::sync::Mutex;

    const TEST_ID: PluginEventKind = PluginEventKind::from_static("bmux.test/snapshot");

    struct Counter {
        value: Mutex<u32>,
    }

    impl StatefulPlugin for Counter {
        fn id(&self) -> PluginEventKind {
            TEST_ID
        }

        fn snapshot(&self) -> bmux_plugin_sdk::StatefulPluginResult<StatefulPluginSnapshot> {
            let v = *self.value.lock().unwrap();
            Ok(StatefulPluginSnapshot::new(
                TEST_ID,
                1,
                v.to_le_bytes().to_vec(),
            ))
        }
    }

    #[test]
    fn registry_starts_empty_and_grows_on_push() {
        let mut registry = StatefulPluginRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        registry.push(StatefulPluginHandle::new(Counter {
            value: Mutex::new(3),
        }));
        assert_eq!(registry.len(), 1);
        assert!(!registry.is_empty());
    }

    #[test]
    fn dirty_flag_starts_clean() {
        let flag = SnapshotDirtyFlag::new();
        assert!(!flag.is_dirty());
        assert_eq!(flag.last_marked_epoch_ms(), 0);
    }

    #[test]
    fn dirty_flag_mark_sets_bit_and_timestamp() {
        let flag = SnapshotDirtyFlag::new();
        flag.mark_dirty();
        assert!(flag.is_dirty());
        assert!(flag.last_marked_epoch_ms() > 0);
    }

    #[test]
    fn dirty_flag_clear_resets_bit_but_keeps_timestamp() {
        let flag = SnapshotDirtyFlag::new();
        flag.mark_dirty();
        let ts = flag.last_marked_epoch_ms();
        flag.clear();
        assert!(!flag.is_dirty());
        assert_eq!(flag.last_marked_epoch_ms(), ts);
    }

    #[test]
    fn take_dirty_after_debounce_waits_for_window() {
        let flag = SnapshotDirtyFlag::new();
        flag.mark_dirty();
        // Immediately: debounce window (1s) not yet elapsed.
        assert!(flag.take_dirty_after_debounce(1_000).is_none());
        // Zero-ms debounce always takes.
        assert!(flag.take_dirty_after_debounce(0).is_some());
        // Flag is now clean.
        assert!(!flag.is_dirty());
    }

    #[tokio::test]
    async fn noop_orchestrator_returns_expected_defaults() {
        let orchestrator = NoopSnapshotOrchestrator;
        assert!(orchestrator.restore_if_present().await.unwrap().is_none());
        assert!(orchestrator.save_now().await.unwrap().is_none());
        assert!(matches!(
            orchestrator.dry_run().await,
            Err(SnapshotOrchestratorError::Disabled)
        ));
        assert!(matches!(
            orchestrator.restore_apply().await,
            Err(SnapshotOrchestratorError::Disabled)
        ));
        let status = orchestrator.status();
        assert!(!status.enabled);
    }

    #[tokio::test]
    async fn handle_wraps_concrete_orchestrator() {
        let handle = SnapshotOrchestratorHandle::noop();
        assert!(
            handle
                .as_dyn()
                .restore_if_present_boxed()
                .await
                .unwrap()
                .is_none()
        );
        assert!(!handle.as_dyn().status().enabled);
    }

    #[test]
    fn dirty_flag_handle_defaults_to_clean() {
        let handle = SnapshotDirtyFlagHandle::default();
        assert!(!handle.flag().is_dirty());
    }
}
