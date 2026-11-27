//! Concrete pane runtime implementation owned by the pane-runtime plugin.

mod capture_writer;
mod console_startup;
mod durable_capture;
mod environment;
pub(crate) mod execution;
pub(crate) mod human_input;
#[cfg(test)]
mod ingest_bench;
pub(crate) mod input_authority;
pub(crate) mod live_presentation;
pub(crate) mod managed_idle;
mod managed_scope;
pub(crate) mod output_filters;
pub(crate) mod pty;
pub(crate) mod reservations;
pub(crate) mod session_control;
mod shell_callback;
#[cfg(feature = "simulator")]
pub mod simulation;
mod startup_capture;

use crate::platform::{
    ProcessStartIdentity, PsProcessEntry, process_start_identity as process_start_identity_for_pid,
    resolve_process_working_directory, send_process_signal,
};

use anyhow::{Context, Result};
use bmux_attach_layout_protocol::{
    AttachFocusTarget, AttachInputModeState, AttachLayer, AttachMouseProtocolEncoding,
    AttachMouseProtocolMode, AttachMouseProtocolState, AttachPaneChunk, AttachPaneInputMode,
    AttachPaneMouseProtocol, AttachRect, AttachScene, AttachSurface, AttachSurfaceKind,
    PaneFocusDirection, PaneLaunchCommand, PaneLayoutNode as IpcPaneLayoutNode, PaneSelector,
    PaneSplitDirection, PaneState, PaneSummary,
};
use bmux_context_state::ContextStateHandle;
use bmux_ipc::{ErrorCode, Event};
use bmux_pane_runtime_plugin_api::{
    PaneRuntimePluginConfig,
    pane_runtime_events::{self, AttachViewComponent, PaneEvent},
    pane_runtime_terminal_events::{self, PromptMarkKind},
};
use bmux_pane_runtime_state::{
    AttachViewport, FloatingPaneLayer, FloatingPaneRuntimeSummary, FloatingPaneScope,
    FloatingSurfaceRuntime, LayoutRect, PaneCommandSource, PaneLaunchSpec, PaneLayoutNode,
    PaneResizeDirection, PaneResurrectionSnapshot, PaneRuntimeMeta, RestoreRuntimeRequest,
    SessionRuntimeError,
};
use bmux_pty::{PtySize, SpawnCommand};
use bmux_recording_protocol::{RecordingEventKind, RecordingPayload as ProtocolRecordingPayload};
use bmux_recording_runtime::{RecordMeta, RecordingSinkHandle};
use bmux_session_models::{ClientId, SessionId};
use bmux_session_state::SessionManagerHandle;
use bmux_snapshot_runtime::{SnapshotDirtyFlag, SnapshotDirtyFlagHandle};
use bmux_terminal_grid::{
    ChunkMark, FilterVerdict, GridLimits, ProtocolState, SequenceHeader, SequenceObserver,
    StreamedString, TerminalGrid, TerminalGridStream,
};
use bmux_terminal_protocol::{ProtocolProfile, TerminalProtocolEngine, protocol_profile_for_term};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;
use switchy_async::sync::watch;
use switchy_async::task::JoinHandle;
use switchy_thread::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use switchy_thread::sync::{Arc, Mutex, RwLock};
use switchy_time::Instant;
use tokio::sync::{mpsc, oneshot};
use tracing::{trace, warn};

pub(crate) fn managed_target_identity(host: &PaneRuntimeHost) -> Result<Option<String>, String> {
    managed_scope::selected(host).map(|scope| scope.map(|scope| scope.identity))
}

/// Capture this activation's managed-target scope from its launch markers.
pub(crate) fn activate_managed_scope(host: &PaneRuntimeHost) {
    managed_scope::activate(host);
}

// Process-wide diagnostic: counts OS threads blocked in pane I/O across every
// activation in this process (a thread budget, not per-instance state).
/// Pane blocking threads alive (see [`crate::run_shared`]).
enum PaneBlockingThreads {}

fn pane_blocking_threads() -> Arc<AtomicUsize> {
    crate::run_shared::run_shared::<PaneBlockingThreads, _>(|| AtomicUsize::new(0))
}

struct PaneBlockingThreadGuard {
    pane_id: Uuid,
    role: &'static str,
}

impl PaneBlockingThreadGuard {
    fn new(pane_id: Uuid, role: &'static str) -> Self {
        let active = pane_blocking_threads().fetch_add(1, Ordering::SeqCst) + 1;
        trace!(%pane_id, role, active, "pane blocking thread started");
        Self { pane_id, role }
    }
}

impl Drop for PaneBlockingThreadGuard {
    fn drop(&mut self) {
        let active = pane_blocking_threads().fetch_sub(1, Ordering::SeqCst) - 1;
        trace!(pane_id = %self.pane_id, role = self.role, active, "pane blocking thread exited");
    }
}
use crate::padding::{
    PanePaddingConfig, PanePaddingMetadata, PanePaddingSpec, base_content_rect,
    padded_content_rect, validate_spec,
};
use uuid::Uuid;

type RecordingPayload = ProtocolRecordingPayload<Event, ErrorCode>;

const MAX_WINDOW_OUTPUT_BUFFER_BYTES: usize = 1_048_576;
const MAX_NODE_OUTPUT_BUFFER_BYTES: usize = 256 * 1_048_576;
const RESPONSE_METADATA_HEADROOM: usize = 65_536;
const PANE_PADDING_PREVIEW_LEASE: Duration = Duration::from_mins(5);
const PANE_PADDING_PREVIEW_SWEEP_INTERVAL: Duration = Duration::from_secs(1);
const RESPONSE_OUTPUT_BUDGET: usize =
    bmux_ipc::frame::MAX_FRAME_PAYLOAD_SIZE - RESPONSE_METADATA_HEADROOM;
/// Encoded bytes one reply may carry. Replies cross both the IPC frame and,
/// when this plugin runs in-process, the native plugin outcome buffer; the
/// smaller binds. [`RESPONSE_OUTPUT_BUDGET`] also bounds in-memory projection,
/// which is far larger per cell than the encoding, so the two stay separate.
const REPLY_WIRE_BUDGET: usize = {
    let native = bmux_plugin_sdk::abi::MAX_BUFFER_BYTES - RESPONSE_METADATA_HEADROOM;
    if native < RESPONSE_OUTPUT_BUDGET {
        native
    } else {
        RESPONSE_OUTPUT_BUDGET
    }
};

// Published pin payload across this manager, independent of client/session count.
const RETAINED_PIN_BUDGET: usize = 64 * 1024 * 1024;
/// Why pane input from a client without an input target is refused.
pub(crate) const NO_INPUT_TARGET: &str =
    "no input target: this client's selection was cleared; select a target to resume input";

fn admit_retained_pin(
    manager: &SessionRuntimeManager,
    session_id: SessionId,
    client_id: ClientId,
    pane_id: Uuid,
    candidate: &bmux_terminal_grid::TerminalGrid,
) -> Result<(), SessionRuntimeError> {
    let mut remaining = RETAINED_PIN_BUDGET;
    for (id, runtime) in &manager.runtimes {
        for (key, pin) in &runtime.scrollback_pins {
            if *id == session_id && *key == (client_id, pane_id) {
                continue;
            }
            let bytes = pin.grid.reservation.bytes;
            remaining = remaining
                .checked_sub(bytes)
                .ok_or(SessionRuntimeError::ResponseBudgetExceeded)?;
        }
    }
    let bytes = candidate
        .retained_capacity_bytes()
        .ok_or(SessionRuntimeError::ResponseBudgetExceeded)?;
    remaining
        .checked_sub(bytes)
        .ok_or(SessionRuntimeError::ResponseBudgetExceeded)?;
    Ok(())
}

/// The host handles one pane-runtime activation received: its view of the
/// host plugin state registry and its event bus. Every runtime, handler and
/// task reaches the host only through the instance it was given; there is no
/// process-wide lookup.
#[derive(Clone)]
pub(crate) struct PaneRuntimeHost {
    states: Arc<bmux_plugin::PluginStateRegistry>,
    events: Option<Arc<bmux_plugin::EventBus>>,
    /// Monotonic revision of this activation's focus-state publications.
    focus_revision: Arc<AtomicU64>,
    /// Monotonic revision of this activation's presentation-fact publications.
    fact_revision: Arc<AtomicU64>,
    /// This activation's output-push subscriptions.
    output_push: Arc<crate::handlers::output_push::OutputPushState>,
    /// This activation's readiness, published once at activation.
    readiness: Arc<std::sync::OnceLock<crate::readiness::Readiness>>,
    /// This activation's raw input capture barriers.
    private_input: Arc<crate::private_input::PrivateInputState>,
    /// This activation's read-only observer connections.
    observers: Arc<Mutex<crate::handlers::attach_commands::ObserverIdentities>>,
    /// This activation's managed-target scope, captured once at activation.
    managed_scope: Arc<managed_scope::ManagedScope>,
    /// This activation's managed idle/retirement admission authority.
    idle: managed_idle::IdleAuthority,
    /// This activation's terminal-session settings and crash-record root.
    sessions: Arc<session_control::SessionAuthority>,
    /// This activation's durable execution reservation store.
    reservations: Arc<reservations::ReservationAuthority>,
    /// This activation's ephemeral HUMAN input epochs.
    human_input: Arc<human_input::HumanInputStreams>,
    /// This activation's synchronous input-invocation scopes.
    input_scopes: Arc<input_authority::InputScopes>,
    /// This activation's application-resurrection client, when enabled.
    resurrection: Arc<crate::application_resurrection::ResurrectionClient>,
    /// Owned pane cleanup tasks still settling for this activation.
    shutdown_owners: Arc<Mutex<BTreeMap<Uuid, PaneShutdownOwner>>>,
}

impl std::fmt::Debug for PaneRuntimeHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaneRuntimeHost")
            .field("events", &self.events.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for PaneRuntimeHost {
    fn default() -> Self {
        Self::detached()
    }
}

impl PaneRuntimeHost {
    pub(crate) fn new(
        states: Arc<bmux_plugin::PluginStateRegistry>,
        events: Option<Arc<bmux_plugin::EventBus>>,
    ) -> Self {
        Self {
            states,
            events,
            focus_revision: Arc::new(AtomicU64::new(0)),
            fact_revision: Arc::new(AtomicU64::new(0)),
            output_push: Arc::default(),
            readiness: Arc::default(),
            private_input: Arc::default(),
            observers: Arc::default(),
            managed_scope: Arc::default(),
            idle: managed_idle::IdleAuthority::default(),
            sessions: Arc::default(),
            reservations: Arc::default(),
            human_input: Arc::default(),
            input_scopes: Arc::default(),
            resurrection: Arc::default(),
            shutdown_owners: Arc::default(),
        }
    }

    pub(crate) fn managed_scope(&self) -> &managed_scope::ManagedScope {
        &self.managed_scope
    }

    pub(crate) const fn idle(&self) -> &managed_idle::IdleAuthority {
        &self.idle
    }

    pub(crate) fn sessions(&self) -> &session_control::SessionAuthority {
        &self.sessions
    }

    pub(crate) fn reservations(&self) -> &reservations::ReservationAuthority {
        &self.reservations
    }

    pub(crate) fn human_input(&self) -> &human_input::HumanInputStreams {
        &self.human_input
    }

    pub(crate) fn input_scopes(&self) -> &input_authority::InputScopes {
        &self.input_scopes
    }

    pub(crate) fn resurrection(&self) -> &crate::application_resurrection::ResurrectionClient {
        &self.resurrection
    }

    pub(crate) fn pane_shutdown_owners(&self) -> &Mutex<BTreeMap<Uuid, PaneShutdownOwner>> {
        &self.shutdown_owners
    }

    /// A host with an empty private registry and no bus (no host supplied).
    pub(crate) fn detached() -> Self {
        Self::new(Arc::new(bmux_plugin::PluginStateRegistry::new()), None)
    }

    pub(crate) fn states(&self) -> &Arc<bmux_plugin::PluginStateRegistry> {
        &self.states
    }

    pub(crate) const fn events(&self) -> Option<&Arc<bmux_plugin::EventBus>> {
        self.events.as_ref()
    }

    pub(crate) fn shutdown_progress(
        &self,
    ) -> bmux_plugin_runtime::shutdown_progress::ShutdownProgress {
        self.states.shutdown_progress()
    }

    fn handle<T: Clone + Send + Sync + 'static>(&self) -> Option<T> {
        self.states
            .get::<T>()
            .and_then(|arc| arc.read().ok().map(|guard| guard.clone()))
    }

    fn context_handle(&self) -> ContextStateHandle {
        self.handle::<ContextStateHandle>()
            .unwrap_or_else(ContextStateHandle::noop)
    }

    /// Refuse pane input from a client left without an input target: its
    /// selection was explicitly cleared and nothing has selected a target
    /// for it since. Its attach stream stays bound to the last session, so
    /// without this a stale or misbehaving client could still write to the
    /// pane it no longer shows. Clients that were never cleared, and hosts
    /// with no selection provider at all, always pass.
    ///
    /// # Errors
    ///
    /// Returns the reason reported to the client when it has no input target.
    pub(crate) fn ensure_client_has_input_target(
        &self,
        client_id: ClientId,
    ) -> Result<(), &'static str> {
        if self
            .context_handle()
            .0
            .selection_cleared_for_client(client_id)
        {
            Err(NO_INPUT_TARGET)
        } else {
            Ok(())
        }
    }

    pub(crate) fn session_handle(&self) -> SessionManagerHandle {
        self.handle::<SessionManagerHandle>()
            .unwrap_or_else(SessionManagerHandle::noop)
    }

    fn snapshot_dirty_flag(&self) -> Arc<SnapshotDirtyFlag> {
        self.handle::<SnapshotDirtyFlagHandle>()
            .map_or_else(|| Arc::new(SnapshotDirtyFlag::new()), |handle| handle.0)
    }

    pub(crate) fn mark_snapshot_dirty(&self) {
        self.snapshot_dirty_flag().mark_dirty();
    }

    /// Publish what a pane's program signalled through its output (see
    /// `pane-runtime-terminal-events`).
    fn publish_terminal_event(&self, event: pane_runtime_terminal_events::TerminalEvent) {
        let Some(events) = &self.events else {
            return;
        };
        if let Err(error) = events.emit(&pane_runtime_terminal_events::EVENT_KIND, event) {
            warn!(%error, "failed publishing pane terminal event");
        }
    }

    pub(crate) fn publish_pane_event(&self, event: PaneEvent) {
        let Some(events) = &self.events else {
            return;
        };
        if let Err(error) = events.emit(&pane_runtime_events::EVENT_KIND, event) {
            warn!(%error, "failed publishing pane-runtime event");
        }
    }

    /// Record an event built by `payload` only when a recording would keep it: pane output is
    /// copied for recording only while something records it.
    fn record_lazily(
        &self,
        kind: RecordingEventKind,
        meta: RecordMeta,
        payload: impl FnOnce() -> RecordingPayload,
    ) {
        let Some(handle) = self.recording_sink() else {
            return;
        };
        if handle.0.accepts(kind, meta) {
            handle.0.record(kind, payload(), meta);
        }
    }

    fn record(&self, kind: RecordingEventKind, payload: RecordingPayload, meta: RecordMeta) {
        let Some(handle) = self.recording_sink() else {
            return;
        };
        handle.0.record(kind, payload, meta);
    }

    pub(crate) fn recording_sink(&self) -> Option<RecordingSinkHandle> {
        self.handle::<RecordingSinkHandle>()
    }

    pub(crate) fn pane_input_recording_data(
        &self,
        data: &[u8],
    ) -> Option<(RecordingSinkHandle, Vec<u8>)> {
        self.recording_sink().map(|handle| (handle, data.to_vec()))
    }

    pub(crate) fn current_context_id_for_session(&self, session_id: SessionId) -> Option<Uuid> {
        self.context_handle().0.context_for_session(session_id)
    }

    pub(crate) fn emit_attach_view_changed_for_layout(&self, session_id: SessionId) {
        let revision = self
            .session_runtime_handle()
            .0
            .bump_attach_view_revision(session_id);
        self.publish_pane_event(PaneEvent::AttachViewChanged {
            context_id: self.current_context_id_for_session(session_id),
            session_id: session_id.0,
            revision: revision.unwrap_or(0),
            components: vec![
                AttachViewComponent::Scene,
                AttachViewComponent::SurfaceContent,
                AttachViewComponent::Layout,
            ],
        });
    }

    pub(crate) fn session_runtime_handle(
        &self,
    ) -> bmux_pane_runtime_state::SessionRuntimeManagerHandle {
        self.handle::<bmux_pane_runtime_state::SessionRuntimeManagerHandle>()
            .unwrap_or_else(bmux_pane_runtime_state::SessionRuntimeManagerHandle::noop)
    }

    /// The registered runtime manager, if any (no noop fallback).
    pub(crate) fn registered_session_runtime(
        &self,
    ) -> Option<bmux_pane_runtime_state::SessionRuntimeManagerHandle> {
        self.handle::<bmux_pane_runtime_state::SessionRuntimeManagerHandle>()
    }

    /// Next focus-state revision for this activation.
    pub(crate) fn next_focus_revision(&self) -> u64 {
        self.focus_revision.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub(crate) fn private_input(&self) -> &crate::private_input::PrivateInputState {
        &self.private_input
    }

    pub(crate) fn observers(&self) -> &Mutex<crate::handlers::attach_commands::ObserverIdentities> {
        &self.observers
    }

    pub(crate) fn readiness(&self) -> &std::sync::OnceLock<crate::readiness::Readiness> {
        &self.readiness
    }

    pub(crate) fn output_push(&self) -> &crate::handlers::output_push::OutputPushState {
        &self.output_push
    }

    /// Next presentation-fact revision for this activation.
    pub(crate) fn next_fact_revision(&self) -> u64 {
        self.fact_revision
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1)
    }

    /// The host's communal presentation fact service (in core's state registry, so behind
    /// std's lock).
    pub(crate) fn fact_service(
        &self,
        // simulation-safe: core's state registry type, written only at registration
    ) -> Arc<std::sync::RwLock<bmux_presentation_state::PresentationFactHostService>> {
        self.states
            .get_or_insert_with(bmux_presentation_state::PresentationFactHostService::new)
    }

    pub(crate) fn spawn_runtime_handle(&self) -> Option<SpawnRuntimeHandle> {
        self.handle::<SpawnRuntimeHandle>()
    }

    fn pane_padding_handle(&self) -> Option<PanePaddingRuntimeHandle> {
        self.handle::<PanePaddingRuntimeHandle>()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedPaneLaunch {
    shell: String,
    cwd: Option<String>,
    replay_command: Option<String>,
    metadata_changed: bool,
}

fn path_like_program(program: &str) -> bool {
    let path = Path::new(program);
    path.is_absolute() || path.components().count() > 1
}

fn program_is_usable(program: &str) -> bool {
    let program = program.trim();
    !program.is_empty()
        && if path_like_program(program) {
            crate::platform::path_is_usable_program(Path::new(program))
        } else {
            crate::platform::program_name_is_on_path(program)
        }
}

fn shell_basename(shell: &str) -> Option<String> {
    Path::new(shell)
        .file_name()
        .and_then(|value| value.to_str())
        .map(|value| value.trim_start_matches('-').to_ascii_lowercase())
        .filter(|value| !value.is_empty())
}

fn shells_support_compatible_replay(previous: &str, resolved: &str) -> bool {
    let previous_kind = shell_kind_for_path(previous);
    let resolved_kind = shell_kind_for_path(resolved);
    match (previous_kind, resolved_kind) {
        (ShellKind::Other, ShellKind::Other) => {
            shell_basename(previous) == shell_basename(resolved)
        }
        (left, right) => left == right,
    }
}

fn valid_resurrection_cwd(cwd: Option<&str>) -> Option<String> {
    cwd.filter(|value| !value.trim().is_empty())
        .filter(|value| crate::files::is_dir(Path::new(value)))
        .map(ToOwned::to_owned)
}

fn resolve_shell_with_baseline(
    persisted_shell: &str,
    configured_shell: &str,
    baseline_shell: &str,
) -> anyhow::Result<String> {
    [persisted_shell.trim(), configured_shell.trim(), baseline_shell]
        .into_iter()
        .find(|candidate| program_is_usable(candidate))
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no usable shell found (persisted='{persisted_shell}', configured='{configured_shell}', baseline='{baseline_shell}')"
            )
        })
}

fn resolve_pane_launch(
    pane_meta: &PaneRuntimeMeta,
    configured_shell: &str,
) -> anyhow::Result<ResolvedPaneLaunch> {
    let shell = if pane_meta.launch.is_some() {
        pane_meta.shell.clone()
    } else {
        resolve_shell_with_baseline(
            &pane_meta.shell,
            configured_shell,
            crate::platform::baseline_shell(),
        )?
    };
    let cwd = valid_resurrection_cwd(pane_meta.resurrection.last_known_cwd.as_deref());
    let replay_command = if pane_meta.launch.is_some() {
        pane_meta.resurrection.active_command.clone()
    } else {
        pane_meta
            .resurrection
            .active_command
            .clone()
            .filter(|_| shells_support_compatible_replay(&pane_meta.shell, &shell))
    };
    let metadata_changed = shell != pane_meta.shell
        || cwd != pane_meta.resurrection.last_known_cwd
        || replay_command != pane_meta.resurrection.active_command;

    Ok(ResolvedPaneLaunch {
        shell,
        cwd,
        replay_command,
        metadata_changed,
    })
}

fn apply_resolved_pane_launch(pane_meta: &mut PaneRuntimeMeta, resolved: &ResolvedPaneLaunch) {
    pane_meta.shell.clone_from(&resolved.shell);
    pane_meta
        .resurrection
        .last_known_cwd
        .clone_from(&resolved.cwd);
    pane_meta
        .resurrection
        .active_command
        .clone_from(&resolved.replay_command);
    if pane_meta.resurrection.active_command.is_none() {
        pane_meta.resurrection.active_command_source = None;
    }
}

fn validate_explicit_launch(launch: &PaneLaunchSpec) -> anyhow::Result<()> {
    if launch.program.trim().is_empty() {
        anyhow::bail!("explicit launch program is empty");
    }
    if let Some(cwd) = launch.cwd.as_deref()
        && (cwd.trim().is_empty() || !crate::files::is_dir(Path::new(cwd)))
    {
        anyhow::bail!("explicit launch cwd is unavailable: '{cwd}'");
    }
    Ok(())
}

fn record_pane_input_raw(
    handle: &RecordingSinkHandle,
    data: Vec<u8>,
    session_id: SessionId,
    pane_id: Uuid,
    client_id: ClientId,
) {
    handle.0.record(
        RecordingEventKind::PaneInputRaw,
        RecordingPayload::Bytes { data },
        RecordMeta {
            session_id: Some(session_id.0),
            pane_id: Some(pane_id),
            client_id: Some(client_id.0),
        },
    );
}

async fn shutdown_runtime_handle(host: &crate::runtime::PaneRuntimeHost, removed: RemovedRuntime) {
    shutdown_panes(host, removed.handle.panes.into_values().collect()).await;
}

// A cancelled service waiter must not cancel its cleanup owner. Retain every
// owner JoinHandle until it has awaited all pane tasks (including their reapers).
// Unlike a detached JoinHandle, this registry makes outstanding cleanup owned.
type PaneShutdownOwner = JoinHandle<std::result::Result<(), String>>;

async fn shutdown_panes(host: &crate::runtime::PaneRuntimeHost, panes: Vec<PaneRuntimeHandle>) {
    if let Err(error) = shutdown_panes_result(host, panes).await {
        warn!(%error, "pane shutdown failed; cleanup owner retained until settlement");
    }
}

async fn shutdown_panes_result(
    host: &crate::runtime::PaneRuntimeHost,
    panes: Vec<PaneRuntimeHandle>,
) -> std::result::Result<(), String> {
    shutdown_panes_owned(host, panes, false).await
}

fn pane_cleanup_failure_summary(
    failures: impl IntoIterator<Item = (Uuid, String)>,
) -> Option<String> {
    let mut causes = BTreeMap::<String, (usize, Vec<Uuid>)>::new();
    let mut total = 0;
    for (id, error) in failures {
        total += 1;
        let (count, examples) = causes.entry(error).or_default();
        *count += 1;
        if examples.len() < 3 {
            examples.push(id);
        }
    }
    if total == 0 {
        return None;
    }
    let mut details = causes
        .iter()
        .take(8)
        .map(|(cause, (count, examples))| {
            let ids = examples
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            format!("{count} pane(s) [{ids}]: {cause}")
        })
        .collect::<Vec<_>>();
    if causes.len() > 8 {
        details.push(format!(
            "{} additional causes; see server logs",
            causes.len() - 8
        ));
    }
    Some(format!(
        "cleanup failed for {total} pane(s): {}",
        details.join("; ")
    ))
}

async fn await_pane_cleanup(
    host: &crate::runtime::PaneRuntimeHost,
    panes: &mut [PaneRuntimeHandle],
    id: Uuid,
    wait_for_settlement: bool,
    settled: &mut Option<oneshot::Sender<std::result::Result<(), String>>>,
    errors: &mut Vec<String>,
) {
    let progress = host.shutdown_progress();
    let prior_errors = errors.clone();
    // Cleanup acknowledgement can fail before the task exits: the task
    // still owns late OS reapers. Observe both independently, returning
    // failure promptly without dropping or aborting that owner.
    let mut acknowledgements = panes
        .iter()
        .map(|pane| {
            let mut cleanup = pane.cleanup.clone();
            Box::pin(async move {
                loop {
                    if let Some(result) = cleanup.borrow_and_update().clone() {
                        return result;
                    }
                    if cleanup.changed().await.is_err() {
                        return Err("pane task ended without cleanup acknowledgement".into());
                    }
                }
            })
        })
        .collect::<Vec<_>>();
    let mut acknowledged = vec![false; panes.len()];
    let mut pending = vec![true; panes.len()];
    let mut diagnostic = Box::pin(switchy_async::time::sleep(Duration::from_secs(5)));
    std::future::poll_fn(|cx| {
        use std::future::Future;
        if diagnostic.as_mut().poll(cx).is_ready() {
            warn!(phase = "Release", cleanup_id = %id,
                pending_tasks = pending.iter().filter(|pending| **pending).count(),
                errors = ?errors,
                "pane cleanup still waiting; owners/reapers retained, plugin unload fenced");
            diagnostic.set(switchy_async::time::sleep(Duration::from_secs(5)));
            let _ = diagnostic.as_mut().poll(cx);
        }
        for (index, (pane, pending)) in panes.iter_mut().zip(&mut pending).enumerate() {
            if !acknowledged[index] {
                match acknowledgements[index].as_mut().poll(cx) {
                    std::task::Poll::Ready(result) => {
                        acknowledged[index] = true;
                        if let Err(error) = progress.advance("resource cleanup acknowledged") {
                            warn!(%error, "shutdown progress publication failed");
                        }
                        if let Err(error) = result {
                            pane.execution.record_cleanup_error(error.clone());
                            errors.push(error);
                        }
                    }
                    std::task::Poll::Pending => {}
                }
            }
            if !*pending {
                continue;
            }
            match std::pin::Pin::new(&mut pane.task).poll(cx) {
                std::task::Poll::Ready(result) => {
                    *pending = false;
                    if let Err(error) = progress.advance("resource cleanup task settled") {
                        warn!(%error, "shutdown progress publication failed");
                    }
                    if let Err(error) = result {
                        let error = format!("pane cleanup task failed: {error}");
                        pane.execution.record_cleanup_error(error.clone());
                        errors.push(error);
                    } else if let Some(Err(error)) = pane.cleanup.borrow().as_ref() {
                        pane.execution.record_cleanup_error(error.clone());
                    } else if pane.cleanup.borrow().is_none() {
                        let error = "pane task ended without cleanup acknowledgement".to_string();
                        pane.execution.record_cleanup_error(error.clone());
                        errors.push(error);
                    }
                    acknowledged[index] = true;
                }
                std::task::Poll::Pending => {}
            }
        }
        // Rebuild from retained execution failures, rather than appending the
        // same receipt on every poll. Include identity once per failed pane.
        errors.clone_from(&prior_errors);
        errors.extend(pane_cleanup_failure_summary(panes.iter().filter_map(
            |pane| {
                pane.execution
                    .cleanup_error()
                    .map(|error| (pane.meta.id, error))
            },
        )));
        if !errors.is_empty()
            && !wait_for_settlement
            && let Some(settled) = settled.take()
        {
            let _ = settled.send(Err(errors.join("; ")));
        }
        if pending.iter().any(|pending| *pending) || acknowledged.iter().any(|ack| !*ack) {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    })
    .await;
}

async fn shutdown_panes_owned(
    host: &crate::runtime::PaneRuntimeHost,
    mut panes: Vec<PaneRuntimeHandle>,
    wait_for_settlement: bool,
) -> std::result::Result<(), String> {
    let id = crate::ids::new_uuid();
    let (begin, admission) = oneshot::channel();
    let (settled, settlement) = oneshot::channel();
    let task_host = host.clone();
    let owner = switchy_async::task::spawn(async move {
        let host = &task_host;
        if admission.await.is_err() {
            return Ok(());
        }
        // Send every stop before waiting for any one pane. Each runtime task
        // freezes its checkpoint before signaling; Drop provides the same fence
        // if startup has not yet reached its stop receiver.
        for pane in &mut panes {
            pane.publication.decide(false);
            if let Some(stop) = pane.stop_tx.take() {
                let _ = stop.send(());
            }
        }
        // Freeze and revoke each lease before waiting on its task. In
        // particular, a failed Sweep must not leave its slave holder alive
        // while the cleanup task waits forever for reader EOF.
        // Startup still owns its child until readiness is published.
        // Do not race Start with Sweep or revoke its lease underneath startup's
        // acknowledged child cleanup. The stop above cancels startup, whose
        // task retains the child/reaper until cleanup actually settles.
        let lifetimes = panes
            .iter()
            .filter(|pane| matches!(*pane.startup.borrow(), Some(Ok(()))))
            .map(|pane| PaneLifetime {
                execution: Arc::clone(&pane.execution),
                resurrection: Arc::clone(&pane.resurrection_state),
            })
            .collect::<Vec<_>>();
        let mut revocations = Vec::with_capacity(lifetimes.len());
        for lifetime in lifetimes {
            let worker_host = host.clone();
            revocations.push(switchy_async::task::spawn_blocking(move || {
                let host = &worker_host;
                let frozen = lifetime
                    .resurrection
                    .lock()
                    .map_err(|_| anyhow::anyhow!("resurrection lock poisoned"))
                    .and_then(|mut state| state.freeze());
                if let Err(error) = frozen {
                    lifetime
                        .execution
                        .record_cleanup_error(format!("checkpoint freeze failed: {error}"));
                }
                // Prefer the acknowledged operation so the original target
                // outcome is retained. Lease EOF is the mandatory fallback,
                // even when that operation failed or remained uncertain.
                if let Err(error) = lifetime
                    .execution
                    .signal_owned_group(host, crate::platform::StopAction::Force)
                {
                    lifetime
                        .execution
                        .record_cleanup_error(format!("group stop failed: {error}"));
                }
                lifetime.revoke();
            }));
        }
        let mut settled = Some(settled);
        let mut errors = Vec::new();
        for revocation in revocations {
            if let Err(error) = revocation.await {
                errors.push(format!("pane lifetime revocation worker failed: {error}"));
            }
        }
        await_pane_cleanup(
            host,
            &mut panes,
            id,
            wait_for_settlement,
            &mut settled,
            &mut errors,
        )
        .await;
        // Lifetime guards are released only after all cleanup tasks settled.
        for pane in &panes {
            pane.lifetime.revoke();
        }
        drop(panes);
        host.pane_shutdown_owners()
            .lock()
            .expect("pane shutdown owners lock")
            .remove(&id);
        let result = if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        };
        if let Some(settled) = settled {
            let _ = settled.send(result.clone());
        }
        result
    });
    host.pane_shutdown_owners()
        .lock()
        .expect("pane shutdown owners lock")
        .insert(id, owner);
    let _ = begin.send(());
    settlement
        .await
        .map_err(|error| format!("pane cleanup owner failed: {error}"))?
}

struct SupervisionCompletion {
    done: Arc<AtomicBool>,
    execution: Arc<execution::ExecutionCapture>,
}

impl Drop for SupervisionCompletion {
    fn drop(&mut self) {
        if switchy_thread::thread::panicking() {
            self.execution
                .record_error("pane supervision thread panicked".into());
        }
        self.done.store(true, Ordering::SeqCst);
        self.execution.notify();
    }
}

/// A pane waiter's work, until its thread (or a startup failure) takes it.
type PaneWaiterWork = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;

fn supervision_join(
    threads: &mut [Option<switchy_thread::thread::JoinHandle<()>>],
) -> switchy_async::task::JoinHandle<std::result::Result<(), String>> {
    let handles: Vec<_> = threads.iter_mut().filter_map(Option::take).collect();
    // This owner retains every unreaped handle across the observer deadline.
    switchy_async::task::spawn_blocking(move || {
        let mut panicked = false;
        for thread in handles {
            panicked |= thread.join().is_err();
        }
        if panicked {
            Err("pane supervision thread panicked".into())
        } else {
            Ok(())
        }
    })
}

#[cfg(test)]
async fn join_supervision_threads(
    join: &mut switchy_async::task::JoinHandle<std::result::Result<(), String>>,
    timeout: Duration,
) -> std::result::Result<(), String> {
    switchy_async::time::timeout(timeout, join)
        .await
        .map_err(|_| "pane supervision threads did not exit before cleanup deadline".to_string())?
        .map_err(|error| format!("pane supervision join failed: {error}"))?
}

async fn stop_unpublished_pane(pane: &mut PaneRuntimeHandle) -> Result<()> {
    pane.publication.decide(false);
    if let Some(stop) = pane.stop_tx.take() {
        let _ = stop.send(());
    }
    switchy_async::time::timeout(Duration::from_secs(5), &mut pane.task)
        .await
        .map_err(|_| anyhow::anyhow!("unpublished pane cleanup timed out"))?
        .map_err(|error| anyhow::anyhow!("unpublished pane task cleanup failed: {error}"))?;
    pane.cleanup
        .borrow()
        .clone()
        .unwrap_or_else(|| Err("pane cleanup ended without process/thread acknowledgement".into()))
        .map_err(anyhow::Error::msg)
}

async fn shutdown_pane_handle(host: &crate::runtime::PaneRuntimeHost, pane: PaneRuntimeHandle) {
    shutdown_panes(host, vec![pane]).await;
}

fn process_pane_output(
    terminal_grid: &switchy_thread::sync::Mutex<TerminalGridStream>,
    output_buffer: &switchy_thread::sync::Mutex<OutputFanoutBuffer>,
    bytes: &[u8],
) -> Result<(), SessionRuntimeError> {
    process_pane_output_with_pointer_queries(
        terminal_grid,
        output_buffer,
        bytes,
        ProtocolProfile::Conservative,
    )
    .map(|_| ())
}

fn process_pane_output_with_pointer_queries(
    terminal_grid: &switchy_thread::sync::Mutex<TerminalGridStream>,
    output_buffer: &switchy_thread::sync::Mutex<OutputFanoutBuffer>,
    bytes: &[u8],
    profile: ProtocolProfile,
) -> Result<Vec<u8>, SessionRuntimeError> {
    use bmux_terminal_protocol::pointer_shape::{
        PointerShape, PointerShapeQueryContext, encode_pointer_shape_query_reply,
    };

    // All combined captures and writes acquire grid before output. The grid guard is held from
    // the parse through the push, so a snapshot (grid, then output) never pairs parsed state with
    // another chunk's offset; the output guard is taken only for the push, so a client reading
    // output does not wait for a parse. Only collect replies here: PTY writes must happen after
    // capture locks release.
    let mut grid = terminal_grid
        .lock()
        .map_err(|_| SessionRuntimeError::Closed)?;
    let mut replies = Vec::new();
    grid.process_with_pointer_queries(bytes, |query, current| {
        if matches!(profile, ProtocolProfile::Bmux) {
            // The grid has already bounded and validated the complete OSC payload.
            let _ = encode_pointer_shape_query_reply(
                query,
                PointerShapeQueryContext {
                    current,
                    default: PointerShape::Text,
                    grabbed: PointerShape::Default,
                },
                &mut replies,
            );
        }
    });
    output_buffer
        .lock()
        .map_err(|_| SessionRuntimeError::Closed)?
        .push_chunk(bytes);
    drop(grid);
    Ok(replies)
}

#[cfg(test)]
mod pointer_query_tests {
    use super::{
        GridLimits, OutputFanoutBuffer, ProtocolProfile, TerminalGridStream,
        process_pane_output_with_pointer_queries,
    };
    use switchy_thread::sync::Mutex;

    fn grid() -> Mutex<TerminalGridStream> {
        Mutex::new(TerminalGridStream::new(80, 24, GridLimits::default()).unwrap())
    }

    fn process(grid: &Mutex<TerminalGridStream>, bytes: &[u8]) -> Vec<u8> {
        let output = Mutex::new(OutputFanoutBuffer::new(4096));
        let reply =
            process_pane_output_with_pointer_queries(grid, &output, bytes, ProtocolProfile::Bmux)
                .unwrap();
        // Collection returns without retaining either capture lock.
        assert!(grid.try_lock().is_ok());
        assert!(output.try_lock().is_ok());
        reply
    }

    #[test]
    fn same_chunk_queries_observe_canonical_parser_order() {
        let grid = grid();
        assert_eq!(
            process(
                &grid,
                b"\x1b]22;pointer\x07\x1b]22;?__current__\x07\x1b]22;crosshair\x07\x1b]22;?__current__\x07",
            ),
            b"\x1b]22;pointer\x1b\\\x1b]22;crosshair\x1b\\",
        );
    }

    #[test]
    fn split_queries_and_setters_preserve_order() {
        let grid = grid();
        assert_eq!(process(&grid, b"\x1b]22;poi"), [] as [u8; 0]);
        assert_eq!(process(&grid, b"nter\x1b\\\x1b]22;?__cur"), [] as [u8; 0]);
        assert_eq!(process(&grid, b"rent__\x1b\\"), b"\x1b]22;pointer\x1b\\");
    }

    #[test]
    fn screens_reset_and_executions_are_isolated() {
        let first = grid();
        let second = grid();
        process(
            &first,
            b"\x1b]22;pointer\x07\x1b[?1049h\x1b]22;crosshair\x07",
        );
        assert_eq!(
            process(&first, b"\x1b]22;?__current__\x07"),
            b"\x1b]22;crosshair\x1b\\"
        );
        assert_eq!(
            process(&second, b"\x1b]22;?__current__\x07"),
            b"\x1b]22;0\x1b\\"
        );
        assert_eq!(
            process(&first, b"\x1b[?1049l\x1b]22;?__current__\x07"),
            b"\x1b]22;pointer\x1b\\"
        );
        assert_eq!(
            process(&first, b"\x1bc\x1b]22;?__current__\x07"),
            b"\x1b]22;0\x1b\\"
        );
        // A replacement starts with a fresh execution-scoped grid, not the old stack.
        let replacement = grid();
        assert_eq!(
            process(&replacement, b"\x1b]22;<\x07\x1b]22;?__current__\x07"),
            b"\x1b]22;0\x1b\\"
        );
    }

    #[test]
    fn snapshot_recovery_keeps_stack_and_partial_query_without_replying() {
        let original = grid();
        process(
            &original,
            b"\x1b]22;pointer\x07\x1b]22;>crosshair\x07\x1b]22;?__cur",
        );
        let snapshot = original.lock().unwrap().snapshot(0, 24);
        let restored = Mutex::new(
            TerminalGridStream::from_snapshot(&snapshot, GridLimits::default()).unwrap(),
        );
        assert_eq!(
            process(
                &restored,
                b"rent__\x07\x1b]22;<\x07\x1b]22;?__current__\x07"
            ),
            b"\x1b]22;crosshair\x1b\\\x1b]22;pointer\x1b\\"
        );
    }

    #[cfg(not(feature = "image-registry"))]
    #[test]
    fn pointer_queries_do_not_replace_cpr_or_color_replies() {
        use super::{PaneCursorTracker, TerminalProtocolEngine, protocol_reply_for_chunk};

        let chunk = b"\x1b[4;7H\x1b[6n\x1b]10;?\x07\x1b]22;pointer\x07\x1b]22;?__current__\x07";
        let grid = grid();
        let pointer = process(&grid, chunk);
        let mut tracker = PaneCursorTracker::new(24, 80);
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
        let mut replies = protocol_reply_for_chunk(
            &mut engine,
            &mut tracker.reader,
            &mut tracker.terminal_grid,
            chunk,
        );
        assert!(replies.starts_with(b"\x1b[4;7R"));
        assert!(replies.windows(5).any(|bytes| bytes == b"\x1b]10;"));
        replies.extend(pointer);
        assert!(replies.ends_with(b"\x1b]22;pointer\x1b\\"));
    }

    #[cfg(feature = "image-registry")]
    #[test]
    fn image_filtered_path_preserves_cpr_color_and_pointer_queries() {
        use super::{
            PaneCursorTracker, TerminalProtocolEngine, follow_image_chunk, image_collector,
        };

        let mut collector = image_collector();
        let mut tracker = PaneCursorTracker::new(24, 80);
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
        let mut registry = bmux_image::ImageRegistry::default();
        let mut replies = Vec::new();
        let events = follow_image_chunk(
            &mut tracker.reader,
            &mut tracker.terminal_grid,
            &mut engine,
            &mut registry,
            &mut collector,
            b"\x1bPq\"1;1;1;6#1;2;100;0;0~\x1b\\\x1b[4;7H\x1b[6n\x1b]10;?\x07\x1b]22;pointer\x07\x1b]22;?__current__\x07",
            &mut replies,
        )
        .unwrap();
        assert!(!events.is_empty());
        let grid = grid();
        let pointer = process(&grid, &tracker.reader.visible);
        assert!(replies.starts_with(b"\x1b[4;7R"));
        assert!(replies.windows(5).any(|bytes| bytes == b"\x1b]10;"));
        replies.extend(pointer);
        assert!(replies.ends_with(b"\x1b]22;pointer\x1b\\"));
    }

    /// A program probing for Kitty graphics (a query, then a cursor position report as a
    /// fence, as `kitty +kitten icat` does) is answered in order: the Kitty reply first.
    #[cfg(feature = "image-registry")]
    #[test]
    fn kitty_queries_are_answered_in_order_with_other_replies() {
        use super::{
            PaneCursorTracker, TerminalProtocolEngine, follow_image_chunk, image_collector,
        };

        let mut collector = image_collector();
        let mut tracker = PaneCursorTracker::new(24, 80);
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
        let mut registry = bmux_image::ImageRegistry::default();
        let mut replies = Vec::new();
        follow_image_chunk(
            &mut tracker.reader,
            &mut tracker.terminal_grid,
            &mut engine,
            &mut registry,
            &mut collector,
            b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[6n",
            &mut replies,
        )
        .unwrap();
        assert_eq!(replies, b"\x1b_Gi=31;OK\x1b\\\x1b[1;1R");
    }

    #[test]
    fn support_and_policy_queries_are_bmux_profile_only() {
        use bmux_terminal_protocol::pointer_shape::PointerShape;

        for profile in [
            ProtocolProfile::Bmux,
            ProtocolProfile::Xterm,
            ProtocolProfile::Screen,
            ProtocolProfile::Conservative,
        ] {
            let grid = grid();
            let reply = process_pane_output_with_pointer_queries(
                &grid,
                &Mutex::new(OutputFanoutBuffer::new(4096)),
                b"\x1b]22;pointer\x07\x1b]22;?pointer,unknown,__default__,__grabbed__\x07",
                profile,
            )
            .unwrap();
            if profile == ProtocolProfile::Bmux {
                assert_eq!(reply, b"\x1b]22;1,0,text,default\x1b\\");
            } else {
                assert_eq!(reply, [] as [u8; 0]);
            }
            assert_eq!(
                grid.lock()
                    .unwrap()
                    .grid()
                    .protocol_state()
                    .pointer_shapes
                    .current(false),
                Some(PointerShape::Pointer)
            );
        }
    }
}

/// Opt-in end-to-end latency log for `bmux-bench replay` (see
/// `docs/development/pane-output-performance.md`): with `BMUX_REPLAY_LOG` naming a file, each
/// replay marker read from a pane appends `S frame written_ns read_ns`, beside the attach client's
/// lines for when the frame reached the host.
/// Log the replay markers in `bytes` as read, when `BMUX_REPLAY_LOG` asks for it, and return
/// them (empty without the log), for the frames that carry them to clients.
fn note_replay_markers_read(bytes: &[u8]) -> Vec<(u64, u128)> {
    // A debug log file opened from the environment; a diagnostic sink shared by the
    // process's (or the simulated run's) activations, not per-instance state.
    enum ReplayLog {}
    let log = crate::run_shared::run_shared::<ReplayLog, _>(|| {
        let path = switchy_env::var_os("BMUX_REPLAY_LOG")?;
        switchy_fs::sync::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
            .map(switchy_thread::sync::Mutex::new)
    });
    let Some(file) = log.as_ref() else {
        return Vec::new();
    };
    let markers = bmux_bench::corpus::markers(bytes);
    if markers.is_empty() {
        return markers;
    }
    let read = switchy_time::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let mut lines = Vec::new();
    for (frame, written) in &markers {
        let _ = writeln!(lines, "S {frame} {written} {read}");
    }
    if let Ok(mut file) = file.lock() {
        let _ = file.write_all(&lines);
    }
    markers
}

fn push_pane_runtime_notice(
    host: &PaneRuntimeHost,
    session_id: SessionId,
    pane_id: Uuid,
    output_buffer: &Arc<switchy_thread::sync::Mutex<OutputFanoutBuffer>>,
    terminal_grid: &Arc<switchy_thread::sync::Mutex<TerminalGridStream>>,
    output_dirty: &Arc<AtomicBool>,
    message: impl AsRef<str>,
) {
    let bytes = message.as_ref().as_bytes();
    if process_pane_output(terminal_grid, output_buffer, bytes).is_err() {
        return;
    }
    output_dirty.store(true, Ordering::SeqCst);
    host.publish_pane_event(PaneEvent::OutputAvailable {
        session_id: session_id.0,
        pane_id,
    });
    crate::handlers::output_push::pane_output_available(host, session_id.0, pane_id);
}

struct PaneFailureTargets<'a> {
    process_id: &'a Arc<switchy_thread::sync::Mutex<Option<u32>>>,
    process_group_id: &'a Arc<switchy_thread::sync::Mutex<Option<i32>>>,
    exit_reason: &'a Arc<switchy_thread::sync::Mutex<Option<String>>>,
    exited: &'a Arc<AtomicBool>,
    output_buffer: &'a Arc<switchy_thread::sync::Mutex<OutputFanoutBuffer>>,
    terminal_grid: &'a Arc<switchy_thread::sync::Mutex<TerminalGridStream>>,
    output_dirty: &'a Arc<AtomicBool>,
    pane_exit_tx: &'a mpsc::UnboundedSender<PaneExitEvent>,
}

fn mark_pane_startup_failed(
    host: &PaneRuntimeHost,
    session_id: SessionId,
    pane_id: Uuid,
    reason: &str,
    targets: &PaneFailureTargets<'_>,
) {
    if let Ok(mut process_id) = targets.process_id.lock() {
        *process_id = None;
    }
    if let Ok(mut process_group_id) = targets.process_group_id.lock() {
        *process_group_id = None;
    }
    if let Ok(mut slot) = targets.exit_reason.lock() {
        *slot = Some(reason.to_string());
    }
    targets.exited.store(true, Ordering::SeqCst);
    push_pane_runtime_notice(
        host,
        session_id,
        pane_id,
        targets.output_buffer,
        targets.terminal_grid,
        targets.output_dirty,
        format!("\r\n[bmux] pane failed to start: {reason}\r\n"),
    );
    warn!(%pane_id, %reason, "pane failed to start");
    let _ = targets.pane_exit_tx.send(PaneExitEvent {
        session_id,
        pane_id,
        exited: Arc::clone(targets.exited),
    });
    host.mark_snapshot_dirty();
}

/// Hang up on the pane's leader unless it has already exited: its pid is
/// fenced before it is reaped, so a reused pid is never signalled.
fn kill_unreaped_leader(
    execution: &execution::ExecutionCapture,
    killer: &mut Box<dyn crate::runtime::pty::ChildKiller + Send + Sync>,
) {
    let Ok(session) = execution.session.lock() else {
        return;
    };
    if session
        .as_ref()
        .is_some_and(|control| control.leader_exited())
    {
        return;
    }
    let _ = killer.kill();
}

struct StartupChild {
    child: Option<Box<dyn crate::runtime::pty::Child + Send + Sync>>,
    runtime: switchy_async::runtime::Handle,
    host: PaneRuntimeHost,
}

impl StartupChild {
    fn new(
        host: &PaneRuntimeHost,
        child: Box<dyn crate::runtime::pty::Child + Send + Sync>,
    ) -> Self {
        Self {
            child: Some(child),
            runtime: switchy_async::runtime::Handle::current(),
            host: host.clone(),
        }
    }

    fn take(&mut self) -> Box<dyn crate::runtime::pty::Child + Send + Sync> {
        self.child
            .take()
            .expect("startup child ownership transferred once")
    }
}

impl std::ops::Deref for StartupChild {
    type Target = dyn crate::runtime::pty::Child + Send + Sync;
    fn deref(&self) -> &Self::Target {
        self.child.as_deref().expect("startup child is owned")
    }
}

impl std::ops::DerefMut for StartupChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.child.as_deref_mut().expect("startup child is owned")
    }
}

impl Drop for StartupChild {
    fn drop(&mut self) {
        let host = &self.host;
        if let Some(child) = self.child.take() {
            // The startup task may be aborted while awaiting native metadata,
            // or its waiter-thread closure may be dropped on thread failure.
            drop(cleanup_failed_startup_child_on(
                host,
                &self.runtime,
                child,
                "startup child owner dropped".into(),
                None,
            ));
        }
    }
}

#[cfg(all(test, unix))]
fn cleanup_failed_startup_child(
    host: &crate::runtime::PaneRuntimeHost,
    child: Box<dyn crate::runtime::pty::Child + Send + Sync>,
    failure: String,
) -> impl std::future::Future<Output = String> {
    let cleanup = cleanup_failed_startup_child_on(
        host,
        &switchy_async::runtime::Handle::current(),
        child,
        failure,
        None,
    );
    async move { cleanup.await.0 }
}

fn cleanup_failed_startup_child_acknowledged(
    host: &crate::runtime::PaneRuntimeHost,
    child: Box<dyn crate::runtime::pty::Child + Send + Sync>,
    failure: String,
    acknowledgement: &watch::Sender<Option<std::result::Result<(), String>>>,
) -> impl std::future::Future<Output = String> + use<> {
    let cleanup = cleanup_failed_startup_child_on(
        host,
        &switchy_async::runtime::Handle::current(),
        child,
        failure,
        Some(acknowledgement.clone()),
    );
    async move { cleanup.await.0 }
}

fn cleanup_failed_startup_child_on(
    host: &crate::runtime::PaneRuntimeHost,
    runtime: &switchy_async::runtime::Handle,
    mut child: Box<dyn crate::runtime::pty::Child + Send + Sync>,
    failure: String,
    acknowledgement: Option<watch::Sender<Option<std::result::Result<(), String>>>>,
) -> impl std::future::Future<Output = (String, Result<(), String>)> + use<> {
    // Transfer ownership before returning the waiter. Dropping the caller's
    // future must not drop the only reaper for this newly spawned process.
    let fallback = failure.clone();
    let host = host.clone();
    let cleanup = runtime.spawn(async move {
        let host = &host;
        let reaper = switchy_async::task::spawn_blocking(move || {
            let kill_error = child.kill().err();
            (child.wait(), kill_error)
        });
        let result = match reaper.await {
            Ok((Ok(_), None)) => Ok(()),
            Ok((Ok(_), Some(error))) => Err(format!("kill failed: {error}")),
            Ok((Err(error), kill_error)) => {
                Err(format!("wait failed: {error}; kill error: {kill_error:?}"))
            }
            Err(error) => Err(format!("child reaper failed: {error}")),
        };
        if let Err(error) = host.shutdown_progress().advance("startup child reaped") {
            warn!(%error, "shutdown progress publication failed");
        }
        if let Some(acknowledgement) = acknowledgement {
            acknowledgement.send_replace(Some(result.clone()));
        }
        match result {
            Ok(()) => (failure, Ok(())),
            Err(error) => (
                format!("{failure}; child cleanup failed: {error}"),
                Err(error),
            ),
        }
    });
    async move {
        match cleanup.await {
            Ok(reason) => reason,
            Err(error) => (
                format!("{fallback}; child cleanup task failed: {error}"),
                Err(error.to_string()),
            ),
        }
    }
}

#[cfg(test)]
async fn poll_failed_startup_child(
    mut poll: impl FnMut() -> std::io::Result<bool>,
    kill_error: Option<std::io::Error>,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = switchy_async::time::Instant::now() + timeout;
    loop {
        match poll() {
            Ok(true) => return Ok(()),
            Err(error) => return Err(format!("wait failed: {error}; kill error: {kill_error:?}")),
            Ok(false) => {}
        }
        if switchy_async::time::Instant::now() >= deadline {
            return Err(format!("timed out; kill error: {kill_error:?}"));
        }
        switchy_async::time::sleep(Duration::from_millis(10)).await;
    }
}

fn format_pane_exit_reason(status: &bmux_pty::ChildExit) -> String {
    if let Some(signal) = status.signal() {
        return format!("process terminated by signal {signal}");
    }
    format!("process exited with status {}", status.exit_code())
}

struct PanePaddingPreviewTransaction {
    owner_client_id: ClientId,
    target_pane_ids: BTreeSet<Uuid>,
    spec: PanePaddingSpec,
    lease_expires_at: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PanePaddingOverridePersistence {
    Runtime,
    Snapshot,
}

struct SessionRuntimeManager {
    runtimes: BTreeMap<SessionId, SessionRuntimeHandle>,
    pane_input_index: Arc<RwLock<BTreeMap<Uuid, PaneInputHandle>>>,
    client_write_permissions: Arc<RwLock<BTreeMap<SessionId, BTreeSet<ClientId>>>>,
    retained_output_bytes: Arc<AtomicUsize>,
    retained_pin_bytes: Arc<AtomicUsize>,
    shell: String,
    pane_term: String,
    protocol_profile: ProtocolProfile,
    bracketed_paste_supported: bool,
    shell_integration_root: Option<std::path::PathBuf>,
    padding_config: PanePaddingConfig,
    restored_padding_overrides: BTreeMap<(SessionId, Uuid), PanePaddingSpec>,
    padding_previews: BTreeMap<Uuid, PanePaddingPreviewTransaction>,
    // Coalesced invalidations captured under the manager lock and published after unlock.
    pending_layout_revisions: BTreeMap<SessionId, u64>,
    pane_exit_tx: mpsc::UnboundedSender<PaneExitEvent>,
    host: PaneRuntimeHost,
}

#[derive(Clone)]
struct PaneInputHandle {
    /// Execution identity is fixed before this live entry is published.
    execution_token: Option<Uuid>,
    session_id: SessionId,
    input_tx: mpsc::UnboundedSender<PaneRuntimeCommand>,
    exited: Arc<AtomicBool>,
    host: PaneRuntimeHost,
}

/// A fresh runtime's OS threads may start before its manager entry exists.
/// Hold their externally visible effects until commit; abandonment wakes them
/// without publishing anything. Legacy/restored runtimes start committed.
#[derive(Default)]
struct StartupPublication {
    decision: Mutex<Option<bool>>,
    changed: switchy_thread::sync::Condvar,
}

impl StartupPublication {
    fn decide(&self, committed: bool) {
        let mut decision = self
            .decision
            .lock()
            .unwrap_or_else(switchy_thread::sync::PoisonError::into_inner);
        if decision.is_none() {
            *decision = Some(committed);
            self.changed.notify_all();
        }
    }

    fn wait(&self) -> bool {
        let decision = self
            .decision
            .lock()
            .unwrap_or_else(switchy_thread::sync::PoisonError::into_inner);
        let decision = self
            .changed
            .wait_while(decision, |decision| decision.is_none())
            .unwrap_or_else(switchy_thread::sync::PoisonError::into_inner);
        *decision == Some(true)
    }
}

struct PendingPublicationGuard(Arc<StartupPublication>);

impl Drop for PendingPublicationGuard {
    fn drop(&mut self) {
        self.0.decide(false);
    }
}

struct PreparedCleanup {
    _admission: managed_idle::Admission,
    runtime: switchy_async::runtime::Handle,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl PreparedCleanup {
    fn reserve(host: &PaneRuntimeHost) -> Result<Arc<Self>> {
        // Process-wide bound on concurrent cleanup tasks: they hold OS
        // resources (threads, PTYs) shared by every activation in this process.
        enum CleanupSlots {}
        Self::reserve_from(
            host,
            crate::run_shared::run_shared::<CleanupSlots, _>(|| tokio::sync::Semaphore::new(64)),
        )
    }

    fn reserve_from(
        host: &PaneRuntimeHost,
        slots: Arc<tokio::sync::Semaphore>,
    ) -> Result<Arc<Self>> {
        let runtime = switchy_async::runtime::Handle::try_current()?;
        let permit = slots.try_acquire_owned().map_err(|_| {
            spawn_failure(bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SpawnError::Busy)
        })?;
        Ok(Arc::new(Self {
            _admission: managed_idle::admit(host)?,
            runtime,
            _permit: permit,
        }))
    }
}

struct PreparedSplit {
    cleanup: Arc<PreparedCleanup>,
    pane: Option<PaneRuntimeHandle>,
    launch: Option<oneshot::Sender<()>>,
    target: Uuid,
    target_execution: Uuid,
}

fn supervise_unpublished_cleanup(mut pane: PaneRuntimeHandle, cleanup: Arc<PreparedCleanup>) {
    pane.publication.decide(false);
    if let Some(stop) = pane.stop_tx.take() {
        let _ = stop.send(());
    }
    if pane.task.is_finished() {
        if !matches!(&*pane.cleanup.borrow(), Some(Ok(()))) {
            warn!(pane_id = %pane.meta.id, cleanup = ?*pane.cleanup.borrow(), "unpublished cleanup did not acknowledge success");
        }
        return;
    }
    // The reservation captures the preparation runtime and holds admission
    // through late cleanup, even if the request owner is dropped elsewhere.
    let runtime = cleanup.runtime.clone();
    runtime.spawn(async move {
        let _cleanup = cleanup;
        if let Err(error) = stop_unpublished_pane(&mut pane).await {
            warn!(pane_id = %pane.meta.id, %error, "unpublished cleanup requires recovery");
            // A deadline is not permission to forget a still-running teardown.
            // Keep ownership until it settles; never report timeout as success.
            if !pane.task.is_finished() {
                let result = (&mut pane.task).await;
                warn!(pane_id = %pane.meta.id, ?result, cleanup = ?*pane.cleanup.borrow(), "late unpublished cleanup settled");
            }
        }
    });
}

impl Drop for PreparedSplit {
    fn drop(&mut self) {
        if let Some(pane) = self.pane.take() {
            supervise_unpublished_cleanup(pane, Arc::clone(&self.cleanup));
        }
    }
}

enum ExecutionExitNotice {
    OwnerExited,
}

struct PreparedFreshRuntime {
    cleanup: Arc<PreparedCleanup>,
    runtime: Option<SessionRuntimeHandle>,
    launch: Option<oneshot::Sender<()>>,
}

impl PreparedFreshRuntime {
    fn launch(&mut self) -> Result<()> {
        if let Some(launch) = self.launch.take() {
            launch.send(()).map_err(|()| {
                anyhow::anyhow!("prepared runtime startup task exited before launch")
            })?;
        }
        Ok(())
    }

    async fn wait_until_ready(&mut self) -> Result<()> {
        self.launch()?;
        for pane in self
            .runtime
            .as_ref()
            .expect("prepared runtime still owns startup")
            .panes
            .values()
        {
            let mut startup = pane.startup.clone();
            loop {
                if let Some(result) = startup.borrow_and_update().clone() {
                    result.map_err(anyhow::Error::msg)?;
                    break;
                }
                startup
                    .changed()
                    .await
                    .map_err(|_| anyhow::anyhow!("pane startup ended without readiness"))?;
            }
            anyhow::ensure!(
                !pane.exited.load(Ordering::SeqCst) || pane.meta.launch.is_some(),
                "pane exited before startup commit"
            );
        }
        Ok(())
    }

    #[cfg(test)]
    fn take(mut self) -> Result<SessionRuntimeHandle> {
        self.launch()?;
        Ok(self
            .runtime
            .take()
            .expect("prepared runtime transferred once"))
    }
}

impl Drop for PreparedFreshRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            for pane in runtime.panes.into_values() {
                supervise_unpublished_cleanup(pane, Arc::clone(&self.cleanup));
            }
        }
    }
}

struct SessionRuntimeHandle {
    panes: BTreeMap<Uuid, PaneRuntimeHandle>,
    layout_root: Option<PaneLayoutNode>,
    focused_pane_id: Uuid,
    zoomed_pane_id: Option<Uuid>,
    floating_surfaces: Vec<FloatingSurfaceRuntime>,
    attached_clients: BTreeSet<ClientId>,
    scrollback_pins: BTreeMap<(ClientId, Uuid), ScrollbackPin>,
    next_scrollback_pin_id: u64,
    attach_viewport: Option<AttachViewport>,
    attach_view_revision: u64,
}

struct PinReservation {
    used: Arc<AtomicUsize>,
    bytes: usize,
}

impl PinReservation {
    fn shrink(&mut self, bytes: usize) -> Result<(), SessionRuntimeError> {
        let released = self
            .bytes
            .checked_sub(bytes)
            .ok_or(SessionRuntimeError::ResponseBudgetExceeded)?;
        self.used.fetch_sub(released, Ordering::AcqRel);
        self.bytes = bytes;
        Ok(())
    }

    fn acquire(used: &Arc<AtomicUsize>, bytes: usize) -> Result<Self, SessionRuntimeError> {
        used.try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current
                .checked_add(bytes)
                .filter(|&next| next <= RETAINED_PIN_BUDGET)
        })
        .map_err(|_| SessionRuntimeError::ResponseBudgetExceeded)?;
        Ok(Self {
            used: Arc::clone(used),
            bytes,
        })
    }
}

impl Drop for PinReservation {
    fn drop(&mut self) {
        self.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

struct CapturedPinGrid {
    grid: TerminalGrid,
    #[cfg(feature = "image-registry")]
    images: Option<bmux_image::registry::ImageHistorySnapshot>,
    // Drop the grid before releasing its charge, including for retired readers.
    reservation: PinReservation,
}

#[cfg(not(feature = "image-registry"))]
impl CapturedPinGrid {
    fn new(grid: TerminalGrid, reservation: PinReservation) -> Result<Self, SessionRuntimeError> {
        let mut captured = Self {
            grid,
            reservation,
            #[cfg(feature = "image-registry")]
            images: None,
        };
        captured.reservation.shrink(
            captured
                .grid
                .retained_capacity_bytes()
                .ok_or(SessionRuntimeError::ResponseBudgetExceeded)?,
        )?;
        Ok(captured)
    }
}

#[derive(Clone)]
struct ScrollbackPin {
    capture_id: Uuid,
    id: u64,
    // Backing content and its charge share one lifetime; neither can escape alone.
    grid: Arc<CapturedPinGrid>,
    total_scrolled_rows: u64,
    stream_end: u64,
    encoded_bytes: usize,
}

struct EncodedByteCount(usize, usize);

impl Default for EncodedByteCount {
    fn default() -> Self {
        Self(0, usize::MAX)
    }
}

impl std::io::Write for EncodedByteCount {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let next = self
            .0
            .checked_add(bytes.len())
            .filter(|size| *size <= self.1)
            .ok_or_else(|| std::io::Error::other("encoded byte budget exceeded"))?;
        self.0 = next;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One pane's output and the offset its snapshot reached.
type SnapshotCursor = (
    Uuid,
    Arc<switchy_thread::sync::Mutex<OutputFanoutBuffer>>,
    u64,
);

fn acknowledge_grid_snapshots(
    client_id: ClientId,
    updates: &mut [SnapshotCursor],
) -> Result<(), SessionRuntimeError> {
    // Concurrent multi-pane snapshots must acquire locks in the same order: their panes'
    // (a deterministic order in simulation, unlike addresses).
    updates.sort_by_key(|(pane_id, _, _)| *pane_id);
    // Locking the same non-reentrant mutex twice would deadlock. Reject an
    // aliased batch before acquiring locks or acknowledging any output.
    if updates.iter().enumerate().any(|(index, (_, output, _))| {
        updates[index + 1..]
            .iter()
            .any(|(_, other, _)| Arc::ptr_eq(output, other))
    }) {
        return Err(SessionRuntimeError::Closed);
    }
    let mut guards = Vec::with_capacity(updates.len());
    for (_, output, offset) in updates.iter() {
        guards.push((
            output.lock().map_err(|_| SessionRuntimeError::Closed)?,
            *offset,
        ));
    }
    // Acquire every lock before mutating any cursor: poison must not commit
    // only a prefix of a response that will never reach the caller.
    // Retain every guard until all updates are applied, so readers cannot
    // observe a committed cursor while another cursor is still unchanged.
    for (output, offset) in &mut guards {
        output.acknowledge_snapshot(client_id, *offset);
    }
    Ok(())
}

fn warn_grid_snapshot_omitted(pane_id: Uuid, remaining: usize) {
    tracing::warn!(
        %pane_id,
        remaining_bytes = remaining,
        wire_budget_bytes = REPLY_WIRE_BUDGET,
        "grid snapshot omitted: pane exceeds remaining reply budget"
    );
}

fn encode_grid_window(
    snapshot: &bmux_terminal_grid::GridSnapshot,
    remaining: &mut usize,
) -> Result<Vec<u8>, SessionRuntimeError> {
    let mut count = EncodedByteCount(0, *remaining);
    serde_json::to_writer(&mut count, snapshot)
        .map_err(|_| SessionRuntimeError::ResponseBudgetExceeded)?;
    // The preflight gives the exact size. Avoid geometric Vec growth, which
    // can reserve substantially more memory than the accepted payload budget.
    let mut encoded = Vec::new();
    encoded
        .try_reserve_exact(count.0)
        .map_err(|_| SessionRuntimeError::Closed)?;
    encoded.resize(count.0, 0);
    let mut output = encoded.as_mut_slice();
    serde_json::to_writer(&mut output, snapshot).map_err(|_| SessionRuntimeError::Closed)?;
    if !output.is_empty() {
        return Err(SessionRuntimeError::Closed);
    }
    *remaining = remaining
        .checked_sub(encoded.len())
        .ok_or(SessionRuntimeError::ResponseBudgetExceeded)?;
    Ok(encoded)
}

#[derive(Debug, Clone)]
struct PaneResurrectionRuntime {
    frozen: bool,
    freeze_error: Option<String>,
    launch: Option<crate::application_resurrection::Launch>,
    /// Ephemeral process incarnation; never persisted or reused on restart.
    observation_execution: Uuid,
    /// Changes on every authenticated shell event, including repeated prompts.
    metadata_revision: u64,
    /// Only metadata received from this incarnation, never restored CWD.
    observed_cwd: Option<String>,
    execution_token: Option<String>,
    active_command: Option<String>,
    active_command_source: Option<PaneCommandSource>,
    last_known_cwd: Option<String>,
}

impl Default for PaneResurrectionRuntime {
    fn default() -> Self {
        Self::from_snapshot(&PaneResurrectionSnapshot::default())
    }
}

impl PaneResurrectionRuntime {
    fn from_snapshot(snapshot: &PaneResurrectionSnapshot) -> Self {
        Self {
            frozen: false,
            freeze_error: None,
            launch: None,
            observation_execution: crate::ids::new_uuid(),
            metadata_revision: 0,
            observed_cwd: None,
            execution_token: snapshot.execution_token.clone(),
            active_command: snapshot.active_command.clone(),
            active_command_source: snapshot.active_command_source,
            last_known_cwd: snapshot.last_known_cwd.clone(),
        }
    }

    fn freeze(&mut self) -> anyhow::Result<()> {
        self.freeze_with(|launch| {
            launch.map_or(Ok(()), |launch| {
                launch.close_checked().map_err(anyhow::Error::msg)
            })
        })
    }

    fn freeze_with(
        &mut self,
        close: impl FnOnce(Option<&crate::application_resurrection::Launch>) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        // In-memory fencing is unconditional: a failed durable close must not
        // allow final prompt events to erase the invocation we are preserving.
        // Keep one close attempt and its result: a stale-endpoint error from a
        // repeated close must not replace success, nor may a repeated freeze
        // hide an earlier durable failure. This lock also orders metadata
        // boundary commits before close; no manager lock is held by callers.
        if self.frozen {
            return self
                .freeze_error
                .clone()
                .map_or(Ok(()), |error| Err(anyhow::Error::msg(error)));
        }
        self.frozen = true;
        if let Err(error) = close(self.launch.as_ref()) {
            self.freeze_error = Some(error.to_string());
            return Err(error);
        }
        self.launch = None;
        Ok(())
    }

    fn close_launch(&mut self) {
        if let Some(launch) = self.launch.take() {
            launch.close();
        }
    }

    fn to_snapshot(&self) -> PaneResurrectionSnapshot {
        PaneResurrectionSnapshot {
            execution_token: self.execution_token.clone(),
            active_command: self.active_command.clone(),
            active_command_source: self.active_command_source,
            last_known_cwd: self.last_known_cwd.clone(),
        }
    }

    fn inspection_version(&self) -> (Uuid, u64, Option<String>) {
        (
            self.observation_execution,
            self.metadata_revision,
            self.execution_token.clone(),
        )
    }

    fn apply_inspection(
        &mut self,
        version: &(Uuid, u64, Option<String>),
        inspection: Option<ProcessInspectionResult>,
    ) {
        // A probe is only a best-effort projection. Never replace a newer
        // invocation, prompt, command, or frozen final snapshot with its result.
        if self.frozen
            || self.inspection_version() != *version
            || self.active_command_source == Some(PaneCommandSource::Verbatim)
        {
            return;
        }
        let Some(inspection) = inspection else {
            // Unavailable/timeout is not evidence that a command reached prompt.
            return;
        };
        self.metadata_revision = self.metadata_revision.wrapping_add(1);
        if let Some(command) = inspection.command {
            self.active_command = Some(command);
            self.active_command_source = Some(PaneCommandSource::Inspection);
        } else if self.active_command_source == Some(PaneCommandSource::Inspection) {
            self.active_command = None;
            self.active_command_source = None;
        }
        if let Some(cwd) = inspection.cwd {
            self.last_known_cwd = Some(cwd);
        }
    }

    fn apply_event(&mut self, event: PaneShellMetadataEvent) {
        if self.frozen {
            return;
        }
        self.metadata_revision = self.metadata_revision.wrapping_add(1);
        // Shell-marker authentication already scopes these events to this PTY.
        let (PaneShellMetadataEvent::CommandStart { cwd, .. }
        | PaneShellMetadataEvent::Prompt { cwd }) = &event
        else {
            return;
        };
        self.observed_cwd = Some(cwd.clone());
        trace!(execution = %self.observation_execution, "observed current execution directory");
        match event {
            PaneShellMetadataEvent::CommandStart { command, cwd } => {
                self.active_command = Some(command);
                self.active_command_source = Some(PaneCommandSource::Verbatim);
                self.last_known_cwd = Some(cwd);
            }
            PaneShellMetadataEvent::Prompt { cwd } => {
                self.last_known_cwd = Some(cwd);
                self.active_command = None;
                self.active_command_source = None;
            }
            PaneShellMetadataEvent::NestedReplay | PaneShellMetadataEvent::AgentReport { .. } => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PaneShellMetadataEvent {
    CommandStart {
        command: String,
        cwd: String,
    },
    Prompt {
        cwd: String,
    },
    NestedReplay,
    /// A program holding the pane's marker reported its own state; published
    /// as a terminal event and never applied to the shell's metadata.
    AgentReport {
        state: String,
        agent: Option<String>,
        message: Option<String>,
    },
}

fn apply_shell_metadata_events_and_take_prompt_replay(
    state: &mut PaneResurrectionRuntime,
    pending_replay_command: &mut Option<String>,
    events: impl IntoIterator<Item = PaneShellMetadataEvent>,
) -> Option<String> {
    let mut replay_command = None;
    for event in events {
        if matches!(event, PaneShellMetadataEvent::NestedReplay) {
            if !state.frozen
                && replay_command.is_none()
                && let Some(launch) = &state.launch
            {
                match launch.take_replay() {
                    Ok(command) => replay_command = command,
                    Err(error) => warn!(%error, "nested replay unavailable"),
                }
            }
            continue;
        }
        let is_prompt = matches!(event, PaneShellMetadataEvent::Prompt { .. });
        state.apply_event(event);
        if is_prompt
            && !state.frozen
            && replay_command.is_none()
            && let Some(command) = pending_replay_command.take()
        {
            state.active_command = Some(command.clone());
            state.active_command_source = Some(PaneCommandSource::Verbatim);
            replay_command = Some(command);
        }
    }
    replay_command
}

#[derive(Debug, PartialEq, Eq)]
enum SpawnDirectoryProbe {
    Shell(String),
    Process(u32, ProcessStartIdentity),
}

struct PaneLifetime {
    execution: Arc<execution::ExecutionCapture>,
    resurrection: Arc<switchy_thread::sync::Mutex<PaneResurrectionRuntime>>,
}

impl PaneLifetime {
    fn revoke(&self) {
        // Handle drop is also a lifetime boundary, not just explicit shutdown.
        // Fence checkpoint writers before any resulting shell prompt can mutate
        // the invocation needed by the last durable restoration snapshot.
        let frozen = self
            .resurrection
            .lock()
            .map_err(|_| anyhow::anyhow!("resurrection lock poisoned"))
            .and_then(|mut resurrection| resurrection.freeze());
        if let Err(error) = frozen {
            self.execution.record_cleanup_error(format!(
                "failed freezing resurrection before pane lifetime end: {error}"
            ));
            warn!(%error, "failed freezing resurrection before pane lifetime end");
        }
        if let Err(error) = self.execution.release_session() {
            self.execution
                .record_cleanup_error(format!("failed revoking pane execution lifetime: {error}"));
            warn!(%error, "failed revoking pane execution lifetime");
        }
    }
}

impl Drop for PaneLifetime {
    fn drop(&mut self) {
        self.revoke();
    }
}

struct PaneRuntimeHandle {
    host: PaneRuntimeHost,
    lifetime: PaneLifetime,
    execution: Arc<execution::ExecutionCapture>,
    publication: Arc<StartupPublication>,
    cleanup: watch::Receiver<Option<std::result::Result<(), String>>>,
    startup: watch::Receiver<Option<std::result::Result<(), String>>>,
    session_id: SessionId,
    meta: PaneRuntimeMeta,
    process_start_identity: Arc<switchy_thread::sync::Mutex<Option<ProcessStartIdentity>>>,
    process_id: Arc<switchy_thread::sync::Mutex<Option<u32>>>,
    process_group_id: Arc<switchy_thread::sync::Mutex<Option<i32>>>,
    resurrection_state: Arc<switchy_thread::sync::Mutex<PaneResurrectionRuntime>>,
    exit_reason: Arc<switchy_thread::sync::Mutex<Option<String>>>,
    stop_tx: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
    input_tx: mpsc::UnboundedSender<PaneRuntimeCommand>,
    output_buffer: Arc<switchy_thread::sync::Mutex<OutputFanoutBuffer>>,
    terminal_grid: Arc<switchy_thread::sync::Mutex<TerminalGridStream>>,
    /// What the pane's program last signalled beyond its screen.
    signals: Arc<switchy_thread::sync::Mutex<RetainedPaneSignals>>,
    /// Each attached client's replica of the grid: what it was last sent frames from.
    frame_replicas: Arc<switchy_thread::sync::Mutex<BTreeMap<ClientId, PaneFrameReplica>>>,
    exited: Arc<AtomicBool>,
    last_requested_size: Arc<switchy_thread::sync::Mutex<(u16, u16)>>,
    /// Set to `true` by the PTY reader when new output arrives. The broadcast
    /// event is only emitted on the `false→true` transition, coalescing
    /// thousands of per-chunk writes into ~1 event per fetch cycle.
    output_dirty: Arc<AtomicBool>,
    mouse_protocol_state: Arc<switchy_thread::sync::Mutex<AttachMouseProtocolState>>,
    input_mode_state: Arc<switchy_thread::sync::Mutex<AttachInputModeState>>,
    padding_override: Option<PanePaddingSpec>,
    padding_preview: Option<PanePaddingSpec>,
    padding_override_persistence: PanePaddingOverridePersistence,
    #[cfg(feature = "image-registry")]
    image_registry: Arc<switchy_thread::sync::Mutex<bmux_image::ImageRegistry>>,
    /// Cell pixel dimensions reported by the client (width, height).
    #[cfg(feature = "image-registry")]
    cell_pixel_size: Arc<switchy_thread::sync::Mutex<(u16, u16)>>,
    /// Set to `true` when the image registry has new content.
    #[cfg(feature = "image-registry")]
    image_dirty: Arc<AtomicBool>,
}

pub(crate) enum PaneRuntimeCommand {
    AdmittedInput(
        Vec<u8>,
        switchy_thread::sync::mpsc::SyncSender<Result<(), String>>,
        tokio::sync::OwnedSemaphorePermit,
        input_authority::InputAuthority,
    ),
    GuardedResize {
        rows: u16,
        cols: u16,
        _permit: tokio::sync::OwnedSemaphorePermit,
    },
    AuthorizedInput(Vec<u8>, input_authority::InputAuthority),
    /// A producer fence must be ordered behind every already-admitted PTY write.
    InputFence(switchy_thread::sync::mpsc::SyncSender<()>),
    Input(Vec<u8>),
    Resize {
        rows: u16,
        cols: u16,
    },
}

/// A pane's mouse protocol, as input routing encodes mouse events for it, from its grid's modes:
/// the most inclusive tracking mode enabled, and SGR before UTF-8 encoding.
const fn attach_mouse_protocol(modes: ProtocolState) -> AttachMouseProtocolState {
    let mode = if modes.mouse_any_motion {
        AttachMouseProtocolMode::AnyMotion
    } else if modes.mouse_button_motion {
        AttachMouseProtocolMode::ButtonMotion
    } else if modes.mouse_press_release {
        AttachMouseProtocolMode::PressRelease
    } else if modes.mouse_x10 {
        AttachMouseProtocolMode::Press
    } else {
        AttachMouseProtocolMode::None
    };
    let encoding = if modes.mouse_sgr {
        AttachMouseProtocolEncoding::Sgr
    } else if modes.mouse_utf8 {
        AttachMouseProtocolEncoding::Utf8
    } else {
        AttachMouseProtocolEncoding::Default
    };
    AttachMouseProtocolState { mode, encoding }
}

/// A pane's cursor key, keypad and paste modes, from its grid's modes.
const fn attach_input_modes(modes: ProtocolState) -> AttachInputModeState {
    AttachInputModeState {
        application_cursor: modes.application_cursor,
        application_keypad: modes.application_keypad,
        bracketed_paste: modes.bracketed_paste,
    }
}

/// The output filters native plugins register for every pane (see
/// [`bmux_terminal_grid::SequenceFilters`]), shared plugin state created by whichever side asks
/// first.
pub(crate) fn pane_output_filters(states: &bmux_plugin::PluginStateRegistry) -> SharedFilters {
    states.get_or_insert_with(bmux_terminal_grid::SequenceFilters::default)
}

/// What the pane reader keeps from chunk to chunk while it follows the pane's output through the
/// pane's grid: the shell metadata filter, the rows already scrolled out of image positions, and
/// the buffers one chunk's visible bytes, metadata events and pointer-shape replies collect in.
/// The grid itself is the pane's one grid: cursor positions for image placement and cursor
/// reports are read from it as of each byte.
struct PaneReader {
    metadata: Option<PaneShellMetadataParser>,
    metadata_events: Vec<PaneShellMetadataEvent>,
    /// The chunk's bytes as the grid sees them (images and shell metadata removed): what the
    /// output buffer publishes.
    visible: Vec<u8>,
    /// The start of a sequence that might yet begin an image, held from earlier chunks until what
    /// follows decides whether it is output ([`SequenceObserver::hold`]).
    held: Vec<u8>,
    pointer_replies: Vec<u8>,
    /// What the chunk's program signalled beyond the screen (title, notifications, ...).
    signals: Vec<PaneSignal>,
    profile: ProtocolProfile,
    /// The output filters plugins registered ([`pane_output_filters`]), consulted as the grid
    /// parses each sequence.
    filters: Option<SharedFilters>,
    /// The grid's viewport scroll revision already applied to image positions, from the first
    /// bytes followed (a grid restored from a snapshot does not start at zero).
    #[cfg(feature = "image-registry")]
    scrolled: Option<u64>,
}

impl PaneReader {
    const fn new(profile: ProtocolProfile, metadata: Option<PaneShellMetadataParser>) -> Self {
        Self {
            metadata,
            metadata_events: Vec::new(),
            visible: Vec::new(),
            held: Vec::new(),
            pointer_replies: Vec::new(),
            signals: Vec::new(),
            profile,
            filters: None,
            #[cfg(feature = "image-registry")]
            scrolled: None,
        }
    }

    /// Consult `filters` for every sequence the pane's program writes.
    fn with_filters(mut self, filters: SharedFilters) -> Self {
        self.filters = Some(filters);
        self
    }

    /// Start a chunk: its visible bytes, metadata events and pointer replies start empty.
    fn begin_chunk(&mut self) {
        self.visible.clear();
        self.metadata_events.clear();
        self.pointer_replies.clear();
        self.signals.clear();
    }

    /// Parse `bytes` into `grid`, answering the queries among them as the grid reaches each:
    /// the protocol engine's into `replies`, pointer-shape queries into the reader's own.
    /// `filters` are the registered output filters, read once per chunk (`read_filters`).
    /// `images` follows each operation the grid applies (image positions move with it).
    fn feed<I: ReaderImages>(
        &mut self,
        grid: &mut TerminalGridStream,
        engine: &mut TerminalProtocolEngine,
        bytes: &[u8],
        replies: &mut Vec<u8>,
        filters: Option<&bmux_terminal_grid::SequenceFilters>,
        images: &mut I,
    ) {
        let observer = ReaderObserver {
            engine,
            replies,
            pointer_replies: &mut self.pointer_replies,
            signals: &mut self.signals,
            profile: self.profile,
            filters: NoFilters,
            images,
            bytes,
            visible: &mut self.visible,
            held: &mut self.held,
            cursor: 0,
            holding: false,
        };
        match filters {
            None => observer.follow(grid),
            Some(filters) => ReaderObserver {
                engine: observer.engine,
                replies: observer.replies,
                pointer_replies: observer.pointer_replies,
                signals: observer.signals,
                profile: observer.profile,
                filters,
                images: observer.images,
                bytes,
                visible: observer.visible,
                held: observer.held,
                cursor: 0,
                holding: false,
            }
            .follow(grid),
        }
    }
}

/// How the pane reader follows images, statically: [`NoImages`] recognises none and compiles to
/// nothing.
trait ReaderImages {
    /// Whether the grid's parse streams image strings (Sixel, Kitty graphics, iTerm2) here.
    const STREAMS: bool = false;
    /// The grid applied an operation: images on screen move with it.
    fn operation(&mut self, grid: &TerminalGrid);
    /// Whether the DCS string `introducer` begins is a Sixel image.
    fn stream_dcs(&mut self, _introducer: &SequenceHeader<'_>) -> bool {
        false
    }
    fn string_start(&mut self, _kind: StreamedString) {}
    fn string_data(&mut self, _data: &[u8]) {}
    /// An image string ended: a complete image is placed at the cursor, its replies in order with
    /// the program's others.
    fn string_end(&mut self, _grid: &TerminalGrid, _completed: bool, _replies: &mut Vec<u8>) {}
}

/// No image support (a build without it, and tests of what does not concern images).
#[cfg(any(test, not(feature = "image-registry")))]
struct NoImages;

#[cfg(any(test, not(feature = "image-registry")))]
impl ReaderImages for NoImages {
    #[inline]
    fn operation(&mut self, _grid: &TerminalGrid) {}
}

/// The image state one PTY read is followed with, and the image events it held.
#[cfg(feature = "image-registry")]
struct ChunkImages<'a> {
    registry: &'a mut bmux_image::ImageRegistry,
    /// The image string in progress, kept from read to read.
    collector: &'a mut bmux_image::ImageCollector,
    cell_pixels: (u16, u16),
    events: Vec<bmux_image::ImageEvent>,
}

/// Images as the grid's parse finds them, placed at the cursor as of each, and images on screen
/// moved with each grid operation in order: erased with the display or history, reset with the
/// terminal, switched with the screen, and scrolled.
#[cfg(feature = "image-registry")]
struct FollowImages<'a, 'b> {
    images: &'a mut ChunkImages<'b>,
    erase: u64,
    history_erase: u64,
    reset: u64,
    /// The grid's viewport scroll revision already applied to image positions.
    scrolled: u64,
    /// Whether any image is held, so operations must move it; with none, screen changes only
    /// flip registry state, caught up before the next image and at the end of the read.
    following: bool,
    /// The first failure; images are not followed after it.
    error: Option<std::io::Error>,
}

#[cfg(feature = "image-registry")]
impl<'a, 'b> FollowImages<'a, 'b> {
    fn new(images: &'a mut ChunkImages<'b>, grid: &TerminalGrid, scrolled: u64) -> Self {
        let following = !images.registry.holds_nothing();
        Self {
            images,
            erase: grid.display_erase_revision(),
            history_erase: grid.history_erase_revision(),
            reset: grid.reset_revision(),
            scrolled,
            following,
            error: None,
        }
    }

    fn follow(&mut self, grid: &TerminalGrid) -> std::io::Result<()> {
        let registry = &mut *self.images.registry;
        if self.reset != grid.reset_revision() {
            self.reset = grid.reset_revision();
            registry.reset();
        }
        registry.set_alternate_screen(grid.mode() == bmux_terminal_grid::GridMode::Alternate);
        let (erase, history_erase) = (grid.display_erase_revision(), grid.history_erase_revision());
        if self.history_erase != history_erase {
            registry.clear();
        } else if self.erase != erase {
            registry.clear_display();
        }
        (self.erase, self.history_erase) = (erase, history_erase);
        let total = grid.viewport_scroll_revision();
        let rows = u16::try_from(total.wrapping_sub(std::mem::replace(&mut self.scrolled, total)))
            .unwrap_or(u16::MAX);
        if rows > 0 {
            registry.scroll_up(rows)?;
            registry.evict_history(grid.max_scrollback_offset());
        }
        Ok(())
    }

    /// Bring image state up to `grid`, whether or not any image is held.
    fn catch_up(&mut self, grid: &TerminalGrid) {
        if self.error.is_none()
            && let Err(error) = self.follow(grid)
        {
            self.error = Some(error);
        }
    }
}

#[cfg(feature = "image-registry")]
impl ReaderImages for FollowImages<'_, '_> {
    const STREAMS: bool = true;

    fn operation(&mut self, grid: &TerminalGrid) {
        if self.following {
            self.catch_up(grid);
        }
    }

    fn stream_dcs(&mut self, introducer: &SequenceHeader<'_>) -> bool {
        introducer.action() == 'q' && introducer.intermediates().is_empty()
    }

    fn string_start(&mut self, kind: StreamedString) {
        self.images.collector.start(match kind {
            StreamedString::Apc => bmux_image::ImageString::Kitty,
            StreamedString::Dcs => bmux_image::ImageString::Sixel,
            StreamedString::Osc => bmux_image::ImageString::ITerm2,
        });
    }

    fn string_data(&mut self, data: &[u8]) {
        self.images.collector.push(data);
    }

    fn string_end(&mut self, grid: &TerminalGrid, completed: bool, replies: &mut Vec<u8>) {
        let Some(mut event) = self.images.collector.finish(completed) else {
            return;
        };
        self.catch_up(grid);
        let (row, col) = grid_cursor_position(grid);
        event.set_position(bmux_image::ImagePosition { row, col });
        let (width, height) = self.images.cell_pixels;
        let registry = &mut *self.images.registry;
        registry.handle_event(event.clone(), width, height);
        // Kitty graphics answers go back to the program in order with its other replies.
        replies.extend(registry.take_replies());
        self.following = !registry.holds_nothing();
        self.images.events.push(event);
    }
}

/// Output filters shared through core's state registry, behind std's lock.
// simulation-safe: read for one chunk's parse, with no switch point inside
type SharedFilters = Arc<std::sync::RwLock<bmux_terminal_grid::SequenceFilters>>;

/// The registered filters, read for one chunk (a poisoned lock reads as none: filters shape what
/// panes show, they gate no access).
fn read_filters(
    shared: Option<&SharedFilters>,
    // simulation-safe: see SharedFilters
) -> Option<std::sync::RwLockReadGuard<'_, bmux_terminal_grid::SequenceFilters>> {
    shared.and_then(|filters| filters.read().ok())
}

/// The filters to consult, when any is registered.
fn active_filters(
    filters: Option<&bmux_terminal_grid::SequenceFilters>,
) -> Option<&bmux_terminal_grid::SequenceFilters> {
    filters.filter(|filters| !filters.is_empty())
}

/// What the pane reader observes of its grid's parse: queries answered by the protocol engine with
/// the cursor and modes as of each query, and pointer-shape queries answered for the bmux profile.
struct ReaderObserver<'a, F, I> {
    engine: &'a mut TerminalProtocolEngine,
    replies: &'a mut Vec<u8>,
    pointer_replies: &'a mut Vec<u8>,
    signals: &'a mut Vec<PaneSignal>,
    profile: ProtocolProfile,
    /// Plugin output filters: [`NoFilters`] when none is registered.
    filters: F,
    /// How images are followed: [`NoImages`] without image support.
    images: &'a mut I,
    /// The chunk being followed, and where what of it is output collects: everything but image
    /// strings ([`SequenceObserver::cut`]), with a possible image start held to the next chunk
    /// ([`SequenceObserver::hold`]).
    bytes: &'a [u8],
    visible: &'a mut Vec<u8>,
    held: &'a mut Vec<u8>,
    /// How far into `bytes` output is decided.
    cursor: usize,
    /// Whether the chunk's end is held.
    holding: bool,
}

impl<F: ReaderFilters, I: ReaderImages> ReaderObserver<'_, F, I> {
    /// Follow the chunk through `grid`, then add what of it is output to the visible bytes.
    fn follow(mut self, grid: &mut TerminalGridStream) {
        grid.process_observed(self.bytes, &mut self);
        if !self.holding {
            self.release(self.bytes.len());
        }
    }

    /// The chunk's bytes from the cursor to `end` are output, after any held start.
    fn release(&mut self, end: usize) {
        self.visible.append(self.held);
        self.visible
            .extend_from_slice(&self.bytes[self.cursor..end]);
    }
}

/// The output filters a pane reader consults, statically: [`NoFilters`] keeps everything and
/// compiles to nothing.
trait ReaderFilters {
    fn filter_csi(&self, sequence: &SequenceHeader<'_>, replacement: &mut Vec<u8>)
    -> FilterVerdict;
    fn filter_osc(&self, params: &[&[u8]], replacement: &mut Vec<u8>) -> FilterVerdict;
    fn filter_esc(
        &self,
        intermediates: &[u8],
        byte: u8,
        replacement: &mut Vec<u8>,
    ) -> FilterVerdict;
}

/// No output filter is registered.
struct NoFilters;

impl ReaderFilters for NoFilters {
    #[inline]
    fn filter_csi(&self, _sequence: &SequenceHeader<'_>, _: &mut Vec<u8>) -> FilterVerdict {
        FilterVerdict::Keep
    }
    #[inline]
    fn filter_osc(&self, _params: &[&[u8]], _: &mut Vec<u8>) -> FilterVerdict {
        FilterVerdict::Keep
    }
    #[inline]
    fn filter_esc(&self, _intermediates: &[u8], _byte: u8, _: &mut Vec<u8>) -> FilterVerdict {
        FilterVerdict::Keep
    }
}

impl ReaderFilters for &bmux_terminal_grid::SequenceFilters {
    fn filter_csi(
        &self,
        sequence: &SequenceHeader<'_>,
        replacement: &mut Vec<u8>,
    ) -> FilterVerdict {
        bmux_terminal_grid::SequenceFilters::filter_csi(self, sequence, replacement)
    }
    fn filter_osc(&self, params: &[&[u8]], replacement: &mut Vec<u8>) -> FilterVerdict {
        bmux_terminal_grid::SequenceFilters::filter_osc(self, params, replacement)
    }
    fn filter_esc(
        &self,
        intermediates: &[u8],
        byte: u8,
        replacement: &mut Vec<u8>,
    ) -> FilterVerdict {
        bmux_terminal_grid::SequenceFilters::filter_esc(self, intermediates, byte, replacement)
    }
}

/// What a pane's program last signalled beyond its screen (see
/// [`bmux_pane_runtime_state::PaneSignals`]), kept so readers see current
/// values even when they missed the events.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RetainedPaneSignals {
    revision: u64,
    title: Option<String>,
    working_directory: Option<String>,
    progress: Option<String>,
    /// The last finished command's exit status; cleared when one starts.
    exit_status: Option<u32>,
    notifications: u64,
    last_notification: Option<String>,
    bells: u64,
    /// The last state a program in the pane reported for itself, and how many
    /// reports there were.
    report: Option<bmux_pane_runtime_state::PaneAgentReport>,
    reports: u64,
}

impl RetainedPaneSignals {
    /// Keep a program's report of its own state.
    fn report(&mut self, state: &str, agent: Option<&str>, message: Option<&str>) {
        self.report = Some(bmux_pane_runtime_state::PaneAgentReport {
            state: state.to_string(),
            agent: agent.map(str::to_string),
            message: message.map(str::to_string),
        });
        self.reports = self.reports.wrapping_add(1);
        self.revision = self.revision.wrapping_add(1);
    }

    /// Keep what `signal` says; returns whether anything changed.
    fn apply(&mut self, signal: &PaneSignal) -> bool {
        let changed = match signal {
            PaneSignal::Title(title) => replace_if_changed(&mut self.title, title),
            PaneSignal::Progress(value) => replace_if_changed(&mut self.progress, value),
            PaneSignal::Notification { title, body } => {
                self.notifications = self.notifications.wrapping_add(1);
                self.last_notification = Some(match title {
                    Some(title) if !title.is_empty() => format!("{title}: {body}"),
                    _ => body.clone(),
                });
                true
            }
            PaneSignal::Bell => {
                self.bells = self.bells.wrapping_add(1);
                true
            }
            PaneSignal::WorkingDirectory(location) => {
                replace_if_changed(&mut self.working_directory, location)
            }
            PaneSignal::PromptMark {
                kind: PromptMarkKind::CommandExecuted,
                ..
            } => self.exit_status.take().is_some(),
            PaneSignal::PromptMark {
                kind: PromptMarkKind::CommandFinished,
                exit_status: Some(status),
                ..
            } => self.exit_status.replace(*status) != Some(*status),
            PaneSignal::PromptMark { .. } | PaneSignal::ClipboardWrite { .. } => false,
        };
        if changed {
            self.revision = self.revision.wrapping_add(1);
        }
        changed
    }
}

fn replace_if_changed(slot: &mut Option<String>, value: &str) -> bool {
    if slot.as_deref() == Some(value) {
        return false;
    }
    *slot = Some(value.to_string());
    true
}

/// Something a pane's program signalled through its output beyond the screen, as the reader saw
/// it (see `pane-runtime-terminal-events`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum PaneSignal {
    Title(String),
    WorkingDirectory(String),
    /// OSC 9;4, from the `4`.
    Progress(String),
    Notification {
        title: Option<String>,
        body: String,
    },
    Bell,
    PromptMark {
        kind: PromptMarkKind,
        line: u64,
        exit_status: Option<u32>,
    },
    /// OSC 52: the program sets a clipboard. Sent only to the clients viewing the pane (in their
    /// frames), which apply their own policy; never published to plugins.
    ClipboardWrite {
        selection: String,
        text: String,
    },
}

/// The clipboard write an OSC 52 payload (`selection;base64`) carries, if any: a query (`?`)
/// reads instead, and text must be valid UTF-8.
fn clipboard_write(payload: &[u8]) -> Option<PaneSignal> {
    let split = payload.iter().position(|byte| *byte == b';')?;
    let (selection, data) = (&payload[..split], &payload[split + 1..]);
    if data == b"?" {
        return None;
    }
    let text = String::from_utf8(decode_base64(std::str::from_utf8(data).ok()?)?).ok()?;
    Some(PaneSignal::ClipboardWrite {
        selection: String::from_utf8_lossy(selection).into_owned(),
        text,
    })
}

impl PaneSignal {
    /// The signal an OSC string carries, if any: OSC 0 and 2 (title), 7 (working directory),
    /// 9 (a notification, or with 9;4 progress), 777;notify (a titled notification) and 133
    /// (prompt marks), with the line the cursor is on.
    fn from_osc(params: &[&[u8]], line: u64) -> Option<Self> {
        let text = |parts: &[&[u8]]| String::from_utf8_lossy(&parts.join(&b';')).into_owned();
        match params {
            [b"0" | b"2", rest @ ..] if !rest.is_empty() => Some(Self::Title(text(rest))),
            [b"7", rest @ ..] if !rest.is_empty() => Some(Self::WorkingDirectory(text(rest))),
            [b"9", rest @ ..] if rest.first() == Some(&b"4".as_slice()) => {
                Some(Self::Progress(text(rest)))
            }
            [b"9", rest @ ..] if !rest.is_empty() => Some(Self::Notification {
                title: None,
                body: text(rest),
            }),
            [b"777", b"notify", title, rest @ ..] => Some(Self::Notification {
                title: Some(String::from_utf8_lossy(title).into_owned()),
                body: text(rest),
            }),
            [b"133", mark, rest @ ..] => {
                let kind = match *mark {
                    b"A" => PromptMarkKind::PromptStart,
                    b"B" => PromptMarkKind::CommandStart,
                    b"C" => PromptMarkKind::CommandExecuted,
                    b"D" => PromptMarkKind::CommandFinished,
                    _ => return None,
                };
                let exit_status = (kind == PromptMarkKind::CommandFinished)
                    .then(|| rest.first())
                    .flatten()
                    .and_then(|status| std::str::from_utf8(status).ok()?.parse().ok());
                Some(Self::PromptMark {
                    kind,
                    line,
                    exit_status,
                })
            }
            _ => None,
        }
    }

    fn into_event(
        self,
        session_id: Uuid,
        pane_id: Uuid,
    ) -> pane_runtime_terminal_events::TerminalEvent {
        use pane_runtime_terminal_events::TerminalEvent;
        match self {
            Self::Title(title) => TerminalEvent::Title {
                session_id,
                pane_id,
                title,
            },
            Self::WorkingDirectory(location) => TerminalEvent::WorkingDirectory {
                session_id,
                pane_id,
                location,
            },
            Self::Progress(value) => TerminalEvent::Progress {
                session_id,
                pane_id,
                value,
            },
            Self::Notification { title, body } => TerminalEvent::Notification {
                session_id,
                pane_id,
                title,
                body,
            },
            Self::Bell => TerminalEvent::Bell {
                session_id,
                pane_id,
            },
            Self::PromptMark {
                kind,
                line,
                exit_status,
            } => TerminalEvent::PromptMark {
                session_id,
                pane_id,
                kind,
                line,
                exit_status,
            },
            Self::ClipboardWrite { .. } => unreachable!("clipboard writes go to clients only"),
        }
    }
}

impl<F: ReaderFilters, I: ReaderImages> SequenceObserver for ReaderObserver<'_, F, I> {
    const STREAMS_APC: bool = I::STREAMS;
    const STREAMS_DCS: bool = I::STREAMS;
    const STREAMED_OSC_PREFIX: &'static [u8] = if I::STREAMS { b"1337;File=" } else { b"" };

    fn operation(&mut self, grid: &TerminalGrid) {
        self.images.operation(grid);
    }

    fn stream_dcs(&mut self, introducer: &SequenceHeader<'_>) -> bool {
        self.images.stream_dcs(introducer)
    }

    fn string_start(&mut self, kind: StreamedString) {
        self.images.string_start(kind);
    }

    fn string_data(&mut self, data: &[u8]) {
        self.images.string_data(data);
    }

    fn string_end(&mut self, grid: &TerminalGrid, completed: bool) {
        self.images.string_end(grid, completed, self.replies);
    }

    fn cut(&mut self, from: ChunkMark, to: usize) {
        match from {
            ChunkMark::Before => self.held.clear(),
            ChunkMark::At(start) => self.release(start),
        }
        self.cursor = to;
    }

    fn hold(&mut self, from: ChunkMark) {
        match from {
            ChunkMark::Before => self.held.extend_from_slice(&self.bytes[self.cursor..]),
            ChunkMark::At(start) => {
                self.release(start);
                self.held.extend_from_slice(&self.bytes[start..]);
            }
        }
        self.cursor = self.bytes.len();
        self.holding = true;
    }

    fn filter_csi(
        &mut self,
        sequence: &SequenceHeader<'_>,
        replacement: &mut Vec<u8>,
    ) -> FilterVerdict {
        self.filters.filter_csi(sequence, replacement)
    }

    fn filter_osc(&mut self, params: &[&[u8]], replacement: &mut Vec<u8>) -> FilterVerdict {
        self.filters.filter_osc(params, replacement)
    }

    fn filter_esc(
        &mut self,
        intermediates: &[u8],
        byte: u8,
        replacement: &mut Vec<u8>,
    ) -> FilterVerdict {
        self.filters.filter_esc(intermediates, byte, replacement)
    }

    fn csi(&mut self, grid: &TerminalGrid, sequence: &SequenceHeader<'_>) {
        if !TerminalProtocolEngine::may_answer_csi(sequence.action()) {
            return;
        }
        let Some(values) = sequence.values() else {
            return;
        };
        let cursor = grid_cursor_position(grid);
        if sequence.action() == 'n' && *values == [6] {
            trace!(
                tracked_row = cursor.0.saturating_add(1),
                tracked_col = cursor.1.saturating_add(1),
                pane_rows = grid.height(),
                pane_cols = grid.width(),
                alternate_screen = matches!(grid.mode(), bmux_terminal_grid::GridMode::Alternate),
                "pane protocol reply: cursor position report"
            );
        }
        self.engine.answer_csi(
            &values,
            sequence.intermediates(),
            sequence.action(),
            cursor,
            grid.protocol_state().bracketed_paste,
            grid.synchronized_update(),
            self.replies,
        );
    }

    fn osc(&mut self, grid: &TerminalGrid, params: &[&[u8]], _bell_terminated: bool) {
        self.engine.answer_osc(params, self.replies);
        let line = grid
            .total_scrolled_rows()
            .saturating_add(grid.cursor().row as u64);
        if let Some(signal) = PaneSignal::from_osc(params, line) {
            self.signals.push(signal);
        }
    }

    fn bell(&mut self, _grid: &TerminalGrid) {
        self.signals.push(PaneSignal::Bell);
    }

    fn wants_osc(&mut self, number: u32) -> bool {
        number == 52
    }

    fn osc_payload(&mut self, _grid: &TerminalGrid, _number: u32, payload: &[u8]) {
        self.signals.extend(clipboard_write(payload));
    }

    fn wants_dcs(&mut self, introducer: &SequenceHeader<'_>) -> bool {
        introducer.values().is_some_and(|values| {
            TerminalProtocolEngine::wants_dcs(
                &values,
                introducer.intermediates(),
                introducer.action(),
            )
        })
    }

    fn dcs(&mut self, _grid: &TerminalGrid, intermediates: &[u8], _action: char, data: &[u8]) {
        self.engine.answer_dcs(intermediates, data, self.replies);
    }

    fn pointer_query(
        &mut self,
        query: &[u8],
        current: Option<bmux_terminal_protocol::pointer_shape::PointerShape>,
    ) {
        use bmux_terminal_protocol::pointer_shape::{
            PointerShape, PointerShapeQueryContext, encode_pointer_shape_query_reply,
        };
        if matches!(self.profile, ProtocolProfile::Bmux) {
            // The grid has already bounded and validated the complete OSC payload.
            let _ = encode_pointer_shape_query_reply(
                query,
                PointerShapeQueryContext {
                    current,
                    default: PointerShape::Text,
                    grabbed: PointerShape::Default,
                },
                self.pointer_replies,
            );
        }
    }
}

/// A grid with the reader state that follows output through it, as tests drive image ordering
/// and cursor reports without a pane: what the reader thread keeps beside a pane's grid.
#[cfg(test)]
struct PaneCursorTracker {
    terminal_grid: TerminalGridStream,
    reader: PaneReader,
    rows: u16,
    cols: u16,
}

#[cfg(test)]
impl PaneCursorTracker {
    fn new(rows: u16, cols: u16) -> Self {
        let (rows, cols) = sanitize_pty_size(rows, cols);
        Self {
            terminal_grid: TerminalGridStream::new(cols, rows, GridLimits::default())
                .expect("test grid dimensions are valid"),
            reader: PaneReader::new(ProtocolProfile::Bmux, None),
            rows,
            cols,
        }
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = sanitize_pty_size(rows, cols);
        if (self.rows, self.cols) != (rows, cols) {
            let _ = self.terminal_grid.resize(cols, rows);
            (self.rows, self.cols) = (rows, cols);
        }
    }
}

/// The cursor of `grid`, as a cursor position report gives it (zero-based row and column).
fn grid_cursor_position(grid: &TerminalGrid) -> (u16, u16) {
    let cursor = grid.cursor();
    (
        u16::try_from(cursor.row).unwrap_or(u16::MAX),
        u16::try_from(cursor.col).unwrap_or(u16::MAX),
    )
}

/// One PTY read as the pane reader follows it: shell metadata is removed first, then the rest is
/// parsed by the pane's grid while the protocol engine answers queries (cursor reports as of each
/// query), and images, recognised by the same parse, are placed at the cursor as of each and move
/// with the grid's erases, resets, screen switches and scrolls. The bytes the grid saw, without
/// image strings, collect in the reader's `visible`.
#[cfg(feature = "image-registry")]
fn follow_chunk(
    reader: &mut PaneReader,
    grid: &mut TerminalGridStream,
    protocol: &mut TerminalProtocolEngine,
    images: &mut ChunkImages<'_>,
    chunk: &[u8],
    replies: &mut Vec<u8>,
) -> std::io::Result<()> {
    reader.begin_chunk();
    images.events.clear();
    let filtered = metadata_filtered(reader, chunk);
    let scrolled = *reader
        .scrolled
        .get_or_insert_with(|| grid.grid().viewport_scroll_revision());
    let shared = reader.filters.clone();
    let guard = read_filters(shared.as_ref());
    let mut follow = FollowImages::new(images, grid.grid(), scrolled);
    reader.feed(
        grid,
        protocol,
        &filtered,
        replies,
        active_filters(guard.as_deref()),
        &mut follow,
    );
    follow.catch_up(grid.grid());
    reader.scrolled = Some(follow.scrolled);
    follow.error.map_or(Ok(()), Err)
}

/// One PTY read as the pane reader follows it: shell metadata is removed first, then the rest is
/// parsed by the pane's grid while the protocol engine answers queries (cursor reports as of each
/// query). The bytes the grid saw collect in the reader's `visible`.
#[cfg(not(feature = "image-registry"))]
fn follow_chunk(
    reader: &mut PaneReader,
    grid: &mut TerminalGridStream,
    protocol: &mut TerminalProtocolEngine,
    chunk: &[u8],
    replies: &mut Vec<u8>,
) {
    reader.begin_chunk();
    let filtered = metadata_filtered(reader, chunk);
    let shared = reader.filters.clone();
    let guard = read_filters(shared.as_ref());
    reader.feed(
        grid,
        protocol,
        &filtered,
        replies,
        active_filters(guard.as_deref()),
        &mut NoImages,
    );
}

/// `bytes` without the shell metadata they carry (collected in the reader's metadata events).
fn metadata_filtered<'b>(reader: &mut PaneReader, bytes: &'b [u8]) -> std::borrow::Cow<'b, [u8]> {
    match reader.metadata.as_mut() {
        Some(parser) => {
            let parsed = parser.process_chunk(bytes);
            reader.metadata_events.extend(parsed.events);
            parsed.filtered
        }
        None => std::borrow::Cow::Borrowed(bytes),
    }
}

fn sanitize_pty_size(rows: u16, cols: u16) -> (u16, u16) {
    (rows.max(1), cols.max(1))
}

/// Convert an image event to a recording payload.
#[cfg(feature = "image-registry")]
fn image_event_to_recording_payload(event: &bmux_image::ImageEvent) -> RecordingPayload {
    match event {
        #[cfg(feature = "image-registry")]
        bmux_image::ImageEvent::SixelImage {
            data,
            position,
            pixel_size,
            ..
        } => RecordingPayload::Image {
            protocol: 0,
            position_row: position.row,
            position_col: position.col,
            cell_rows: 0,
            cell_cols: 0,
            pixel_width: pixel_size.width,
            pixel_height: pixel_size.height,
            data: data.clone(),
        },
        #[cfg(feature = "image-registry")]
        bmux_image::ImageEvent::KittyCommand { command: cmd, .. } => {
            let mut apc_body = Vec::new();
            match cmd {
                bmux_image::KittyCommand::Transmit {
                    image_id,
                    data,
                    width,
                    height,
                    ..
                } => {
                    apc_body = bmux_image::codec::kitty::encode_transmit(
                        *image_id,
                        bmux_image::KittyFormat::Rgba,
                        data,
                        *width,
                        *height,
                    );
                }
                bmux_image::KittyCommand::Place(placement) => {
                    apc_body = bmux_image::codec::kitty::encode_place(
                        placement.image_id,
                        placement.placement_id,
                        placement.position.row,
                        placement.position.col,
                    );
                }
                _ => {}
            }
            RecordingPayload::Image {
                protocol: 1,
                position_row: 0,
                position_col: 0,
                cell_rows: 0,
                cell_cols: 0,
                pixel_width: 0,
                pixel_height: 0,
                data: apc_body,
            }
        }
        #[cfg(feature = "image-registry")]
        bmux_image::ImageEvent::ITerm2Image { data, position, .. } => RecordingPayload::Image {
            protocol: 2,
            position_row: position.row,
            position_col: position.col,
            cell_rows: 0,
            cell_cols: 0,
            pixel_width: 0,
            pixel_height: 0,
            data: data.clone(),
        },
    }
}

/// A collector for image strings, as the reader thread keeps one.
#[cfg(all(test, feature = "image-registry"))]
fn image_collector() -> bmux_image::ImageCollector {
    bmux_image::ImageCollector::with_max_encoded_bytes(16 * 1024 * 1024)
}

/// Follow `chunk` with image support as the reader thread does (8 by 16 pixel cells), without a
/// pane; returns the image events it held.
#[cfg(all(test, feature = "image-registry"))]
fn follow_image_chunk(
    reader: &mut PaneReader,
    grid: &mut TerminalGridStream,
    protocol: &mut TerminalProtocolEngine,
    registry: &mut bmux_image::ImageRegistry,
    collector: &mut bmux_image::ImageCollector,
    chunk: &[u8],
    replies: &mut Vec<u8>,
) -> std::io::Result<Vec<bmux_image::ImageEvent>> {
    let mut images = ChunkImages {
        registry,
        collector,
        cell_pixels: (8, 16),
        events: Vec::new(),
    };
    follow_chunk(reader, grid, protocol, &mut images, chunk, replies)?;
    Ok(images.events)
}

#[cfg(all(test, feature = "image-registry"))]
mod image_lifecycle_tests {
    use super::*;

    #[test]
    fn pinned_image_query_projects_history_and_rejects_incompatible_geometry() {
        let mut tracker = PaneCursorTracker::new(3, 80);
        let mut protocol = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
        let mut registry = bmux_image::ImageRegistry::default();
        let mut collector = image_collector();
        follow_image_chunk(
            &mut tracker.reader,
            &mut tracker.terminal_grid,
            &mut protocol,
            &mut registry,
            &mut collector,
            b"\x1bPq\"1;1;1;6#1;2;100;0;0~\x1b\\\r\n\r\n\r\n",
            &mut Vec::new(),
        )
        .unwrap();
        let mut budget = RESPONSE_OUTPUT_BUDGET;
        let images = registry.capture_history(&mut budget).unwrap();
        let grid = tracker
            .terminal_grid
            .grid()
            .try_clone_charged(&mut budget)
            .unwrap();
        let bytes = RESPONSE_OUTPUT_BUDGET - budget;
        let pin = CapturedPinGrid {
            grid,
            images: Some(images),
            reservation: PinReservation::acquire(&Arc::new(AtomicUsize::new(0)), bytes).unwrap(),
        };
        registry.reset();
        let mut request = crate::handlers::attach_state::HistoryImagesArgs {
            session_id: crate::ids::new_uuid(),
            pane_id: crate::ids::new_uuid(),
            pin_id: 1,
            capture_id: crate::ids::new_uuid(),
            width: 80,
            scrollback_offset: 1,
            rows: 3,
        };
        let encoded = encode_captured_image_window(&pin, &request).unwrap();
        let images: Vec<bmux_attach_image_protocol::AttachPaneImage> =
            serde_json::from_slice(&encoded).unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].position_row, 0);
        assert!(!images[0].raw_data.is_empty());
        request.scrollback_offset = 0;
        assert_eq!(encode_captured_image_window(&pin, &request).unwrap(), b"[]");
        request.width = 40;
        assert!(encode_captured_image_window(&pin, &request).is_err());
        request.scrollback_offset = 5;
        let reflowed = encode_reflowed_image_window(&pin, &request).unwrap();
        let reflowed: Vec<bmux_attach_image_protocol::AttachPaneImage> =
            serde_json::from_slice(&reflowed).unwrap();
        assert_eq!(reflowed.len(), 1);
        assert_eq!(reflowed[0].position_row, 0);
        request.width = 80;
        request.rows = 1;
        request.scrollback_offset = 1;
        assert_eq!(encode_reflowed_image_window(&pin, &request).unwrap(), b"[]");
        request.scrollback_offset = 3;
        let short: Vec<bmux_attach_image_protocol::AttachPaneImage> =
            serde_json::from_slice(&encode_reflowed_image_window(&pin, &request).unwrap()).unwrap();
        assert_eq!(short.len(), 1);
        assert_eq!(short[0].position_row, 0);
        request.scrollback_offset = 100;
        assert!(encode_captured_image_window(&pin, &request).is_err());
    }

    #[test]
    fn pinned_image_reflow_accepts_a_trailing_wide_character() {
        let mut tracker = PaneCursorTracker::new(1, 4);
        let mut protocol = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
        let mut registry = bmux_image::ImageRegistry::default().with_viewport_height(1);
        let mut collector = image_collector();
        follow_image_chunk(
            &mut tracker.reader,
            &mut tracker.terminal_grid,
            &mut protocol,
            &mut registry,
            &mut collector,
            "\x1bPq\"1;1;1;6#1;2;100;0;0~\x1b\\ab界".as_bytes(),
            &mut Vec::new(),
        )
        .unwrap();
        let mut budget = RESPONSE_OUTPUT_BUDGET;
        let images = registry.capture_history(&mut budget).unwrap();
        let grid = tracker
            .terminal_grid
            .grid()
            .try_clone_charged(&mut budget)
            .unwrap();
        let pin = CapturedPinGrid {
            grid,
            images: Some(images),
            reservation: PinReservation::acquire(
                &Arc::new(AtomicUsize::new(0)),
                RESPONSE_OUTPUT_BUDGET - budget,
            )
            .unwrap(),
        };
        for (width, scrollback_offset) in [(4, 0), (2, 1)] {
            let request = crate::handlers::attach_state::HistoryImagesArgs {
                session_id: crate::ids::new_uuid(),
                pane_id: crate::ids::new_uuid(),
                pin_id: 1,
                capture_id: crate::ids::new_uuid(),
                width,
                scrollback_offset,
                rows: 1,
            };
            let images: Vec<bmux_attach_image_protocol::AttachPaneImage> =
                serde_json::from_slice(&encode_reflowed_image_window(&pin, &request).unwrap())
                    .unwrap();
            assert_eq!(images.len(), 1);
            assert_eq!(images[0].position_row, 0);
            assert_eq!(images[0].position_col, 0);
            assert!(!images[0].raw_data.is_empty());
        }
    }

    /// Images follow the grid's operations in its one pass exactly as they would byte by byte:
    /// a stream of images, scrolling, erases and screen switches leaves the same image positions
    /// (on screen and in history) and the same screen whole or a byte per chunk.
    #[test]
    fn images_follow_grid_operations_alike_whole_or_byte_by_byte() {
        let image = b"\x1bPq\"1;1;1;6#1;2;100;0;0~\x1b\\";
        let mut stream = b"first\r\n".to_vec();
        stream.extend_from_slice(image);
        stream.extend_from_slice(b"\r\na\r\nb\r\nc\r\n");
        stream.extend_from_slice(image);
        stream.extend_from_slice(b"\r\nwrapping text that runs past the edge of the pane\r\n");
        stream.extend_from_slice(b"\x1b[?1049h");
        stream.extend_from_slice(image);
        stream.extend_from_slice(b"alt\r\n\x1b[2J\x1b[?1049l");
        stream.extend_from_slice(b"d\r\ne\r\n\x1b[2J");
        stream.extend_from_slice(image);
        stream.extend_from_slice(b"\r\nf");
        let follow = |chunk_size: usize| {
            let mut tracker = PaneCursorTracker::new(3, 20);
            let mut protocol = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
            let mut registry = bmux_image::ImageRegistry::default();
            let mut collector = image_collector();
            for chunk in stream.chunks(chunk_size) {
                follow_image_chunk(
                    &mut tracker.reader,
                    &mut tracker.terminal_grid,
                    &mut protocol,
                    &mut registry,
                    &mut collector,
                    chunk,
                    &mut Vec::new(),
                )
                .unwrap();
            }
            let place = |image: &bmux_image::PaneImage| {
                (image.position.row, image.position.col, image.cell_size.rows)
            };
            let visible: Vec<_> = registry.images().iter().map(place).collect();
            let history: Vec<Vec<_>> = (0..8)
                .map(|offset| {
                    registry
                        .images_in_viewport(offset, 3)
                        .into_iter()
                        .map(place)
                        .collect()
                })
                .collect();
            let screen = bmux_terminal_grid::visible_text(tracker.terminal_grid.grid(), 0, 3);
            (visible, history, screen)
        };
        let whole = follow(stream.len());
        assert!(!whole.0.is_empty(), "an image stays on screen");
        assert!(
            whole.1.iter().skip(1).any(|images| !images.is_empty()),
            "an image scrolled into history: {whole:?}"
        );
        assert_eq!(follow(1), whole);
        assert_eq!(follow(7), whole);
    }

    #[test]
    fn scroll_then_place_uses_final_cursor_without_double_processing() {
        let image = b"\x1bPq\"1;1;1;6#1;2;100;0;0~\x1b\\";
        let mut bytes = b"one\r\ntwo\r\nthree\r\nfour\r\n".to_vec();
        bytes.extend_from_slice(image);
        for split in 0..=bytes.len() {
            let mut tracker = PaneCursorTracker::new(3, 80);
            let mut protocol = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
            let mut registry = bmux_image::ImageRegistry::default();
            let mut collector = image_collector();
            for chunk in [&bytes[..split], &bytes[split..]] {
                follow_image_chunk(
                    &mut tracker.reader,
                    &mut tracker.terminal_grid,
                    &mut protocol,
                    &mut registry,
                    &mut collector,
                    chunk,
                    &mut Vec::new(),
                )
                .unwrap();
            }
            assert_eq!(registry.images().len(), 1, "split {split}");
            assert_eq!(registry.images()[0].position.row, 2, "split {split}");
            for _ in 0..3 {
                follow_image_chunk(
                    &mut tracker.reader,
                    &mut tracker.terminal_grid,
                    &mut protocol,
                    &mut registry,
                    &mut collector,
                    b"\r\n",
                    &mut Vec::new(),
                )
                .unwrap();
            }
            assert_eq!(registry.images(), []);
            let historical = registry.project_viewport(3, 3).unwrap();
            assert_eq!(historical.len(), 1, "split {split}");
            assert_eq!(historical[0].position.row, 2);
        }
    }

    #[test]
    fn reset_discards_both_screen_image_namespaces_at_every_split() {
        let image = b"\x1bPq\"1;1;1;6#1;2;100;0;0~\x1b\\";
        let mut bytes = image.to_vec();
        bytes.extend_from_slice(b"\r\n\r\n\r\n\x1b[?1049h");
        bytes.extend_from_slice(image);
        bytes.extend_from_slice(b"\x1bc\x1b[?1049l");
        for split in 0..=bytes.len() {
            let mut tracker = PaneCursorTracker::new(3, 80);
            let mut protocol = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
            let mut registry = bmux_image::ImageRegistry::default();
            let mut collector = image_collector();
            for chunk in [&bytes[..split], &bytes[split..]] {
                follow_image_chunk(
                    &mut tracker.reader,
                    &mut tracker.terminal_grid,
                    &mut protocol,
                    &mut registry,
                    &mut collector,
                    chunk,
                    &mut Vec::new(),
                )
                .unwrap();
            }
            assert!(registry.images().is_empty(), "split {split}");
            assert!(
                registry.project_viewport(1, 3).unwrap().is_empty(),
                "split {split}"
            );
        }
    }

    #[test]
    fn alternate_scroll_moves_images_without_creating_main_history() {
        let mut tracker = PaneCursorTracker::new(3, 80);
        let mut protocol = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
        let mut registry = bmux_image::ImageRegistry::default().with_viewport_height(3);
        let mut collector = image_collector();
        for bytes in [
            b"\x1b[?1049h\x1b[3;1H\x1bPq\"1;1;1;6#1;2;100;0;0~\x1b\\".as_slice(),
            b"\r\n",
        ] {
            follow_image_chunk(
                &mut tracker.reader,
                &mut tracker.terminal_grid,
                &mut protocol,
                &mut registry,
                &mut collector,
                bytes,
                &mut Vec::new(),
            )
            .unwrap();
        }
        assert_eq!(registry.images().len(), 1);
        assert_eq!(registry.images()[0].position.row, 1);
        assert_eq!(tracker.terminal_grid.grid().total_scrolled_rows(), 0);
    }

    #[test]
    fn alternate_images_do_not_borrow_normal_scrollback_retention() {
        let mut tracker = PaneCursorTracker::new(3, 80);
        let mut protocol = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
        let mut registry = bmux_image::ImageRegistry::default().with_viewport_height(3);
        let mut collector = image_collector();
        follow_image_chunk(
            &mut tracker.reader,
            &mut tracker.terminal_grid,
            &mut protocol,
            &mut registry,
            &mut collector,
            b"normal\r\n\r\n\r\n\r\n\x1b[?1049h\x1bPq\"1;1;1;6#1;2;100;0;0~\x1b\\\r\n\r\n\r\n",
            &mut Vec::new(),
        )
        .unwrap();
        assert!(tracker.terminal_grid.grid().main_row_count() > 3);
        assert_eq!(tracker.terminal_grid.grid().max_scrollback_offset(), 0);
        assert_eq!(registry.images(), []);
        assert_eq!(
            registry.project_viewport(1, 3).unwrap(),
            [] as [bmux_image::PaneImage; 0]
        );
    }

    #[test]
    fn alternate_screen_scrolling_preserves_normal_image_history() {
        let image = b"\x1bPq\"1;1;1;6#1;2;100;0;0~\x1b\\";
        let mut bytes = image.to_vec();
        bytes.extend_from_slice(b"\r\n\r\n\r\n\x1b[?1049h");
        bytes.extend_from_slice(image);
        bytes.extend_from_slice(b"\r\n\r\n\r\n\r\n\x1b[3J\x1b[?1049l");
        for split in 0..=bytes.len() {
            let mut tracker = PaneCursorTracker::new(3, 80);
            let mut protocol = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
            let mut registry = bmux_image::ImageRegistry::default();
            let mut collector = image_collector();
            for chunk in [&bytes[..split], &bytes[split..]] {
                follow_image_chunk(
                    &mut tracker.reader,
                    &mut tracker.terminal_grid,
                    &mut protocol,
                    &mut registry,
                    &mut collector,
                    chunk,
                    &mut Vec::new(),
                )
                .unwrap();
            }
            assert!(registry.images().is_empty(), "split {split}");
            let history = registry.project_viewport(1, 3).unwrap();
            assert_eq!(history.len(), 1, "split {split}");
            assert_eq!(history[0].id, 1, "split {split}");
            assert_eq!(history[0].position.row, 0, "split {split}");
        }
    }

    #[test]
    fn screen_switches_and_erases_preserve_stream_order_at_every_split() {
        let image = b"\x1bPq\"1;1;1;6#1;2;100;0;0~\x1b\\";
        let mut bytes = image.to_vec();
        bytes.extend_from_slice(b"\x1b[?1049h\x1b[2J");
        bytes.extend_from_slice(image);
        bytes.extend_from_slice(b"\x1b[?1049l");
        for split in 0..=bytes.len() {
            let mut tracker = PaneCursorTracker::new(24, 80);
            let mut protocol = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
            let mut registry = bmux_image::ImageRegistry::default();
            let mut collector = image_collector();
            for chunk in [&bytes[..split], &bytes[split..]] {
                follow_image_chunk(
                    &mut tracker.reader,
                    &mut tracker.terminal_grid,
                    &mut protocol,
                    &mut registry,
                    &mut collector,
                    chunk,
                    &mut Vec::new(),
                )
                .unwrap();
            }
            assert_eq!(registry.images().len(), 1, "split {split}");
            assert_eq!(
                registry.images()[0].id,
                1,
                "normal screen restored at split {split}"
            );
            follow_image_chunk(
                &mut tracker.reader,
                &mut tracker.terminal_grid,
                &mut protocol,
                &mut registry,
                &mut collector,
                b"\x1b[2J",
                &mut Vec::new(),
            )
            .unwrap();
            assert_eq!(registry.images(), []);
        }
    }

    /// A status request (DECRQSS, `DCS $ q`) is answered with image support too: only a DCS
    /// string with no intermediates is Sixel (the interceptor took this one for an image).
    #[test]
    fn status_requests_are_answered_not_taken_for_sixel() {
        let mut tracker = PaneCursorTracker::new(24, 80);
        let mut protocol = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
        let mut registry = bmux_image::ImageRegistry::default();
        let mut replies = Vec::new();
        let events = follow_image_chunk(
            &mut tracker.reader,
            &mut tracker.terminal_grid,
            &mut protocol,
            &mut registry,
            &mut image_collector(),
            b"\x1bP$qm\x1b\\",
            &mut replies,
        )
        .unwrap();
        assert!(events.is_empty());
        assert!(replies.starts_with(b"\x1bP1$r"), "{replies:?}");
        assert_eq!(tracker.reader.visible, b"\x1bP$qm\x1b\\");
    }

    /// What an image event carries, without where it was placed.
    fn payload(event: &bmux_image::ImageEvent) -> (u8, Vec<u8>) {
        match event {
            bmux_image::ImageEvent::SixelImage { data, .. } => (0, data.clone()),
            bmux_image::ImageEvent::KittyCommand { command, .. } => {
                (1, format!("{command:?}").into_bytes())
            }
            bmux_image::ImageEvent::ITerm2Image { data, .. } => (2, data.clone()),
        }
    }

    /// The grid's parse finds the images the interceptor found ahead of it, in the same reads, and
    /// leaves raw readers the same bytes wherever reads split the output: image strings removed,
    /// and a start that might yet be one held to a later read (a DCS header with intermediates,
    /// which cannot begin Sixel, is released as they arrive, where the interceptor held it to its
    /// final byte).
    #[test]
    fn the_parse_finds_images_and_output_as_the_interceptor_did() {
        let pieces: &[&[u8]] = &[
            b"plain",
            b"\x1b[1;31mred\x1b[0m",
            b"\x1b]0;title\x07",
            b"\x1b]133;A\x1b\\",
            b"\x1b]1337;SetUserVar=a=b\x07",
            b"\x1bPq#0;2;0;0;0~-\x07~\x1b\\",
            b"\x1bPq#0~\x1b[m",
            b"\x1bP1$r0m\x1b\\",
            b"\x1b_Gi=7,a=q,s=1,v=1,f=24;AAAA\x1b\\",
            b"\x1b_Gi=8,a=q,s=1,v=1,f=24;AAAA\x07",
            b"\x1b_Gi=9,a=q;AA\x18",
            b"\x1b_x\x1b\\",
            b"\x1b_\x07",
            b"\x1b]1337;File=inline=1:AAAA\x07",
            b"\x1b]1337;File=inline=1:BBBB\x1b\\",
            b"\x1b]13\x18",
            b"\x1b\x1b[2J",
            b"\x18\x1a",
        ];
        for first in pieces {
            for second in pieces {
                let stream = [*first, *second, b"end"].concat();
                for split in 0..=stream.len() {
                    let mut interceptor = bmux_image::ImageInterceptor::new();
                    let mut tracker = PaneCursorTracker::new(24, 80);
                    let mut protocol = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
                    let mut registry = bmux_image::ImageRegistry::default();
                    let mut collector = image_collector();
                    let (mut visible, mut filtered) = (Vec::new(), Vec::new());
                    let context =
                        format!("{:?} split at {split}", String::from_utf8_lossy(&stream));
                    for chunk in [&stream[..split], &stream[split..]] {
                        let expected = interceptor.process(chunk);
                        let events = follow_image_chunk(
                            &mut tracker.reader,
                            &mut tracker.terminal_grid,
                            &mut protocol,
                            &mut registry,
                            &mut collector,
                            chunk,
                            &mut Vec::new(),
                        )
                        .unwrap();
                        visible.extend_from_slice(&tracker.reader.visible);
                        filtered.extend_from_slice(&expected.filtered);
                        assert_eq!(
                            events.iter().map(payload).collect::<Vec<_>>(),
                            expected.events.iter().map(payload).collect::<Vec<_>>(),
                            "{context}"
                        );
                    }
                    assert_eq!(
                        String::from_utf8_lossy(&visible),
                        String::from_utf8_lossy(&filtered),
                        "{context}"
                    );
                }
            }
        }
    }
}

/// The replies `chunk` draws as the pane reader follows it through `grid` (tests drive replies
/// through this).
#[cfg(test)]
fn protocol_reply_for_chunk(
    protocol_engine: &mut TerminalProtocolEngine,
    reader: &mut PaneReader,
    grid: &mut TerminalGridStream,
    chunk: &[u8],
) -> Vec<u8> {
    let mut replies = Vec::new();
    reader.feed(
        grid,
        protocol_engine,
        chunk,
        &mut replies,
        None,
        &mut NoImages,
    );
    replies
}

const BMUX_SHELL_METADATA_PREFIX: &str = "633;bmux;";
const MAX_SHELL_METADATA_PAYLOAD_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellKind {
    Bash,
    Zsh,
    Fish,
    Nu,
    Other,
}

#[derive(Debug, Default)]
struct PaneShellMetadataParseOutput<'chunk> {
    /// The chunk without the shell metadata it carried: the chunk itself when it carried none.
    filtered: std::borrow::Cow<'chunk, [u8]>,
    events: Vec<PaneShellMetadataEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneShellMetadataTerminator {
    Bell,
    StringTerminator,
}

#[derive(Debug, Default)]
struct PaneShellMetadataParser {
    state: PaneShellMetadataParserState,
    token: Option<String>,
}

#[derive(Debug, Default)]
enum PaneShellMetadataParserState {
    #[default]
    Ground,
    Escape,
    Osc {
        payload: Vec<u8>,
        saw_escape: bool,
    },
}

impl PaneShellMetadataParser {
    fn process_chunk<'chunk>(
        &mut self,
        chunk: &'chunk [u8],
    ) -> PaneShellMetadataParseOutput<'chunk> {
        // Outside any escape, a chunk that begins no OSC and does not end inside an escape
        // carries no metadata and leaves the parser as it was: it passes through as is.
        if matches!(self.state, PaneShellMetadataParserState::Ground)
            && chunk.last() != Some(&0x1b)
            && memchr::memmem::find(chunk, b"\x1b]").is_none()
        {
            return PaneShellMetadataParseOutput {
                filtered: std::borrow::Cow::Borrowed(chunk),
                events: Vec::new(),
            };
        }
        let mut out = PaneShellMetadataParseOutput {
            filtered: std::borrow::Cow::Owned(Vec::with_capacity(chunk.len())),
            events: Vec::new(),
        };
        let filtered = out.filtered.to_mut();

        for byte in chunk {
            match &mut self.state {
                PaneShellMetadataParserState::Ground => {
                    if *byte == 0x1b {
                        self.state = PaneShellMetadataParserState::Escape;
                    } else {
                        filtered.push(*byte);
                    }
                }
                PaneShellMetadataParserState::Escape => {
                    if *byte == b']' {
                        self.state = PaneShellMetadataParserState::Osc {
                            payload: Vec::new(),
                            saw_escape: false,
                        };
                    } else {
                        filtered.push(0x1b);
                        if *byte == 0x1b {
                            self.state = PaneShellMetadataParserState::Escape;
                        } else {
                            filtered.push(*byte);
                            self.state = PaneShellMetadataParserState::Ground;
                        }
                    }
                }
                PaneShellMetadataParserState::Osc {
                    payload,
                    saw_escape,
                } => {
                    if *saw_escape {
                        if *byte == b'\\' {
                            let payload = std::mem::take(payload);
                            if let Some(event) =
                                decode_bound_shell_metadata(&payload, self.token.as_deref())
                            {
                                out.events.push(event);
                            } else {
                                append_raw_osc_sequence(
                                    filtered,
                                    &payload,
                                    PaneShellMetadataTerminator::StringTerminator,
                                );
                            }
                            self.state = PaneShellMetadataParserState::Ground;
                            continue;
                        }
                        payload.push(0x1b);
                        *saw_escape = false;
                    }

                    if *byte == 0x1b {
                        *saw_escape = true;
                    } else if *byte == 0x07 {
                        let payload = std::mem::take(payload);
                        if let Some(event) =
                            decode_bound_shell_metadata(&payload, self.token.as_deref())
                        {
                            out.events.push(event);
                        } else {
                            append_raw_osc_sequence(
                                filtered,
                                &payload,
                                PaneShellMetadataTerminator::Bell,
                            );
                        }
                        self.state = PaneShellMetadataParserState::Ground;
                    } else {
                        payload.push(*byte);
                        if payload.len() > MAX_SHELL_METADATA_PAYLOAD_BYTES {
                            filtered.push(0x1b);
                            filtered.push(b']');
                            filtered.extend_from_slice(payload);
                            self.state = PaneShellMetadataParserState::Ground;
                        }
                    }
                }
            }
        }

        out
    }
}

fn append_raw_osc_sequence(
    output: &mut Vec<u8>,
    payload: &[u8],
    terminator: PaneShellMetadataTerminator,
) {
    output.push(0x1b);
    output.push(b']');
    output.extend_from_slice(payload);
    match terminator {
        PaneShellMetadataTerminator::Bell => output.push(0x07),
        PaneShellMetadataTerminator::StringTerminator => {
            output.push(0x1b);
            output.push(b'\\');
        }
    }
}

fn decode_bound_shell_metadata(
    payload: &[u8],
    token: Option<&str>,
) -> Option<PaneShellMetadataEvent> {
    let Some(token) = token else {
        return decode_shell_metadata_payload(payload);
    };
    let text = std::str::from_utf8(payload).ok()?;
    let rest = text.strip_prefix(BMUX_SHELL_METADATA_PREFIX)?;
    let rest = rest.strip_prefix(token)?.strip_prefix(';')?;
    if rest == "nested-ready" {
        return Some(PaneShellMetadataEvent::NestedReplay);
    }
    // Only a marked payload may report: printed output cannot know the marker.
    if let Some(report) = rest.strip_prefix("agent;") {
        return decode_agent_report(report);
    }
    decode_shell_metadata_payload(format!("{BMUX_SHELL_METADATA_PREFIX}{rest}").as_bytes())
}

fn decode_shell_metadata_payload(payload: &[u8]) -> Option<PaneShellMetadataEvent> {
    let payload = std::str::from_utf8(payload).ok()?;
    let payload = payload.strip_prefix(BMUX_SHELL_METADATA_PREFIX)?;
    let mut fields = payload.split(';');
    let kind = fields.next()?;
    match kind {
        "start" => {
            let command = decode_shell_metadata_field(fields.next()?)?;
            let cwd = decode_shell_metadata_field(fields.next()?)?;
            if fields.next().is_some() || command.trim().is_empty() || cwd.trim().is_empty() {
                return None;
            }
            Some(PaneShellMetadataEvent::CommandStart { command, cwd })
        }
        "prompt" => {
            let cwd = decode_shell_metadata_field(fields.next()?)?;
            if fields.next().is_some() || cwd.trim().is_empty() {
                return None;
            }
            Some(PaneShellMetadataEvent::Prompt { cwd })
        }
        _ => None,
    }
}

/// Longest agent name and message a report may carry.
const MAX_AGENT_REPORT_NAME: usize = 64;
const MAX_AGENT_REPORT_MESSAGE: usize = 512;

/// `<state>[;<base64 agent>[;<base64 message>]]`; an empty field is absent.
fn decode_agent_report(report: &str) -> Option<PaneShellMetadataEvent> {
    let mut fields = report.split(';');
    let state = fields.next()?;
    if !matches!(state, "working" | "blocked" | "idle" | "release") {
        return None;
    }
    let mut optional = |limit: usize| -> Option<Option<String>> {
        match fields.next() {
            None | Some("") => Some(None),
            Some(field) => {
                let text = decode_shell_metadata_field(field)?;
                let text = text.trim();
                (!text.chars().any(char::is_control))
                    .then(|| (!text.is_empty()).then(|| text.chars().take(limit).collect()))
            }
        }
    };
    let agent = optional(MAX_AGENT_REPORT_NAME)?;
    let message = optional(MAX_AGENT_REPORT_MESSAGE)?;
    if fields.next().is_some() {
        return None;
    }
    Some(PaneShellMetadataEvent::AgentReport {
        state: state.to_string(),
        agent,
        message,
    })
}

fn decode_shell_metadata_field(value: &str) -> Option<String> {
    let decoded = decode_base64(value)?;
    String::from_utf8(decoded).ok()
}

fn decode_base64(value: &str) -> Option<Vec<u8>> {
    let bytes = value.as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return None;
    }

    let mut out = Vec::with_capacity((bytes.len() / 4) * 3);
    let mut chunk_start = 0_usize;
    while chunk_start < bytes.len() {
        let a = decode_base64_value(bytes[chunk_start])?;
        let b = decode_base64_value(bytes[chunk_start + 1])?;
        let c_raw = bytes[chunk_start + 2];
        let d_raw = bytes[chunk_start + 3];

        let c = if c_raw == b'=' {
            0
        } else {
            decode_base64_value(c_raw)?
        };
        let d = if d_raw == b'=' {
            0
        } else {
            decode_base64_value(d_raw)?
        };

        let padding = usize::from(c_raw == b'=') + usize::from(d_raw == b'=');
        if padding > 0 && chunk_start + 4 != bytes.len() {
            return None;
        }

        out.push((a << 2) | (b >> 4));
        if padding < 2 {
            out.push((b << 4) | (c >> 2));
        }
        if padding == 0 {
            out.push((c << 6) | d);
        }

        chunk_start += 4;
    }

    Some(out)
}

const fn decode_base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

fn shell_kind_for_path(shell: &str) -> ShellKind {
    let basename = std::path::Path::new(shell)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(shell)
        .trim_start_matches('-')
        .to_ascii_lowercase();
    // Windows shells are executables such as `nu.exe`.
    let basename = basename
        .strip_suffix(".exe")
        .map_or_else(|| basename.clone(), str::to_owned);
    match basename.as_str() {
        "bash" => ShellKind::Bash,
        "zsh" => ShellKind::Zsh,
        "fish" => ShellKind::Fish,
        "nu" => ShellKind::Nu,
        _ => ShellKind::Other,
    }
}

fn configure_pane_terminal_env(
    command: &mut SpawnCommand,
    pane_term: &str,
    image_protocol: Option<&str>,
) {
    command.env("TERM", pane_term);
    command.env("TERM_PROGRAM", "bmux");
    command.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
    command.env("BMUX_PANE", "1");

    // Physical-host identity must not leak through the emulated pane boundary.
    command.env_remove("LC_TERMINAL");
    command.env_remove("LC_TERMINAL_VERSION");
    command.env_remove("KITTY_WINDOW_ID");
    command.env_remove("KITTY_LISTEN_ON");

    if let Some(protocol) = image_protocol {
        command.env("BMUX_IMAGE_PROTOCOL", protocol);
    } else {
        command.env_remove("BMUX_IMAGE_PROTOCOL");
    }
}

fn clear_managed_launch_env(command: &mut SpawnCommand) {
    // Launch authority belongs to this server, not nested ordinary servers
    // started by its shells or detached programs.
    command.env_remove("BMUX_MANAGED_EXECUTABLE_SHA256");
    for key in [
        "BMUX_MANAGED_TARGET_IDENTITY",
        "BMUX_MANAGED_TARGET_ID",
        "BMUX_MANAGED_IDLE_GRACE_SECONDS",
    ] {
        command.env_remove(key);
    }
}

fn configure_device_seal_broker_env(command: &mut SpawnCommand) {
    if switchy_env::var("BMUX_DEVICE_SEAL_SCOPE").is_ok_and(|scope| scope == "client") {
        if let Ok(exe) = crate::platform::files::current_executable() {
            command.env(
                "SSHENV_DEVICE_SEAL_COMMAND",
                format!("{} device-seal-broker", exe.display()),
            );
        }
        return;
    }

    command.env("SSHENV_DEVICE_SEAL_COMMAND", "");
}

fn configure_shell_integration_command(
    command: &mut SpawnCommand,
    shell: &str,
    integration_root: Option<&std::path::Path>,
    token: &str,
) -> Result<()> {
    let Some(integration_root) = integration_root else {
        return Ok(());
    };

    match shell_kind_for_path(shell) {
        ShellKind::Bash => {
            let rcfile_path = integration_root.join("bash").join("bmux.bashrc");
            write_shell_integration_file(
                &rcfile_path,
                &shell_integration_bash_rcfile().replace(
                    BMUX_SHELL_METADATA_PREFIX,
                    &format!("{BMUX_SHELL_METADATA_PREFIX}{token};"),
                ),
            )?;
            command.arg("--rcfile");
            command.arg(&rcfile_path);
        }
        ShellKind::Zsh => {
            let zdotdir = integration_root.join("zsh");
            let zshenv_path = zdotdir.join(".zshenv");
            let zshrc_path = zdotdir.join(".zshrc");
            write_shell_integration_file(&zshenv_path, shell_integration_zsh_env())?;
            write_shell_integration_file(
                &zshrc_path,
                &shell_integration_zsh_rc().replace(
                    BMUX_SHELL_METADATA_PREFIX,
                    &format!("{BMUX_SHELL_METADATA_PREFIX}{token};"),
                ),
            )?;
            command.env("ZDOTDIR", zdotdir.as_os_str());
            if let Ok(original_zdotdir) = switchy_env::var("ZDOTDIR")
                && !original_zdotdir.trim().is_empty()
            {
                command.env("BMUX_ORIG_ZDOTDIR", original_zdotdir);
            }
        }
        ShellKind::Fish => {
            let xdg_config_home = integration_root.join("fish-xdg");
            let fish_config_path = xdg_config_home.join("fish").join("config.fish");
            write_shell_integration_file(
                &fish_config_path,
                &shell_integration_fish_config().replace(
                    BMUX_SHELL_METADATA_PREFIX,
                    &format!("{BMUX_SHELL_METADATA_PREFIX}{token};"),
                ),
            )?;
            command.env("XDG_CONFIG_HOME", xdg_config_home.as_os_str());
            if let Ok(original_xdg_config_home) = switchy_env::var("XDG_CONFIG_HOME")
                && !original_xdg_config_home.trim().is_empty()
            {
                command.env("BMUX_ORIG_XDG_CONFIG_HOME", original_xdg_config_home);
            }
        }
        ShellKind::Nu => {
            let config_path = integration_root.join("nu").join("config.nu");
            write_shell_integration_file(
                &config_path,
                &shell_integration_nu_config().replace(
                    BMUX_SHELL_METADATA_PREFIX,
                    &format!("{BMUX_SHELL_METADATA_PREFIX}{token};"),
                ),
            )?;
            command.arg("--config");
            command.arg(&config_path);
        }
        ShellKind::Other => {}
    }

    Ok(())
}

fn write_shell_integration_file(path: &std::path::Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        switchy_fs::sync::create_dir_all(parent).with_context(|| {
            format!("failed creating shell integration dir {}", parent.display())
        })?;
        crate::platform::files::restrict_directory(parent)?;
    }
    if let Ok(existing) = switchy_fs::sync::read_to_string(path)
        && existing == contents
    {
        return Ok(());
    }
    switchy_fs::sync::write(path, contents)
        .with_context(|| format!("failed writing shell integration file {}", path.display()))
}

#[allow(clippy::literal_string_with_formatting_args)]
const fn shell_integration_bash_rcfile() -> &'static str {
    r#"if [[ -f "${HOME}/.bashrc" ]]; then
  source "${HOME}/.bashrc"
fi

if [[ -n "${BMUX_ROOT_SHELL_PID:-}" && "$BMUX_ROOT_SHELL_PID" != "$$" && -n "${BMUX_SHELL_CALLBACK:-}" ]]; then
  eval "$("$BMUX_SHELL_CALLBACK" __shell-integration bash)"
  return
fi
export BMUX_ROOT_SHELL_PID=$$

if [[ -z "${__BMUX_RESURRECTION_HOOK_INSTALLED:-}" ]]; then
  __BMUX_RESURRECTION_HOOK_INSTALLED=1

  __bmux_b64() {
    printf '%s' "$1" | base64 | tr -d '\r\n'
  }

  __bmux_emit_start() {
    local cmd_b64 cwd_b64
    cmd_b64="$(__bmux_b64 "$1")"
    cwd_b64="$(__bmux_b64 "$2")"
    printf '\033]633;bmux;start;%s;%s\007' "$cmd_b64" "$cwd_b64"
  }

  __bmux_emit_prompt() {
    local cwd_b64
    cwd_b64="$(__bmux_b64 "$1")"
    printf '\033]633;bmux;prompt;%s\007' "$cwd_b64"
  }

  __BMUX_READY_FOR_CMD=0
  __BMUX_ORIGINAL_PROMPT_COMMAND="${PROMPT_COMMAND:-}"

  __bmux_preexec_trap() {
    [[ -n "${__BMUX_EMIT_GUARD:-}" ]] && return
    [[ "${__BMUX_READY_FOR_CMD:-0}" != "1" ]] && return
    __BMUX_READY_FOR_CMD=0
    __BMUX_EMIT_GUARD=1

    local hist_line cmd
    hist_line="$(HISTTIMEFORMAT= builtin history 1 2>/dev/null)"
    cmd="${1:-$hist_line}"
    if [[ "$hist_line" =~ ^[[:space:]]*[0-9]+[[:space:]]+(.*)$ ]]; then
      cmd="${BASH_REMATCH[1]}"
    fi
    if [[ -z "$cmd" && -n "${BASH_COMMAND:-}" ]]; then
      cmd="${BASH_COMMAND}"
    fi
    if [[ -n "$cmd" ]]; then
      __bmux_emit_start "$cmd" "$PWD"
    fi

    __BMUX_EMIT_GUARD=
  }

  __bmux_precmd_hook() {
    [[ -n "${__BMUX_EMIT_GUARD:-}" ]] && return
    __BMUX_EMIT_GUARD=1
    # Keep DEBUG disarmed for the entire inherited prompt command, not only
    # our marker. Otherwise prompt work consumes the next command boundary.
    if [[ -n "$__BMUX_ORIGINAL_PROMPT_COMMAND" ]]; then
      eval "$__BMUX_ORIGINAL_PROMPT_COMMAND"
    fi
    __bmux_emit_prompt "$PWD"
    __BMUX_READY_FOR_CMD=1
    __BMUX_EMIT_GUARD=
  }

  # bash-preexec may install its dispatcher lazily at the first prompt and
  # replace DEBUG. Participate in its public hook array as well as the native
  # fallback; READY_FOR_CMD makes the two paths idempotent per command.
  __bmux_bp_preexec() {
    __BMUX_READY_FOR_CMD=1
    __bmux_preexec_trap "$1"
  }
  preexec_functions+=(__bmux_bp_preexec)
  trap '__bmux_preexec_trap' DEBUG
  PROMPT_COMMAND="__bmux_precmd_hook"
fi
"#
}

#[allow(clippy::literal_string_with_formatting_args)]
const fn shell_integration_zsh_env() -> &'static str {
    r#"if [[ -n "${BMUX_ORIG_ZDOTDIR:-}" ]]; then
  __bmux_orig_zdotdir="${BMUX_ORIG_ZDOTDIR}"
else
  __bmux_orig_zdotdir="${HOME}"
fi

if [[ -f "${__bmux_orig_zdotdir}/.zshenv" ]]; then
  source "${__bmux_orig_zdotdir}/.zshenv"
fi
"#
}

#[allow(clippy::literal_string_with_formatting_args)]
const fn shell_integration_zsh_rc() -> &'static str {
    r#"if [[ -n "${BMUX_ORIG_ZDOTDIR:-}" ]]; then
  __bmux_orig_zdotdir="${BMUX_ORIG_ZDOTDIR}"
else
  __bmux_orig_zdotdir="${HOME}"
fi

if [[ -f "${__bmux_orig_zdotdir}/.zshrc" ]]; then
  source "${__bmux_orig_zdotdir}/.zshrc"
fi

if [[ -n "${BMUX_ROOT_SHELL_PID:-}" && "$BMUX_ROOT_SHELL_PID" != "$$" && -n "${BMUX_SHELL_CALLBACK:-}" ]]; then
  eval "$("$BMUX_SHELL_CALLBACK" __shell-integration zsh)"
  return
fi
export BMUX_ROOT_SHELL_PID=$$

if [[ -z "${__BMUX_RESURRECTION_HOOK_INSTALLED:-}" ]]; then
  __BMUX_RESURRECTION_HOOK_INSTALLED=1

  __bmux_b64() {
    printf '%s' "$1" | base64 | tr -d '\r\n'
  }

  __bmux_emit_start() {
    local cmd_b64 cwd_b64
    cmd_b64="$(__bmux_b64 "$1")"
    cwd_b64="$(__bmux_b64 "$2")"
    printf '\033]633;bmux;start;%s;%s\007' "$cmd_b64" "$cwd_b64"
  }

  __bmux_emit_prompt() {
    local cwd_b64
    cwd_b64="$(__bmux_b64 "$1")"
    printf '\033]633;bmux;prompt;%s\007' "$cwd_b64"
  }

  function __bmux_preexec_hook() {
    [[ -n "${__BMUX_EMIT_GUARD:-}" ]] && return
    __BMUX_EMIT_GUARD=1
    if [[ -n "$1" ]]; then
      __bmux_emit_start "$1" "$PWD"
    fi
    __BMUX_EMIT_GUARD=
  }

  function __bmux_precmd_hook() {
    [[ -n "${__BMUX_EMIT_GUARD:-}" ]] && return
    __BMUX_EMIT_GUARD=1
    __bmux_emit_prompt "$PWD"
    __BMUX_EMIT_GUARD=
  }

  typeset -ga preexec_functions
  typeset -ga precmd_functions
  preexec_functions=(__bmux_preexec_hook ${preexec_functions:#__bmux_preexec_hook})
  precmd_functions=(__bmux_precmd_hook ${precmd_functions:#__bmux_precmd_hook})
fi
"#
}

#[allow(clippy::literal_string_with_formatting_args)]
const fn shell_integration_fish_config() -> &'static str {
    r#"set -l __bmux_orig_xdg "$BMUX_ORIG_XDG_CONFIG_HOME"
if test -z "$__bmux_orig_xdg"
  set __bmux_orig_xdg "$HOME/.config"
end

# The private XDG directory is only for loading this integration file. Restore
# the user directory before their config or child applications observe it.
if test -n "$BMUX_ORIG_XDG_CONFIG_HOME"
  set -gx XDG_CONFIG_HOME "$BMUX_ORIG_XDG_CONFIG_HOME"
else
  set -e XDG_CONFIG_HOME
end

set -l __bmux_orig_fish_config "$__bmux_orig_xdg/fish/config.fish"
if test -f "$__bmux_orig_fish_config"
  source "$__bmux_orig_fish_config"
end

if not set -q __bmux_resurrection_hook_installed
  set -g __bmux_resurrection_hook_installed 1

  function __bmux_b64 --argument-names value
    # Fish command substitutions preserve wrapped lines as separate list items.
    printf '%s' "$value" | base64 | string replace -ra '\n|\r' '' | string join ''
  end

  function __bmux_emit_start --argument-names cmd cwd
    set -l cmd_b64 (__bmux_b64 "$cmd")
    set -l cwd_b64 (__bmux_b64 "$cwd")
    printf '\e]633;bmux;start;%s;%s\a' "$cmd_b64" "$cwd_b64"
  end

  function __bmux_emit_prompt --argument-names cwd
    set -l cwd_b64 (__bmux_b64 "$cwd")
    printf '\e]633;bmux;prompt;%s\a' "$cwd_b64"
  end

  function __bmux_preexec --on-event fish_preexec
    set -l cmd (string join ' ' -- $argv)
    if test -n "$cmd"
      __bmux_emit_start "$cmd" "$PWD"
    end
  end

  function __bmux_prompt --on-event fish_prompt
    __bmux_emit_prompt "$PWD"
  end
end
"#
}

/// `XDG_DATA_DIRS` for a pane: its own nested-shell share first, then the
/// inherited directories without other bmux panes' shares. A server started
/// inside a bmux pane inherits that pane's share, whose integration files
/// would otherwise install a second set of shell hooks bound to the outer
/// runtime's version of them.
fn pane_xdg_data_dirs(
    share: std::path::PathBuf,
    inherited: Option<std::ffi::OsString>,
) -> Vec<std::path::PathBuf> {
    let mut dirs = vec![share];
    match inherited {
        Some(inherited) => {
            dirs.extend(std::env::split_paths(&inherited).filter(|dir| !is_bmux_nested_share(dir)));
        }
        // The XDG default, spelled out: the list separator differs per platform.
        None => dirs.extend(["/usr/local/share", "/usr/share"].map(std::path::PathBuf::from)),
    }
    dirs
}

/// Whether `dir` is a pane's nested-shell share
/// (`…/shell-integration/<marker>/nested-share`).
fn is_bmux_nested_share(dir: &std::path::Path) -> bool {
    dir.file_name() == Some(std::ffi::OsStr::new("nested-share"))
        && dir
            .parent()
            .and_then(std::path::Path::parent)
            .and_then(std::path::Path::file_name)
            == Some(std::ffi::OsStr::new("shell-integration"))
}

#[allow(clippy::literal_string_with_formatting_args)]
const fn shell_integration_nested_nu() -> &'static str {
    r"# Install once per Nushell process, like the Bash and Zsh integrations: an
# inherited XDG_DATA_DIRS can list more than one bmux share directory, and
# duplicate hooks would report every lifecycle boundary twice.
if (($env.APPLICATION_RESURRECTION_HOST? | default '') != '') and (($env.BMUX_SHELL_CALLBACK? | default '') != '') and (($env.__BMUX_NESTED_PID? | default '') != ($nu.pid | into string)) {
  $env.__BMUX_NESTED_PID = ($nu.pid | into string)
  $env.config = ($env.config
    | upsert hooks.pre_prompt (($env.config.hooks.pre_prompt? | default []) | append {||
        let cwd = (^$env.BMUX_SHELL_CALLBACK __shell-invocation location $nu.current-exe ($nu.pid | into string) '' ($env.PWD | into string))
        if (($cwd | str length) > 0) { cd $cwd }
        ^$env.BMUX_SHELL_CALLBACK __shell-invocation prompt $nu.current-exe ($nu.pid | into string) '' ($env.PWD | into string)
      })
    | upsert hooks.pre_execution (($env.config.hooks.pre_execution? | default []) | append {||
        let text = (commandline)
        if (($text | str trim | str length) > 0) {
          ^$env.BMUX_SHELL_CALLBACK __shell-invocation start $nu.current-exe ($nu.pid | into string) $text ($env.PWD | into string)
        }
      }))
}
"
}

#[allow(clippy::literal_string_with_formatting_args)]
const fn shell_integration_nu_config() -> &'static str {
    r#"const __bmux_user_config_path = ($nu.default-config-dir | path join "config.nu")
const __bmux_user_config = if ($__bmux_user_config_path | path exists) { $__bmux_user_config_path } else { null }
source $__bmux_user_config

def __bmux_hook_list [value] {
  let kind = ($value | describe)
  if ($kind | str starts-with "list<") {
    $value
  } else if $kind == "nothing" {
    []
  } else {
    [$value]
  }
}

def __bmux_marker [kind: string, fields: list<string>] {
  let esc = (char -u "1b")
  let bel = (char bel)
  let encoded = ($fields | each {|field| $field | encode base64 } | str join ";")
  if (($encoded | str length) == 0) {
    $"($esc)]633;bmux;($kind)($bel)"
  } else {
    $"($esc)]633;bmux;($kind);($encoded)($bel)"
  }
}

def __bmux_emit_start [command: string, cwd: string] {
  print --no-newline (__bmux_marker "start" [$command $cwd])
}

def __bmux_emit_prompt [cwd: string] {
  print --no-newline (__bmux_marker "prompt" [$cwd])
}

let __bmux_pre_execution_hooks = (__bmux_hook_list ($env.config | get -o hooks.pre_execution))
let __bmux_pre_prompt_hooks = (__bmux_hook_list ($env.config | get -o hooks.pre_prompt))

$env.config = (
  $env.config
  | upsert hooks.pre_prompt (
      $__bmux_pre_prompt_hooks
      | append {|| __bmux_emit_prompt ($env.PWD | into string) }
    )
  | upsert hooks.pre_execution (
      $__bmux_pre_execution_hooks
      | append {||
          let command = (commandline)
          if (($command | str trim | str length) > 0) {
            __bmux_emit_start $command ($env.PWD | into string)
          }
        }
    )
)
"#
}

impl PaneInputHandle {
    fn send_input(&self, data: Vec<u8>) -> std::result::Result<(), SessionRuntimeError> {
        let host = &self.host;
        if self.exited.load(Ordering::SeqCst) {
            return Err(SessionRuntimeError::Closed);
        }
        self.input_tx
            .send(input_authority::settled_local_command(
                host,
                data,
                self.session_id,
            )?)
            .map_err(|_| SessionRuntimeError::Closed)
    }
}

impl PaneRuntimeHandle {
    fn input_handle(&self, session_id: SessionId) -> PaneInputHandle {
        PaneInputHandle {
            execution_token: self
                .resurrection_state
                .lock()
                .ok()
                .map(|state| state.observation_execution),
            session_id,
            input_tx: self.input_tx.clone(),
            exited: Arc::clone(&self.exited),
            host: self.host.clone(),
        }
    }

    fn send_input(
        &self,
        data: Vec<u8>,
        session_id: SessionId,
    ) -> std::result::Result<(), SessionRuntimeError> {
        let host = self.host.clone();
        let host = &host;
        self.input_tx
            .send(input_authority::settled_local_command(
                host, data, session_id,
            )?)
            .map_err(|_| SessionRuntimeError::Closed)
    }

    // Called with last_requested_size/grid held. The legacy resize consumer
    // takes the same size lock before touching the master.
    fn resize_uncaptured_master(
        &self,
        rows: u16,
        cols: u16,
    ) -> Result<(), bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalExecutionError>
    {
        let master = self.execution.master.lock().map_err(execution::failed)?;
        master
            .as_ref()
            .ok_or_else(|| execution::failed("PTY master is unavailable"))?
            .resize(PtySize { rows, cols })
            .map_err(execution::failed)
    }

    fn resize_pty(&self, rows: u16, cols: u16) {
        self.resize_pty_admitted(rows, cols, None);
    }

    fn dispatch_uncaptured_resize(
        &self,
        rows: u16,
        cols: u16,
        permit: Option<tokio::sync::OwnedSemaphorePermit>,
    ) {
        let command = match permit {
            Some(permit) => PaneRuntimeCommand::GuardedResize {
                rows,
                cols,
                _permit: permit,
            },
            None => PaneRuntimeCommand::Resize { rows, cols },
        };
        let captured = self.execution.writer().ok().flatten().is_some()
            || self
                .execution
                .durable_capture
                .lock()
                .is_ok_and(|capture| capture.is_some());
        if !captured {
            let _ = self.input_tx.send(command);
        }
    }

    // Caller retains geometry, grid, output and capture locks throughout this
    // commit, preserving the reader/resize ordering across this private helper.
    fn commit_captured_resize(
        &self,
        capture: &mut Option<durable_capture::DurableCapture>,
        grid: &mut TerminalGridStream,
        rows: u16,
        cols: u16,
    ) -> bool {
        if let Some(capture) = capture.as_mut() {
            let resized = self.execution.master.lock().ok().and_then(|master| {
                master
                    .as_ref()
                    .map(|master| master.resize(PtySize { rows, cols }))
            });
            if !matches!(resized, Some(Ok(()))) {
                return false;
            }
            if let Err(error) = capture.resize(cols, rows) {
                let _ = capture.end(Some(error.to_string()));
                return false;
            }
        }
        if let Err(error) = grid.resize(cols, rows) {
            if let Some(capture) = capture.as_mut() {
                let _ = capture.end(Some(format!(
                    "retained geometry could not commit: {error:?}"
                )));
            }
            return false;
        }
        true
    }

    /// Tell clients the pane's grid changed, as new output does: once until they read it.
    fn notify_grid_changed(&self) {
        if self
            .output_dirty
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            self.host.publish_pane_event(PaneEvent::OutputAvailable {
                session_id: self.session_id.0,
                pane_id: self.meta.id,
            });
            crate::handlers::output_push::pane_output_available(
                &self.host,
                self.session_id.0,
                self.meta.id,
            );
        }
    }

    #[cfg(feature = "image-registry")]
    fn publish_resized_images(&self) {
        if self
            .image_dirty
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            self.host.publish_pane_event(PaneEvent::ImageAvailable {
                session_id: self.session_id.0,
                pane_id: self.meta.id,
            });
        }
    }

    fn lock_resize_capture_order(
        &self,
    ) -> std::io::Result<switchy_thread::sync::MutexGuard<'_, ()>> {
        let order = self
            .execution
            .capture_order
            .lock()
            .map_err(|_| std::io::Error::other("capture ordering lock poisoned"))?;
        if let Some(writer) = self.execution.writer()? {
            let boundary = writer.progress()?.admitted.raw_end;
            writer.wait_projected_raw(boundary, Duration::from_secs(2))?;
        }
        let boundary = self
            .execution
            .durable_capture
            .lock()
            .map_err(|_| std::io::Error::other("capture authority lock poisoned"))?
            .as_ref()
            .map(durable_capture::DurableCapture::raw_bytes);
        if let Some(boundary) = boundary {
            self.execution.wait_for_projection(boundary)?;
        }
        Ok(order)
    }

    fn resize_pty_admitted(
        &self,
        rows: u16,
        cols: u16,
        permit: Option<tokio::sync::OwnedSemaphorePermit>,
    ) {
        // Managed ordered captures use execution::resize, which prepares the
        // fence outside the runtime manager. Never wait for disk here.
        if self.execution.writer().ok().flatten().is_some() {
            self.execution
                .record_error("ordered capture resize requires execution control path".into());
            return;
        }
        let Ok(_order) = self.lock_resize_capture_order() else {
            return;
        };
        let mut ticket = None;
        let _ = self.resize_pty_under_order(rows, cols, permit, None, &mut ticket, None);
    }

    fn publish_ordered_resize(
        &self,
        reservation: capture_writer::Reservation,
        grid: &mut TerminalGridStream,
        rows: u16,
        cols: u16,
    ) -> Option<capture_writer::Ticket> {
        let resized = self.execution.master.lock().is_ok_and(|master| {
            master
                .as_ref()
                .is_some_and(|master| master.resize(PtySize { rows, cols }).is_ok())
        });
        if !resized || grid.resize(cols, rows).is_err() {
            self.execution
                .fail_capture("ordered geometry side effect failed; recovery required");
            return None;
        }
        match reservation.resize(cols, rows) {
            Ok(admitted) => {
                if let Ok(Some(writer)) = self.execution.writer()
                    && let Err(error) = writer.projected(admitted)
                {
                    self.execution.fail_capture(&error.to_string());
                    return None;
                }
                Some(admitted)
            }
            Err(error) => {
                self.execution
                    .fail_capture(&format!("geometry admission failed after resize: {error}"));
                None
            }
        }
    }

    // Caller owns capture_order and has drained the admitted live prefix before
    // entering the runtime manager. All capacity was reserved outside locks.
    // Keep geometry, image, parser, and PTY publication in one lock-ordered
    // transaction; splitting this sequence would obscure the resize barrier.
    #[allow(clippy::too_many_lines)]
    fn resize_pty_under_order(
        &self,
        rows: u16,
        cols: u16,
        permit: Option<tokio::sync::OwnedSemaphorePermit>,
        reservation: Option<capture_writer::Reservation>,
        ticket: &mut Option<capture_writer::Ticket>,
        expected_grid_revision: Option<u64>,
    ) -> Result<(), bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalExecutionError>
    {
        // Serialize geometry publication with the reader's entire output chunk.
        // The reader acquires size before image state, grid, and output.
        let Ok(mut last) = self.last_requested_size.lock() else {
            return Err(execution::failed("geometry lock poisoned"));
        };
        // The size last requested: the grid's geometry once the reader has consumed what
        // preceded this resize.
        #[cfg(feature = "image-registry")]
        let (previous_rows, previous_cols) = *last;
        // Pinned captures hold image state before grid/output. Participate in
        // that ordering so a resize cannot bisect a combined capture.
        #[cfg(feature = "image-registry")]
        let Ok(mut images) = self.image_registry.lock() else {
            return Err(execution::failed("image lock poisoned"));
        };
        #[cfg(feature = "image-registry")]
        let mut images_changed = false;
        if let Ok(mut grid) = self.terminal_grid.lock() {
            if let Some(revision) = expected_grid_revision {
                execution::validate_alternate_source(grid.grid(), revision)?;
                if *last == (rows, cols) {
                    if self
                        .execution
                        .writer()
                        .map_err(execution::failed)?
                        .is_none()
                        && self
                            .execution
                            .durable_capture
                            .lock()
                            .map_err(execution::failed)?
                            .is_none()
                    {
                        // A legacy same-size request may still be queued. Ack
                        // only after the actual PTY has accepted this geometry.
                        self.resize_uncaptured_master(rows, cols)?;
                    }
                    return Ok(());
                }
            }
            // The retained parser and output watermark must have consumed all
            // capture-ahead bytes before geometry can enter the same stream.
            // Hold grid -> output -> capture, matching output publication.
            let Ok(_output) = self.output_buffer.lock() else {
                return Err(execution::failed("output lock poisoned"));
            };
            let Ok(mut capture) = self.execution.durable_capture.lock() else {
                return Err(execution::failed("capture lock poisoned"));
            };
            if capture.as_ref().is_some_and(|capture| {
                capture.raw_bytes() != self.execution.output_watermark.load(Ordering::SeqCst)
            }) {
                return Err(execution::failed("capture prefix is not projected"));
            }
            #[cfg(feature = "image-registry")]
            if (cols != previous_cols || rows != previous_rows)
                && rows > 0
                && cols > 0
                && images.has_main_images()
            {
                // Stage geometry before committing image projection and grids.
                let mut budget = RESPONSE_OUTPUT_BUDGET;
                let Some(mut resized) = grid.grid().try_clone_charged(&mut budget) else {
                    return Err(execution::failed("image reflow exceeds response budget"));
                };
                let Ok(shift) = resized.resize_with_main_row_shift(cols, rows) else {
                    return Err(execution::failed("image reflow geometry rejected"));
                };
                if let Err(error) = images.remap_positions(rows, |row, column| {
                    if let Some(shift) = shift {
                        return row
                            .checked_sub(shift)
                            .map(|row| (row, column))
                            .ok_or_else(|| std::io::Error::other("image resize anchor overflow"));
                    }
                    let (row, column) = grid
                        .grid()
                        .retained_position_after_resize(
                            row,
                            usize::from(column),
                            cols,
                            rows,
                            &mut budget,
                        )
                        .map_err(|error| {
                            std::io::Error::other(format!("image reflow anchor: {error:?}"))
                        })?;
                    Ok((row, u16::try_from(column).map_err(std::io::Error::other)?))
                }) {
                    warn!(%error, "image resize failed; retaining previous geometry");
                    return Err(execution::failed(error));
                }
                images
                    .evict_main_history(resized.main_row_count().saturating_sub(resized.height()));
                images_changed = true;
            }
            #[cfg(feature = "image-registry")]
            if rows > 0 && cols > 0 && rows != previous_rows && !images_changed {
                let revision = images.sequence();
                if let Err(error) = images.resize_alternate_viewport(rows) {
                    warn!(%error, "alternate image resize failed; retaining previous geometry");
                    return Err(execution::failed(error));
                }
                images_changed = images.sequence() != revision;
            }
            if let Some(reservation) = reservation {
                let Some(admitted) =
                    self.publish_ordered_resize(reservation, &mut grid, rows, cols)
                else {
                    return Err(execution::failed("ordered geometry could not commit"));
                };
                *ticket = Some(admitted);
            } else if expected_grid_revision.is_some() && capture.is_none() {
                // Ordinary local panes have no capture writer. Commit PTY and
                // source geometry synchronously under the reader's size/grid
                // locks; queue admission alone cannot acknowledge a resize.
                self.resize_uncaptured_master(rows, cols)?;
                if let Err(error) = grid.resize(cols, rows) {
                    self.execution.record_error(format!(
                        "source geometry failed after PTY resize: {error:?}"
                    ));
                    return Err(execution::failed(
                        "source geometry could not commit after PTY resize",
                    ));
                }
            } else if !self.commit_captured_resize(&mut capture, &mut grid, rows, cols) {
                return Err(execution::failed("terminal geometry could not commit"));
            }
        } else {
            return Err(execution::failed("grid lock poisoned"));
        }
        *last = (rows, cols);
        self.execution
            .geometry_revision
            .fetch_add(1, Ordering::SeqCst);
        self.execution.notify();
        // The grid changed without output: clients replicating it take its new geometry now,
        // not when the program next writes.
        self.notify_grid_changed();
        if expected_grid_revision.is_none() {
            self.dispatch_uncaptured_resize(rows, cols, permit);
        }
        #[cfg(feature = "image-registry")]
        drop(images);
        drop(last);
        #[cfg(feature = "image-registry")]
        if images_changed {
            self.publish_resized_images();
        }
        Ok(())
    }
}

/// Where one client's replica of a pane's grid is: its cursor, and the grid revision it was last
/// sent a frame at (a grid that has not changed since needs no frame).
#[derive(Debug, Clone, Default)]
struct PaneFrameReplica {
    cursor: bmux_terminal_grid::ReplicaCursor,
    revision: u64,
    /// `bmux-bench replay` markers read since its last frame (only while replay is logged).
    replay_markers: Vec<(u64, u128)>,
    /// Clipboard writes (selection, text) the pane's program made since its last frame.
    clipboard_writes: Vec<(String, String)>,
}

#[derive(Clone)]
struct PaneAttachDataRef {
    pane_id: Uuid,
    name: Option<String>,
    exited: Arc<AtomicBool>,
    exit_reason: Arc<switchy_thread::sync::Mutex<Option<String>>>,
    output_buffer: Arc<switchy_thread::sync::Mutex<OutputFanoutBuffer>>,
    terminal_grid: Arc<switchy_thread::sync::Mutex<TerminalGridStream>>,
    /// Each attached client's replica of the grid: what it was last sent frames from.
    frame_replicas: Arc<switchy_thread::sync::Mutex<BTreeMap<ClientId, PaneFrameReplica>>>,
    output_dirty: Arc<AtomicBool>,
    mouse_protocol_state: Arc<switchy_thread::sync::Mutex<AttachMouseProtocolState>>,
    input_mode_state: Arc<switchy_thread::sync::Mutex<AttachInputModeState>>,
    #[cfg(feature = "image-registry")]
    image_registry: Arc<switchy_thread::sync::Mutex<bmux_image::ImageRegistry>>,
    #[cfg(feature = "image-registry")]
    image_dirty: Arc<AtomicBool>,
}

struct AttachSceneDataSnapshot {
    focused_pane_id: Uuid,
    panes: Vec<PaneAttachDataRef>,
    layout_root: bmux_attach_layout_protocol::PaneLayoutNode,
    scene: AttachScene,
    zoomed: bool,
}

#[derive(Clone)]
struct PanePersistenceDataRef {
    meta: PaneRuntimeMeta,
    exited: Arc<AtomicBool>,
    process_id: Arc<switchy_thread::sync::Mutex<Option<u32>>>,
    process_group_id: Arc<switchy_thread::sync::Mutex<Option<i32>>>,
    process_start_identity: Arc<switchy_thread::sync::Mutex<Option<ProcessStartIdentity>>>,
    resurrection_state: Arc<switchy_thread::sync::Mutex<PaneResurrectionRuntime>>,
}

struct PersistenceSnapshotParts {
    focused_pane_id: Uuid,
    zoomed_pane_id: Option<Uuid>,
    layout_root: Option<PaneLayoutNode>,
    floating_surfaces: Vec<FloatingSurfaceRuntime>,
    attached_clients: BTreeSet<ClientId>,
    attach_viewport: Option<AttachViewport>,
    panes: Vec<PanePersistenceDataRef>,
}

fn pane_persistence_data_ref(pane: &PaneRuntimeHandle) -> PanePersistenceDataRef {
    PanePersistenceDataRef {
        meta: pane.meta.clone(),
        exited: Arc::clone(&pane.exited),
        process_id: Arc::clone(&pane.process_id),
        process_group_id: Arc::clone(&pane.process_group_id),
        process_start_identity: Arc::clone(&pane.process_start_identity),
        resurrection_state: Arc::clone(&pane.resurrection_state),
    }
}

fn pane_attach_data_ref(
    _owner_session_id: SessionId,
    pane_id: Uuid,
    pane: &PaneRuntimeHandle,
) -> PaneAttachDataRef {
    PaneAttachDataRef {
        pane_id,
        name: pane.meta.name.clone(),
        exited: Arc::clone(&pane.exited),
        exit_reason: Arc::clone(&pane.exit_reason),
        output_buffer: Arc::clone(&pane.output_buffer),
        terminal_grid: Arc::clone(&pane.terminal_grid),
        frame_replicas: Arc::clone(&pane.frame_replicas),
        output_dirty: Arc::clone(&pane.output_dirty),
        mouse_protocol_state: Arc::clone(&pane.mouse_protocol_state),
        input_mode_state: Arc::clone(&pane.input_mode_state),
        #[cfg(feature = "image-registry")]
        image_registry: Arc::clone(&pane.image_registry),
        #[cfg(feature = "image-registry")]
        image_dirty: Arc::clone(&pane.image_dirty),
    }
}

fn pane_summary_for_data_ref(
    index: usize,
    focused_pane_id: Uuid,
    pane: &PaneAttachDataRef,
) -> PaneSummary {
    PaneSummary {
        id: pane.pane_id,
        index: u32::try_from(index.saturating_add(1)).unwrap_or(u32::MAX),
        name: pane.name.clone(),
        focused: pane.pane_id == focused_pane_id,
        state: if pane.exited.load(Ordering::SeqCst) {
            PaneState::Exited
        } else {
            PaneState::Running
        },
        state_reason: pane
            .exit_reason
            .lock()
            .ok()
            .and_then(|reason| reason.clone()),
    }
}

fn attach_pane_chunk_for_client(
    pane: &PaneAttachDataRef,
    client_id: ClientId,
    max_bytes: usize,
) -> Result<AttachPaneChunk, SessionRuntimeError> {
    attach_pane_chunk_and_remainder_for_client(pane, client_id, max_bytes).map(|(chunk, _)| chunk)
}

/// A client's next chunk of pane output, and whether output it has not read remains buffered
/// (the read stopped at its byte budget). The chunk says whether a synchronized update is open
/// at its own end, not at the latest output.
fn attach_pane_chunk_and_remainder_for_client(
    pane: &PaneAttachDataRef,
    client_id: ClientId,
    max_bytes: usize,
) -> Result<(AttachPaneChunk, bool), SessionRuntimeError> {
    let mut output = pane
        .output_buffer
        .lock()
        .map_err(|_| SessionRuntimeError::Closed)?;
    let read = output.read_for_client(client_id, max_bytes);
    let sync_update_active = output.sync_update_open_at(read.stream_end);
    let remainder = read.stream_end < output.end_offset();
    drop(output);
    Ok((
        AttachPaneChunk {
            pane_id: pane.pane_id,
            data: read.bytes,
            stream_start: read.stream_start,
            stream_end: read.stream_end,
            stream_gap: read.stream_gap,
            sync_update_active,
        },
        remainder,
    ))
}

fn attach_pane_recent_chunk(
    pane: &PaneAttachDataRef,
    client_id: ClientId,
    max_bytes: usize,
) -> Result<AttachPaneChunk, SessionRuntimeError> {
    let (read, sync_update_active) = {
        let mut output = pane
            .output_buffer
            .lock()
            .map_err(|_| SessionRuntimeError::Closed)?;
        let read = output.read_recent_with_offsets(max_bytes);
        output.advance_client_to_end(client_id);
        let sync_update_active = output.sync_update_open_at(read.stream_end);
        (read, sync_update_active)
    };
    pane.output_dirty.store(false, Ordering::SeqCst);
    Ok(AttachPaneChunk {
        pane_id: pane.pane_id,
        data: read.bytes,
        stream_start: read.stream_start,
        stream_end: read.stream_end,
        stream_gap: read.stream_gap,
        sync_update_active,
    })
}

impl SessionRuntimeManager {
    fn bump_attach_view_revision(&mut self, session_id: SessionId) -> Option<u64> {
        let runtime = self.runtimes.get_mut(&session_id)?;
        runtime.attach_view_revision = runtime.attach_view_revision.saturating_add(1);
        Some(runtime.attach_view_revision)
    }

    fn pane_outer_rect(&self, session_id: SessionId, pane_id: Uuid) -> Option<AttachRect> {
        let runtime = self.runtimes.get(&session_id)?;
        if runtime.zoomed_pane_id == Some(pane_id) {
            return Some(attach_rect_from_layout_rect(scene_root_from_viewport(
                runtime.attach_viewport,
            )));
        }
        if let Some(surface) = runtime
            .floating_surfaces
            .iter()
            .find(|surface| surface.pane_id == pane_id)
        {
            return Some(attach_rect_from_layout_rect(surface.rect));
        }
        let root = scene_root_from_viewport(runtime.attach_viewport);
        let mut rects = BTreeMap::new();
        if let Some(layout) = &runtime.layout_root {
            collect_layout_rects(layout, root, &mut rects);
        }
        rects
            .get(&pane_id)
            .copied()
            .map(attach_rect_from_layout_rect)
    }

    fn pane_session_id(&self, pane_id: Uuid) -> Option<SessionId> {
        self.runtimes.iter().find_map(|(session_id, runtime)| {
            runtime.panes.contains_key(&pane_id).then_some(*session_id)
        })
    }

    fn preview_states(
        &self,
        token: Uuid,
    ) -> Result<Vec<crate::padding_api::RuntimePanePaddingState>, SessionRuntimeError> {
        let preview = self
            .padding_previews
            .get(&token)
            .ok_or(SessionRuntimeError::NotFound)?;
        preview
            .target_pane_ids
            .iter()
            .map(|pane_id| {
                let session_id = self
                    .pane_session_id(*pane_id)
                    .ok_or(SessionRuntimeError::NotFound)?;
                self.padding_state(session_id, Some(*pane_id))
            })
            .collect()
    }

    fn drop_padding_preview_targets(
        previews: &mut BTreeMap<Uuid, PanePaddingPreviewTransaction>,
        pane_ids: &BTreeSet<Uuid>,
    ) {
        for preview in previews.values_mut() {
            preview
                .target_pane_ids
                .retain(|pane_id| !pane_ids.contains(pane_id));
        }
        previews.retain(|_, preview| !preview.target_pane_ids.is_empty());
    }

    fn apply_preview_spec_to_targets(
        &mut self,
        target_pane_ids: &BTreeSet<Uuid>,
        spec: Option<PanePaddingSpec>,
    ) -> Result<(), SessionRuntimeError> {
        let targets = target_pane_ids
            .iter()
            .map(|pane_id| {
                let session_id = self
                    .pane_session_id(*pane_id)
                    .ok_or(SessionRuntimeError::NotFound)?;
                let before = self.padding_state(session_id, Some(*pane_id))?;
                Ok((session_id, *pane_id, before))
            })
            .collect::<Result<Vec<_>, SessionRuntimeError>>()?;
        for (session_id, pane_id, before) in targets {
            let pane = self
                .runtimes
                .get_mut(&session_id)
                .and_then(|runtime| runtime.panes.get_mut(&pane_id))
                .ok_or(SessionRuntimeError::NotFound)?;
            pane.padding_preview = spec;
            let after = self.padding_state(session_id, Some(pane_id))?;
            self.apply_padding_geometry_change(session_id, pane_id, &before, &after)?;
        }
        Ok(())
    }

    pub(crate) fn begin_padding_preview(
        &mut self,
        owner_client_id: ClientId,
        pane_ids: Vec<Uuid>,
        spec: PanePaddingSpec,
    ) -> Result<(Uuid, Vec<crate::padding_api::RuntimePanePaddingState>), SessionRuntimeError> {
        validate_spec(spec).map_err(|_| SessionRuntimeError::Closed)?;
        let target_pane_ids = pane_ids.into_iter().collect::<BTreeSet<_>>();
        if target_pane_ids.is_empty()
            || target_pane_ids
                .iter()
                .any(|pane_id| self.pane_session_id(*pane_id).is_none())
        {
            return Err(SessionRuntimeError::NotFound);
        }
        if self.padding_previews.values().any(|preview| {
            preview.owner_client_id != owner_client_id
                && !preview.target_pane_ids.is_disjoint(&target_pane_ids)
        }) {
            return Err(SessionRuntimeError::Closed);
        }
        let owned_tokens = self
            .padding_previews
            .iter()
            .filter_map(|(token, preview)| {
                (preview.owner_client_id == owner_client_id).then_some(*token)
            })
            .collect::<Vec<_>>();
        for token in owned_tokens {
            self.cancel_padding_preview(owner_client_id, token)?;
        }
        let token = crate::ids::new_uuid();
        self.padding_previews.insert(
            token,
            PanePaddingPreviewTransaction {
                owner_client_id,
                target_pane_ids: target_pane_ids.clone(),
                spec,
                lease_expires_at: Instant::now() + PANE_PADDING_PREVIEW_LEASE,
            },
        );
        self.apply_preview_spec_to_targets(&target_pane_ids, Some(spec))?;
        Ok((token, self.preview_states(token)?))
    }

    pub(crate) fn update_padding_preview(
        &mut self,
        owner_client_id: ClientId,
        token: Uuid,
        pane_ids: Vec<Uuid>,
        spec: PanePaddingSpec,
    ) -> Result<Vec<crate::padding_api::RuntimePanePaddingState>, SessionRuntimeError> {
        validate_spec(spec).map_err(|_| SessionRuntimeError::Closed)?;
        let next_targets = pane_ids.into_iter().collect::<BTreeSet<_>>();
        let preview = self
            .padding_previews
            .get(&token)
            .ok_or(SessionRuntimeError::NotFound)?;
        if preview.owner_client_id != owner_client_id {
            return Err(SessionRuntimeError::NotFound);
        }
        let previous_targets = preview.target_pane_ids.clone();
        let previous_spec = preview.spec;
        if previous_targets == next_targets && previous_spec == spec {
            let preview = self
                .padding_previews
                .get_mut(&token)
                .ok_or(SessionRuntimeError::NotFound)?;
            preview.lease_expires_at = Instant::now() + PANE_PADDING_PREVIEW_LEASE;
            return self.preview_states(token);
        }
        self.apply_preview_spec_to_targets(&previous_targets, None)?;
        if next_targets.is_empty()
            || next_targets
                .iter()
                .any(|pane_id| self.pane_session_id(*pane_id).is_none())
            || self.padding_previews.iter().any(|(other_token, other)| {
                *other_token != token
                    && other.owner_client_id != owner_client_id
                    && !other.target_pane_ids.is_disjoint(&next_targets)
            })
        {
            self.apply_preview_spec_to_targets(&previous_targets, Some(previous_spec))?;
            return Err(SessionRuntimeError::NotFound);
        }
        let preview = self
            .padding_previews
            .get_mut(&token)
            .ok_or(SessionRuntimeError::NotFound)?;
        preview.target_pane_ids.clone_from(&next_targets);
        preview.spec = spec;
        preview.lease_expires_at = Instant::now() + PANE_PADDING_PREVIEW_LEASE;
        self.apply_preview_spec_to_targets(&next_targets, Some(spec))?;
        self.preview_states(token)
    }

    pub(crate) fn cancel_padding_preview(
        &mut self,
        owner_client_id: ClientId,
        token: Uuid,
    ) -> Result<(), SessionRuntimeError> {
        let preview = self
            .padding_previews
            .remove(&token)
            .ok_or(SessionRuntimeError::NotFound)?;
        if preview.owner_client_id != owner_client_id {
            self.padding_previews.insert(token, preview);
            return Err(SessionRuntimeError::NotFound);
        }
        self.apply_preview_spec_to_targets(&preview.target_pane_ids, None)
    }

    pub(crate) fn commit_padding_preview(
        &mut self,
        owner_client_id: ClientId,
        token: Uuid,
        persistence: PanePaddingOverridePersistence,
    ) -> Result<Vec<crate::padding_api::RuntimePanePaddingState>, SessionRuntimeError> {
        let preview = self
            .padding_previews
            .get(&token)
            .ok_or(SessionRuntimeError::NotFound)?;
        if preview.owner_client_id != owner_client_id {
            return Err(SessionRuntimeError::NotFound);
        }
        let targets = preview.target_pane_ids.iter().copied().collect::<Vec<_>>();
        let target_sessions = targets
            .iter()
            .map(|pane_id| {
                let session_id = self
                    .pane_session_id(*pane_id)
                    .ok_or(SessionRuntimeError::NotFound)?;
                self.runtimes
                    .get(&session_id)
                    .and_then(|runtime| runtime.panes.get(pane_id))
                    .ok_or(SessionRuntimeError::NotFound)?;
                Ok((*pane_id, session_id))
            })
            .collect::<Result<Vec<_>, SessionRuntimeError>>()?;
        let preview = self
            .padding_previews
            .remove(&token)
            .ok_or(SessionRuntimeError::NotFound)?;
        for (pane_id, session_id) in &target_sessions {
            let pane = self
                .runtimes
                .get_mut(session_id)
                .and_then(|runtime| runtime.panes.get_mut(pane_id))
                .expect("preview commit targets were validated while manager is exclusively held");
            pane.padding_preview = None;
            pane.padding_override = Some(preview.spec);
            pane.padding_override_persistence = persistence;
        }
        self.host.mark_snapshot_dirty();
        target_sessions
            .into_iter()
            .map(|(pane_id, session_id)| self.padding_state(session_id, Some(pane_id)))
            .collect()
    }

    pub(crate) fn padding_state(
        &self,
        session_id: SessionId,
        pane_id: Option<Uuid>,
    ) -> Result<crate::padding_api::RuntimePanePaddingState, SessionRuntimeError> {
        let runtime = self
            .runtimes
            .get(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        let pane_id = pane_id.unwrap_or(runtime.focused_pane_id);
        let pane = runtime
            .panes
            .get(&pane_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        let outer = self
            .pane_outer_rect(session_id, pane_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        let base = base_content_rect(outer);
        let active_command = pane
            .resurrection_state
            .lock()
            .ok()
            .and_then(|state| state.active_command.clone());
        let metadata = PanePaddingMetadata {
            name: pane.meta.name.as_deref(),
            shell: &pane.meta.shell,
            active_command: active_command.as_deref(),
        };
        let declarative = self.padding_config.resolve(metadata, base, None);
        let runtime_effective = self
            .padding_config
            .resolve(metadata, base, pane.padding_override);
        let preview_spec = pane.padding_preview;
        let effective_spec = preview_spec.unwrap_or(runtime_effective.spec);
        Ok(crate::padding_api::RuntimePanePaddingState {
            session_id: session_id.0,
            pane_id,
            declarative: declarative.spec,
            matched_rule_index: declarative.matched_rule_index,
            runtime_override: pane.padding_override,
            effective: effective_spec,
            outer_rect: outer,
            base_content_rect: base,
            effective_content_rect: padded_content_rect(base, effective_spec),
            persist_runtime_overrides: self.padding_config.persist_runtime_overrides,
        })
    }

    pub(crate) fn reconcile_padding_after_metadata_change(
        &mut self,
        session_id: SessionId,
        pane_id: Uuid,
        before: &crate::padding_api::RuntimePanePaddingState,
    ) -> Result<crate::padding_api::RuntimePanePaddingState, SessionRuntimeError> {
        let after = self.padding_state(session_id, Some(pane_id))?;
        let mut before = *before;
        // The metadata snapshot may predate a viewport/layout mutation. Compare
        // policy on the current base rectangle, not geometry from another view.
        before.effective_content_rect =
            padded_content_rect(after.base_content_rect, before.effective);
        self.apply_padding_geometry_change(session_id, pane_id, &before, &after)?;
        Ok(after)
    }

    fn apply_padding_geometry_change(
        &mut self,
        session_id: SessionId,
        pane_id: Uuid,
        before: &crate::padding_api::RuntimePanePaddingState,
        after: &crate::padding_api::RuntimePanePaddingState,
    ) -> Result<(), SessionRuntimeError> {
        if before.effective_content_rect == after.effective_content_rect {
            return Ok(());
        }
        if before.effective_content_rect.w != after.effective_content_rect.w
            || before.effective_content_rect.h != after.effective_content_rect.h
        {
            let pane = self
                .runtimes
                .get(&session_id)
                .and_then(|runtime| runtime.panes.get(&pane_id))
                .ok_or(SessionRuntimeError::NotFound)?;
            pane.resize_pty(
                after.effective_content_rect.h.max(1),
                after.effective_content_rect.w.max(1),
            );
        }
        if let Some(revision) = self.bump_attach_view_revision(session_id) {
            self.pending_layout_revisions.insert(session_id, revision);
        }
        Ok(())
    }

    pub(crate) fn clear_padding_overrides(
        &mut self,
        pane_ids: &[Uuid],
    ) -> Result<Vec<crate::padding_api::RuntimePanePaddingState>, SessionRuntimeError> {
        let targets = pane_ids
            .iter()
            .map(|pane_id| {
                let session_id = self
                    .pane_session_id(*pane_id)
                    .ok_or(SessionRuntimeError::NotFound)?;
                let before = self.padding_state(session_id, Some(*pane_id))?;
                Ok((session_id, *pane_id, before))
            })
            .collect::<Result<Vec<_>, SessionRuntimeError>>()?;
        for (session_id, pane_id, _) in &targets {
            let pane = self
                .runtimes
                .get_mut(session_id)
                .and_then(|runtime| runtime.panes.get_mut(pane_id))
                .ok_or(SessionRuntimeError::NotFound)?;
            pane.padding_override = None;
            pane.padding_override_persistence = PanePaddingOverridePersistence::Snapshot;
        }
        let mut states = Vec::with_capacity(targets.len());
        for (session_id, pane_id, before) in targets {
            let after = self.padding_state(session_id, Some(pane_id))?;
            self.apply_padding_geometry_change(session_id, pane_id, &before, &after)?;
            states.push(after);
        }
        self.host.mark_snapshot_dirty();
        Ok(states)
    }

    pub(crate) fn set_padding_override(
        &mut self,
        session_id: SessionId,
        pane_id: Option<Uuid>,
        spec: Option<PanePaddingSpec>,
    ) -> Result<crate::padding_api::RuntimePanePaddingState, SessionRuntimeError> {
        if let Some(spec) = spec {
            validate_spec(spec).map_err(|_| SessionRuntimeError::Closed)?;
        }
        let target = {
            let runtime = self
                .runtimes
                .get(&session_id)
                .ok_or(SessionRuntimeError::NotFound)?;
            pane_id.unwrap_or(runtime.focused_pane_id)
        };
        let before = self.padding_state(session_id, Some(target))?;
        self.runtimes
            .get_mut(&session_id)
            .and_then(|runtime| runtime.panes.get_mut(&target))
            .ok_or(SessionRuntimeError::NotFound)?
            .padding_override = spec;
        if let Some(pane) = self
            .runtimes
            .get_mut(&session_id)
            .and_then(|runtime| runtime.panes.get_mut(&target))
        {
            pane.padding_override_persistence = PanePaddingOverridePersistence::Snapshot;
        }
        let after = self.padding_state(session_id, Some(target))?;
        self.apply_padding_geometry_change(session_id, target, &before, &after)?;
        self.host.mark_snapshot_dirty();
        Ok(after)
    }

    pub(crate) fn install_restored_padding_override(
        &mut self,
        session_id: SessionId,
        pane_id: Uuid,
        spec: PanePaddingSpec,
    ) {
        self.restored_padding_overrides
            .insert((session_id, pane_id), spec);
    }

    pub(crate) fn padding_overrides_for_snapshot(
        &self,
    ) -> BTreeMap<(SessionId, Uuid), PanePaddingSpec> {
        if !self.padding_config.persist_runtime_overrides {
            return BTreeMap::new();
        }
        self.runtimes
            .iter()
            .flat_map(|(session_id, runtime)| {
                runtime.panes.iter().filter_map(|(pane_id, pane)| {
                    (pane.padding_override_persistence == PanePaddingOverridePersistence::Snapshot)
                        .then_some(pane.padding_override)
                        .flatten()
                        .map(|spec| ((*session_id, *pane_id), spec))
                })
            })
            .collect()
    }

    fn active_session_ids(&self) -> Vec<SessionId> {
        self.runtimes.keys().copied().collect()
    }

    fn reconcile_exited_pane_focus(&mut self, session_id: SessionId, pane_id: Uuid) -> bool {
        let Some(runtime) = self.runtimes.get_mut(&session_id) else {
            return false;
        };

        let exited_surface_scopes = runtime
            .floating_surfaces
            .iter()
            .filter(|surface| surface.pane_id == pane_id)
            .map(|surface| surface.scope)
            .collect::<Vec<_>>();
        let exited_was_floating = !exited_surface_scopes.is_empty();

        let floating_focus_is_exited = runtime
            .floating_surfaces
            .iter()
            .any(|surface| surface.pane_id == pane_id && surface.cursor_owner);

        for surface in &mut runtime.floating_surfaces {
            if surface.pane_id == pane_id {
                surface.cursor_owner = false;
            }
        }

        let focus_is_exited = runtime
            .panes
            .get(&runtime.focused_pane_id)
            .is_none_or(|pane| pane.exited.load(Ordering::SeqCst));
        if (runtime.focused_pane_id == pane_id || focus_is_exited || floating_focus_is_exited)
            && let Some(next_focus) = next_live_focus_after_exit(runtime, pane_id)
        {
            runtime.focused_pane_id = next_focus;
            for surface in &mut runtime.floating_surfaces {
                surface.cursor_owner = surface.pane_id == next_focus;
            }
        }

        exited_was_floating
            && exited_surface_scopes.iter().any(|scope| {
                matches!(
                    scope,
                    FloatingPaneScope::ClientGlobal | FloatingPaneScope::ServerGlobal
                )
            })
    }
}

/// Lightweight ECMA-48 escape sequence phase tracker.
///
/// Classifies each byte of a terminal output stream as either part of normal
/// ground-state text or inside an escape sequence (CSI, OSC, DCS, etc.).
/// Used by [`OutputFanoutBuffer`] to record safe resume boundaries so that
/// [`OutputFanoutBuffer::read_recent`] never returns bytes starting mid-sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscSeqPhase {
    /// Normal text and C0 controls — safe for a fresh parser to start here.
    Ground,
    /// Saw ESC (0x1B); next byte determines the sequence type.
    Escape,
    /// Inside a CSI sequence (ESC `[` …); ends on a final byte 0x40–0x7E.
    Csi,
    /// Inside an OSC string (ESC `]` …); ends on BEL (0x07) or ST (ESC `\`).
    Osc,
    /// Saw ESC inside an OSC body — looking for `\` to complete ST.
    OscEsc,
    /// Inside a DCS passthrough (ESC `P` …); ends on ST (ESC `\`).
    Dcs,
    /// Saw ESC inside a DCS body — looking for `\` to complete ST.
    DcsEsc,
    /// Inside an SOS, PM, or APC string (ESC `X`/`^`/`_` …); ends on ST.
    Sos,
    /// Saw ESC inside an SOS/PM/APC body — looking for `\` to complete ST.
    SosEsc,
}

impl EscSeqPhase {
    /// Advance the state machine by one byte, returning the new phase.
    #[inline]
    const fn advance(self, byte: u8) -> Self {
        // CAN (0x18) and SUB (0x1A) abort any sequence from any state.
        if byte == 0x18 || byte == 0x1A {
            return Self::Ground;
        }
        match self {
            Self::Ground => {
                if byte == 0x1B {
                    Self::Escape
                } else {
                    Self::Ground
                }
            }
            Self::Escape => match byte {
                b'[' => Self::Csi,
                b']' => Self::Osc,
                b'P' => Self::Dcs,
                b'X' | b'^' | b'_' => Self::Sos,
                // Intermediate bytes (0x20–0x2F) stay in Escape (ESC intermediate
                // sequence).  ESC restarts.
                0x1B | 0x20..=0x2F => Self::Escape,
                // Final bytes (0x30–0x7E) complete a two-byte escape.
                // Everything else also returns to Ground.
                _ => Self::Ground,
            },
            Self::Csi => match byte {
                // Final byte completes the CSI sequence.
                0x40..=0x7E => Self::Ground,
                // ESC inside CSI aborts it and starts a new escape.
                0x1B => Self::Escape,
                // Parameter bytes (0x30–0x3F) and intermediate bytes (0x20–0x2F)
                // continue the sequence.  Anything else also stays in CSI (tolerant
                // parsing, matching xterm behavior for invalid bytes).
                _ => Self::Csi,
            },
            Self::Osc => match byte {
                0x07 => Self::Ground, // BEL terminates OSC
                0x1B => Self::OscEsc,
                _ => Self::Osc,
            },
            Self::OscEsc => match byte {
                b'\\' => Self::Ground, // ST (ESC \) terminates OSC
                0x1B => Self::Escape,  // nested ESC aborts OSC
                _ => Self::Osc,        // false alarm, back to body
            },
            Self::Dcs => match byte {
                0x1B => Self::DcsEsc,
                _ => Self::Dcs,
            },
            Self::DcsEsc => match byte {
                b'\\' => Self::Ground,
                0x1B => Self::Escape,
                _ => Self::Dcs,
            },
            Self::Sos => match byte {
                0x1B => Self::SosEsc,
                _ => Self::Sos,
            },
            Self::SosEsc => match byte {
                b'\\' => Self::Ground,
                0x1B => Self::Escape,
                _ => Self::Sos,
            },
        }
    }

    const fn is_ground(self) -> bool {
        matches!(self, Self::Ground)
    }

    /// The index of the first byte in `bytes` that can change this phase (all of them in the
    /// short states after an ESC), or `None` when none can.
    fn next_significant(self, bytes: &[u8]) -> Option<usize> {
        // CAN and SUB abort a sequence in any state; text has no sequence to abort.
        let found = match self {
            Self::Ground => memchr::memchr(0x1b, bytes),
            Self::Csi => bytes
                .iter()
                .position(|&byte| matches!(byte, 0x40..=0x7e | 0x1b | 0x18 | 0x1a)),
            Self::Osc => {
                let end = memchr::memchr3(0x07, 0x1b, 0x18, bytes);
                let sub = memchr::memchr(0x1a, &bytes[..end.unwrap_or(bytes.len())]);
                sub.or(end)
            }
            Self::Dcs | Self::Sos => memchr::memchr3(0x1b, 0x18, 0x1a, bytes),
            Self::Escape | Self::OscEsc | Self::DcsEsc | Self::SosEsc => Some(0),
        };
        found.filter(|_| !bytes.is_empty())
    }
}

/// Where synchronized updates (DEC mode 2026) open and close in a pane's output stream, so a
/// read can say whether the application is mid-frame exactly at the read's end, rather than
/// whatever the reader thread saw last (which may already be the application's next frame).
///
/// Recognizes the sequences applications emit, `ESC [ ? 2026 h` and `ESC [ ? 2026 l`, across
/// chunk boundaries.
#[derive(Debug, Default)]
struct SyncUpdateMarks {
    /// `(offset just after the sequence, open)`, ascending.
    marks: VecDeque<(u64, bool)>,
    /// The state before the first retained mark.
    base: bool,
    /// Bytes of `ESC [ ? 2026` matched so far.
    matched: usize,
}

impl SyncUpdateMarks {
    const PREFIX: &'static [u8] = b"\x1b[?2026";

    /// Marks from a point where an update is `open` or not.
    fn starting(open: bool) -> Self {
        Self {
            base: open,
            ..Self::default()
        }
    }

    /// Whether no part of the sequence is matched: a byte other than ESC then matches nothing.
    const fn idle(&self) -> bool {
        self.matched == 0
    }

    /// Feed one stream byte; `after` is the stream offset just after it.
    fn scan(&mut self, byte: u8, after: u64) {
        if self.matched == Self::PREFIX.len() {
            self.matched = 0;
            match byte {
                b'h' => self.marks.push_back((after, true)),
                b'l' => self.marks.push_back((after, false)),
                _ => {}
            }
            return;
        }
        if byte == Self::PREFIX[self.matched] {
            self.matched += 1;
        } else {
            self.matched = usize::from(byte == Self::PREFIX[0]);
        }
    }

    /// Whether an update is open at stream `offset`.
    fn open_at(&self, offset: u64) -> bool {
        self.marks
            .iter()
            .rev()
            .find(|(at, _)| *at <= offset)
            .map_or(self.base, |(_, open)| *open)
    }

    /// Fold marks at or before `offset` (no longer retained) into the base state.
    fn forget_before(&mut self, offset: u64) {
        while let Some(&(at, open)) = self.marks.front() {
            if at > offset {
                break;
            }
            self.base = open;
            self.marks.pop_front();
        }
    }
}

/// How long after the last read asking for bytes an output buffer keeps retaining output. Byte
/// readers (the playbook, snapshot chunks) read on and on while they follow a pane; attach clients
/// take frames and read none.
const RAW_READ_LEASE: Duration = Duration::from_secs(30);

/// A pane grid's parser after a chunk: between sequences (a reader may start after it), and
/// whether a synchronized update is open.
#[derive(Debug, Clone, Copy)]
struct ChunkBoundary {
    between_sequences: bool,
    sync_update_open: bool,
}

impl ChunkBoundary {
    fn of(grid: &TerminalGridStream) -> Self {
        Self {
            between_sequences: grid.between_sequences(),
            sync_update_open: grid.grid().synchronized_update(),
        }
    }
}

struct OutputFanoutBuffer {
    max_bytes: usize,
    start_offset: u64,
    data: VecDeque<u8>,
    cursors: BTreeMap<ClientId, u64>,
    retained_output_bytes: Arc<AtomicUsize>,
    /// Running escape-sequence phase at the end of the buffer.
    esc_phase: EscSeqPhase,
    /// Where synchronized updates (DEC mode 2026) open and close in the retained stream.
    sync_updates: SyncUpdateMarks,
    /// Escape-sequence spans: `(esc_start, safe_resume)` pairs where
    /// `esc_start` is the offset of the ESC byte that began a sequence and
    /// `safe_resume` is the first offset after the sequence completed
    /// (Ground state).  An open (incomplete) span has `safe_resume == u64::MAX`.
    /// Sorted ascending by `esc_start`.
    esc_spans: VecDeque<(u64, u64)>,
    /// When a reader last asked for bytes (creation counts as one). While that is within
    /// [`RAW_READ_LEASE`] output is retained; otherwise pushing it only advances the offsets, and
    /// a reader that returns is told of the gap and resynchronizes from a snapshot.
    last_read: std::cell::Cell<Instant>,
    /// Retaining output, and following its sequences.
    retaining: bool,
    /// The grid's parser after the last chunk pushed.
    boundary: ChunkBoundary,
}

struct OutputRead {
    bytes: Vec<u8>,
    stream_start: u64,
    stream_end: u64,
    stream_gap: bool,
}

impl Drop for OutputFanoutBuffer {
    fn drop(&mut self) {
        self.retained_output_bytes
            .fetch_sub(self.data.len(), Ordering::SeqCst);
    }
}

impl OutputFanoutBuffer {
    #[cfg(test)]
    fn new(max_bytes: usize) -> Self {
        Self::with_accounting(max_bytes, Arc::new(AtomicUsize::new(0)))
    }

    fn with_accounting(max_bytes: usize, retained_output_bytes: Arc<AtomicUsize>) -> Self {
        Self {
            max_bytes: max_bytes.max(1),
            start_offset: 0,
            data: VecDeque::new(),
            cursors: BTreeMap::new(),
            retained_output_bytes,
            esc_phase: EscSeqPhase::Ground,
            esc_spans: VecDeque::new(),
            sync_updates: SyncUpdateMarks::default(),
            last_read: std::cell::Cell::new(Instant::now()),
            retaining: true,
            boundary: ChunkBoundary {
                between_sequences: true,
                sync_update_open: false,
            },
        }
    }

    /// A reader asked for up to `max_bytes`: when that is any, output is retained for it.
    fn note_read(&self, max_bytes: usize) {
        if max_bytes > 0 {
            self.last_read.set(Instant::now());
        }
    }

    /// Push a chunk the pane's grid has parsed, `after` being its parser then. It is retained
    /// while a reader asks for bytes; retention resumes only where the grid was between
    /// sequences, so the escape phase and synchronized-update state start exact (the grid's).
    fn push_parsed(&mut self, chunk: &[u8], after: ChunkBoundary) {
        let demanded = self.last_read.get().elapsed() < RAW_READ_LEASE;
        if demanded && !self.retaining && self.boundary.between_sequences {
            self.retaining = true;
            self.esc_phase = EscSeqPhase::Ground;
            self.esc_spans.clear();
            self.sync_updates = SyncUpdateMarks::starting(self.boundary.sync_update_open);
        }
        if demanded && self.retaining {
            self.push_chunk(chunk);
        } else {
            self.skip(chunk.len());
        }
        self.boundary = after;
    }

    /// Advance past `len` bytes without retaining them (or what is retained): no reader wants
    /// them.
    fn skip(&mut self, len: usize) {
        self.retaining = false;
        let dropped = self.data.len();
        self.data.clear();
        self.retained_output_bytes
            .fetch_sub(dropped, Ordering::SeqCst);
        self.start_offset = self.start_offset.saturating_add((dropped + len) as u64);
        self.esc_spans.clear();
        self.sync_updates = SyncUpdateMarks::starting(false);
    }

    /// Whether a synchronized update is open at stream `offset` (after the bytes before it).
    fn sync_update_open_at(&self, offset: u64) -> bool {
        self.sync_updates.open_at(offset)
    }

    fn end_offset(&self) -> u64 {
        self.start_offset + self.data.len() as u64
    }

    fn register_client_at_tail(&mut self, client_id: ClientId) {
        self.cursors.insert(client_id, self.end_offset());
    }

    fn unregister_client(&mut self, client_id: ClientId) {
        self.cursors.remove(&client_id);
    }

    #[cfg(test)]
    fn set_client_cursor(&mut self, client_id: ClientId, offset: u64) {
        let clamped = offset.clamp(self.start_offset, self.end_offset());
        self.cursors.insert(client_id, clamped);
    }

    fn acknowledge_snapshot(&mut self, client_id: ClientId, offset: u64) {
        let Some(current) = self.cursors.get(&client_id).copied() else {
            // Encoding runs outside the output lock. Detach may have retired
            // this cursor since capture; an acknowledgement cannot register it.
            return;
        };
        // Do not clamp to the retained start: eviction during encoding must
        // remain visible as a stream gap on the next raw-output read.
        self.cursors
            .insert(client_id, current.max(offset).min(self.end_offset()));
    }

    fn push_chunk(&mut self, chunk: &[u8]) {
        self.push_chunk_with_node_cap(chunk, MAX_NODE_OUTPUT_BUFFER_BYTES);
    }

    fn push_chunk_with_node_cap(&mut self, chunk: &[u8], node_max_bytes: usize) {
        let base_offset = self.end_offset();
        self.retained_output_bytes
            .fetch_add(chunk.len(), Ordering::SeqCst);
        self.data.extend(chunk);

        // Track escape-sequence phase for every byte.  Record spans
        // so that `first_safe_offset_at_or_after` can determine whether
        // any position is inside an escape sequence.
        //
        // The ESC byte itself is a safe start — a fresh ground-state parser
        // correctly handles it.  The unsafe region begins at the byte AFTER
        // the ESC (e.g. the `[` in CSI, the `]` in OSC) and extends to the
        // byte after the final/terminator byte.
        let mut i = 0;
        while i < chunk.len() {
            // Text between escapes changes neither the phase nor the marks: jump to the next
            // ESC (CAN and SUB abort only sequences, and ground is no sequence).
            // Bytes that cannot change the phase (text between escapes, a sequence's body) are
            // skipped to the next one that can, unless a synchronized-update mark is mid-match.
            if self.sync_updates.idle() {
                match self.esc_phase.next_significant(&chunk[i..]) {
                    Some(skip) => i += skip,
                    None => break,
                }
            }
            let byte = chunk[i];
            self.sync_updates.scan(byte, base_offset + i as u64 + 1);
            let prev = self.esc_phase;
            self.esc_phase = prev.advance(byte);

            if prev.is_ground() && !self.esc_phase.is_ground() {
                // Ground → non-Ground: open a new span starting AFTER the ESC byte.
                self.esc_spans
                    .push_back((base_offset + i as u64 + 1, u64::MAX));
            } else if !prev.is_ground() && self.esc_phase.is_ground() {
                // non-Ground → Ground: close the current span.  The safe
                // resume point is the byte after the final/terminator byte.
                if let Some(last) = self.esc_spans.back_mut()
                    && last.1 == u64::MAX
                {
                    last.1 = base_offset + i as u64 + 1;
                }
            }
            i += 1;
        }

        let mut contention_retries = 0;
        loop {
            // Evict a batch, not one atomic accounting operation per byte. Other
            // buffers may grow concurrently, so recheck the shared budget after
            // each batch just as the byte-wise implementation did.
            let local_excess = self.data.len().saturating_sub(self.max_bytes);
            let retained = self.retained_output_bytes.load(Ordering::SeqCst);
            let node_excess = retained.saturating_sub(node_max_bytes);
            let remove = local_excess.max(node_excess).min(self.data.len());
            if remove == 0 {
                break;
            }
            // Reserve the accounting decrement before draining. Competing buffers
            // must not all evict the same stale node excess as one large batch.
            if self
                .retained_output_bytes
                .compare_exchange(
                    retained,
                    retained - remove,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_err()
            {
                contention_retries += 1;
                if contention_retries < 8 {
                    continue;
                }
                // Bound retry-only work under adversarial contention. The old
                // byte-wise eviction guarantees progress and explicit gap repair.
                self.data.pop_front();
                self.retained_output_bytes.fetch_sub(1, Ordering::SeqCst);
                self.start_offset = self.start_offset.saturating_add(1);
                contention_retries = 0;
                continue;
            }
            contention_retries = 0;
            self.data.drain(..remove);
            self.start_offset = self.start_offset.saturating_add(remove as u64);
        }

        // Prune spans that are entirely before start_offset.
        while let Some(&(_, safe_resume)) = self.esc_spans.front() {
            if safe_resume != u64::MAX && safe_resume <= self.start_offset {
                self.esc_spans.pop_front();
            } else {
                break;
            }
        }

        self.sync_updates.forget_before(self.start_offset);

        // Do not mutate per-client cursors here.  `read_for_client` performs
        // clamping and reports `stream_gap` so clients can recover parser
        // continuity with explicit metadata.
    }

    fn read_for_client(&mut self, client_id: ClientId, max_bytes: usize) -> OutputRead {
        self.note_read(max_bytes);
        let limit = max_bytes.max(1);
        let end = self.end_offset();

        // Pre-compute the safe resume position before borrowing cursors
        // mutably, since first_ground_boundary_at_or_after borrows self
        // immutably.
        let safe_resume = self.first_safe_offset_at_or_after(self.start_offset);

        let cursor = self.cursors.entry(client_id).or_insert(end);

        let stream_gap = if *cursor < self.start_offset {
            // Bytes were evicted before the client could read them.  Advance
            // the cursor to the nearest safe position so the client
            // never receives bytes starting mid-escape-sequence.
            *cursor = safe_resume;
            true
        } else {
            false
        };

        let stream_start = *cursor;

        #[allow(clippy::cast_possible_truncation)]
        let available = end.saturating_sub(*cursor) as usize;
        if available == 0 {
            return OutputRead {
                bytes: Vec::new(),
                stream_start,
                stream_end: stream_start,
                stream_gap,
            };
        }

        let to_read = available.min(limit);
        #[allow(clippy::cast_possible_truncation)]
        let start_index = (*cursor - self.start_offset) as usize;
        let bytes = self
            .data
            .iter()
            .skip(start_index)
            .take(to_read)
            .copied()
            .collect::<Vec<_>>();
        *cursor = cursor.saturating_add(bytes.len() as u64);

        OutputRead {
            bytes,
            stream_start,
            stream_end: *cursor,
            stream_gap,
        }
    }

    fn read_at(&self, cursor: u64, limit: usize) -> OutputRead {
        self.note_read(limit);
        let end = self.end_offset();
        let stream_gap = cursor < self.start_offset || cursor > end;
        let stream_start = cursor.clamp(self.start_offset, end);
        if limit == 0 || stream_start == end {
            return OutputRead {
                bytes: Vec::new(),
                stream_start,
                stream_end: stream_start,
                stream_gap,
            };
        }
        let available = usize::try_from(end.saturating_sub(stream_start)).unwrap_or(usize::MAX);
        let to_read = available.min(limit);
        let start_index =
            usize::try_from(stream_start.saturating_sub(self.start_offset)).unwrap_or(usize::MAX);
        let bytes = self
            .data
            .iter()
            .skip(start_index)
            .take(to_read)
            .copied()
            .collect::<Vec<_>>();
        OutputRead {
            stream_end: stream_start.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX)),
            bytes,
            stream_start,
            stream_gap,
        }
    }

    fn read_recent_with_offsets(&self, max_bytes: usize) -> OutputRead {
        self.note_read(max_bytes);
        let end = self.end_offset();
        if self.data.is_empty() || max_bytes == 0 {
            return OutputRead {
                bytes: Vec::new(),
                stream_start: end,
                stream_end: end,
                stream_gap: false,
            };
        }
        let to_read = self.data.len().min(max_bytes);
        let intended_start = end - to_read as u64;
        let safe_start = self.first_safe_offset_at_or_after(intended_start);

        if safe_start >= end {
            return OutputRead {
                bytes: Vec::new(),
                stream_start: end,
                stream_end: end,
                stream_gap: false,
            };
        }

        #[allow(clippy::cast_possible_truncation)]
        let start_index = (safe_start - self.start_offset) as usize;
        OutputRead {
            bytes: self.data.iter().skip(start_index).copied().collect(),
            stream_start: safe_start,
            stream_end: end,
            stream_gap: false,
        }
    }

    /// Return the first stream offset >= `target` where a fresh ground-state
    /// parser can safely start consuming bytes.
    ///
    /// Checks the escape-sequence span list to determine whether `target`
    /// falls inside an open span.  If so, advances to the span's
    /// `safe_resume` offset.
    fn first_safe_offset_at_or_after(&self, target: u64) -> u64 {
        // Find the span that could contain `target`.  We need the latest
        // span whose esc_start <= target.
        //
        // Binary search by esc_start (the first element of each tuple).
        let idx = self
            .esc_spans
            .binary_search_by(|&(esc_start, _)| esc_start.cmp(&target))
            .unwrap_or_else(|insert_point| insert_point.saturating_sub(1));

        // Check a small window of spans around the search result.  Due to
        // binary_search edge cases with saturating_sub, check idx and idx+1.
        for check in idx..self.esc_spans.len().min(idx + 2) {
            let (esc_start, safe_resume) = self.esc_spans[check];
            if esc_start <= target && target < safe_resume {
                // `target` falls inside this escape sequence.
                if safe_resume == u64::MAX {
                    // Sequence is still open (not yet terminated).
                    return self.end_offset();
                }
                return safe_resume;
            }
        }

        // `target` is not inside any escape sequence — it's in Ground state.
        target
    }

    /// Advance an existing client's read cursor to the end of the buffer,
    /// so the next `read_for_client` call only returns data written after
    /// this point. Used after snapshot reads to avoid re-delivering bytes
    /// the client already received via `read_recent`.
    fn advance_client_to_end(&mut self, client_id: ClientId) {
        let end = self.end_offset();
        if let Some(cursor) = self.cursors.get_mut(&client_id) {
            *cursor = end;
        }
    }
}

struct RemovedRuntime {
    session_id: SessionId,
    handle: SessionRuntimeHandle,
}

// Retained as manager-local DTOs for legacy helper methods that are still useful
// in tests and during the ongoing runtime-store split.
#[allow(dead_code)]
struct AttachLayoutState {
    focused_pane_id: Uuid,
    panes: Vec<PaneSummary>,
    layout_root: IpcPaneLayoutNode,
    scene: AttachScene,
    zoomed: bool,
}

// Retained as manager-local DTOs for legacy helper methods that are still useful
// in tests and during the ongoing runtime-store split.
#[allow(dead_code)]
struct AttachSnapshotState {
    focused_pane_id: Uuid,
    panes: Vec<PaneSummary>,
    layout_root: IpcPaneLayoutNode,
    scene: AttachScene,
    chunks: Vec<AttachPaneChunk>,
    pane_mouse_protocols: Vec<AttachPaneMouseProtocol>,
    pane_input_modes: Vec<AttachPaneInputMode>,
    zoomed: bool,
}

// Retained as manager-local DTOs for legacy helper methods that are still useful
// in tests and during the ongoing runtime-store split.
#[allow(dead_code)]
struct AttachPaneSnapshotState {
    chunks: Vec<AttachPaneChunk>,
    pane_mouse_protocols: Vec<AttachPaneMouseProtocol>,
    pane_input_modes: Vec<AttachPaneInputMode>,
}

fn ipc_layout_from_runtime(node: &PaneLayoutNode) -> IpcPaneLayoutNode {
    match node {
        PaneLayoutNode::Leaf { pane_id } => IpcPaneLayoutNode::Leaf { pane_id: *pane_id },
        PaneLayoutNode::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let percent = (ratio * 100.0).round();
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let ratio_percent = percent.clamp(10.0, 90.0) as u8;
            IpcPaneLayoutNode::Split {
                direction: *direction,
                ratio_percent,
                first: Box::new(ipc_layout_from_runtime(first)),
                second: Box::new(ipc_layout_from_runtime(second)),
            }
        }
    }
}

fn collect_runtime_layout_pane_ids(node: &PaneLayoutNode, out: &mut BTreeSet<Uuid>) -> Result<()> {
    match node {
        PaneLayoutNode::Leaf { pane_id } => {
            if !out.insert(*pane_id) {
                anyhow::bail!("duplicate pane id {pane_id} in runtime layout")
            }
        }
        PaneLayoutNode::Split {
            ratio,
            first,
            second,
            ..
        } => {
            if !(0.1..=0.9).contains(ratio) {
                anyhow::bail!("runtime split ratio {ratio} out of range [0.1, 0.9]")
            }
            collect_runtime_layout_pane_ids(first, out)?;
            collect_runtime_layout_pane_ids(second, out)?;
        }
    }
    Ok(())
}

fn floating_pane_ids(runtime: &SessionRuntimeHandle) -> BTreeSet<Uuid> {
    runtime
        .floating_surfaces
        .iter()
        .map(|surface| surface.pane_id)
        .collect()
}

fn tiled_pane_ids(runtime: &SessionRuntimeHandle) -> BTreeSet<Uuid> {
    let floating = floating_pane_ids(runtime);
    runtime
        .panes
        .keys()
        .copied()
        .filter(|pane_id| !floating.contains(pane_id))
        .collect()
}

fn ordered_tiled_pane_ids(runtime: &SessionRuntimeHandle) -> Vec<Uuid> {
    let mut pane_ids = Vec::new();
    if let Some(layout_root) = &runtime.layout_root {
        layout_root.pane_order(&mut pane_ids);
    }
    pane_ids
}

fn ordered_pane_ids(runtime: &SessionRuntimeHandle) -> Vec<Uuid> {
    let mut pane_ids = ordered_tiled_pane_ids(runtime);
    for surface in &runtime.floating_surfaces {
        if runtime.panes.contains_key(&surface.pane_id) && !pane_ids.contains(&surface.pane_id) {
            pane_ids.push(surface.pane_id);
        }
    }
    pane_ids
}

fn fallback_ipc_layout(runtime: &SessionRuntimeHandle) -> IpcPaneLayoutNode {
    runtime.layout_root.as_ref().map_or_else(
        || IpcPaneLayoutNode::Leaf {
            pane_id: runtime.focused_pane_id,
        },
        ipc_layout_from_runtime,
    )
}

fn floating_summary(surface: FloatingSurfaceRuntime) -> FloatingPaneRuntimeSummary {
    FloatingPaneRuntimeSummary {
        id: surface.id,
        pane_id: surface.pane_id,
        anchor_pane_id: surface.anchor_pane_id,
        context_id: surface.context_id,
        client_id: surface.client_id,
        rect: surface.rect,
        scope: surface.scope,
        layer: surface.layer,
        z: surface.z,
        visible: surface.visible,
        opaque: surface.opaque,
        accepts_input: surface.accepts_input,
        cursor_owner: surface.cursor_owner,
    }
}

fn validate_runtime_layout_matches_panes(runtime: &SessionRuntimeHandle) -> Result<()> {
    let pane_ids = tiled_pane_ids(runtime);
    let mut layout_ids = BTreeSet::new();
    if let Some(layout_root) = &runtime.layout_root {
        collect_runtime_layout_pane_ids(layout_root, &mut layout_ids)?;
    }
    if pane_ids != layout_ids {
        anyhow::bail!(
            "runtime layout panes do not match tiled runtime pane map (layout: {}, panes: {})",
            layout_ids.len(),
            pane_ids.len()
        )
    }
    Ok(())
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn split_layout_rect(rect: LayoutRect, ratio: f32, vertical: bool) -> (LayoutRect, LayoutRect) {
    let ratio = ratio.clamp(0.1, 0.9);
    if vertical {
        let split = ((f32::from(rect.w) * ratio).round()) as u16;
        let first_w = split.max(1).min(rect.w.saturating_sub(1).max(1));
        let second_w = rect.w.saturating_sub(first_w).max(1);
        (
            LayoutRect {
                x: rect.x,
                y: rect.y,
                w: first_w,
                h: rect.h,
            },
            LayoutRect {
                x: rect.x.saturating_add(first_w),
                y: rect.y,
                w: second_w,
                h: rect.h,
            },
        )
    } else {
        let split = ((f32::from(rect.h) * ratio).round()) as u16;
        let first_h = split.max(1).min(rect.h.saturating_sub(1).max(1));
        let second_h = rect.h.saturating_sub(first_h).max(1);
        (
            LayoutRect {
                x: rect.x,
                y: rect.y,
                w: rect.w,
                h: first_h,
            },
            LayoutRect {
                x: rect.x,
                y: rect.y.saturating_add(first_h),
                w: rect.w,
                h: second_h,
            },
        )
    }
}

fn collect_layout_rects(
    node: &PaneLayoutNode,
    rect: LayoutRect,
    out: &mut BTreeMap<Uuid, LayoutRect>,
) {
    match node {
        PaneLayoutNode::Leaf { pane_id } => {
            out.insert(*pane_id, rect);
        }
        PaneLayoutNode::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let vertical = matches!(direction, PaneSplitDirection::Vertical);
            let (first_rect, second_rect) = split_layout_rect(rect, *ratio, vertical);
            collect_layout_rects(first, first_rect, out);
            collect_layout_rects(second, second_rect, out);
        }
    }
}

fn layout_order_fallback_after_close(order: &[Uuid], target: Uuid) -> Option<Uuid> {
    let target_index = order.iter().position(|id| *id == target)?;
    order
        .iter()
        .copied()
        .filter(|id| *id != target)
        .nth(target_index)
        .or_else(|| {
            order
                .iter()
                .copied()
                .take(target_index)
                .rfind(|id| *id != target)
        })
}

fn rect_center(rect: LayoutRect) -> (u32, u32) {
    (
        u32::from(rect.x).saturating_mul(2) + u32::from(rect.w),
        u32::from(rect.y).saturating_mul(2) + u32::from(rect.h),
    )
}

fn rect_center_distance(a: LayoutRect, b: LayoutRect) -> u32 {
    let (ax, ay) = rect_center(a);
    let (bx, by) = rect_center(b);
    ax.abs_diff(bx).saturating_add(ay.abs_diff(by))
}

fn next_focus_after_close(
    layout_root: &PaneLayoutNode,
    focused_pane_id: Uuid,
    target: Uuid,
    viewport: Option<AttachViewport>,
) -> Option<Uuid> {
    let mut order = Vec::new();
    layout_root.pane_order(&mut order);
    if !order.contains(&target) {
        return None;
    }
    if focused_pane_id != target && order.contains(&focused_pane_id) {
        return Some(focused_pane_id);
    }

    let fallback = layout_order_fallback_after_close(&order, target);
    let root = scene_root_from_viewport(viewport);
    let mut rects = BTreeMap::new();
    collect_layout_rects(layout_root, root, &mut rects);
    let target_rect = rects.get(&target).copied()?;
    let target_index = order.iter().position(|id| *id == target).unwrap_or(0);

    rects
        .iter()
        .filter(|(pane_id, _)| **pane_id != target)
        .min_by_key(|(pane_id, rect)| {
            let order_index = order
                .iter()
                .position(|id| id == *pane_id)
                .unwrap_or(usize::MAX);
            (
                rect_center_distance(target_rect, **rect),
                order_index.abs_diff(target_index),
                order_index,
            )
        })
        .map(|(pane_id, _)| *pane_id)
        .or(fallback)
}

const fn attach_rect_from_layout_rect(rect: LayoutRect) -> AttachRect {
    AttachRect {
        x: rect.x,
        y: rect.y,
        w: rect.w,
        h: rect.h,
    }
}

fn pane_content_rect_for_metadata(
    padding_config: &PanePaddingConfig,
    metadata: PanePaddingMetadata<'_>,
    runtime_override: Option<PanePaddingSpec>,
    rect: AttachRect,
) -> AttachRect {
    let base = base_content_rect(rect);
    let resolved = padding_config.resolve(metadata, base, runtime_override);
    padded_content_rect(base, resolved.spec)
}

fn pane_content_rect(
    padding_config: &PanePaddingConfig,
    pane: &PaneRuntimeHandle,
    rect: AttachRect,
) -> AttachRect {
    let active_command = pane
        .resurrection_state
        .lock()
        .ok()
        .and_then(|state| state.active_command.clone());
    pane_content_rect_for_metadata(
        padding_config,
        PanePaddingMetadata {
            name: pane.meta.name.as_deref(),
            shell: &pane.meta.shell,
            active_command: active_command.as_deref(),
        },
        pane.padding_preview.or(pane.padding_override),
        rect,
    )
}

fn scene_root_from_viewport(viewport: Option<AttachViewport>) -> LayoutRect {
    let Some(viewport) = viewport else {
        return LayoutRect {
            x: 0,
            y: 0,
            w: 1,
            h: 1,
        };
    };
    let content = viewport.content_region();
    LayoutRect {
        x: content.x,
        y: content.y,
        w: content.cols,
        h: content.rows,
    }
}

fn floating_surface_visible_for_attach(
    owner_session_id: SessionId,
    attach_session_id: SessionId,
    runtime: &SessionRuntimeHandle,
    surface: &FloatingSurfaceRuntime,
    client_id: ClientId,
    context_id: Option<Uuid>,
) -> bool {
    if !runtime.panes.contains_key(&surface.pane_id) {
        return false;
    }
    match surface.scope {
        FloatingPaneScope::PerPane => {
            owner_session_id == attach_session_id
                && surface
                    .anchor_pane_id
                    .is_some_and(|anchor| runtime.panes.contains_key(&anchor))
        }
        FloatingPaneScope::PerWindow => {
            owner_session_id == attach_session_id
                && (surface.context_id.is_none() || surface.context_id == context_id)
        }
        FloatingPaneScope::PerSession => owner_session_id == attach_session_id,
        FloatingPaneScope::ClientGlobal => surface.client_id.is_none_or(|owner| owner == client_id),
        FloatingPaneScope::ServerGlobal => true,
    }
}

fn attach_surface_from_floating(
    padding_config: &PanePaddingConfig,
    pane: &PaneRuntimeHandle,
    surface: &FloatingSurfaceRuntime,
) -> AttachSurface {
    let rect = attach_rect_from_layout_rect(surface.rect);
    AttachSurface {
        id: surface.id,
        kind: AttachSurfaceKind::FloatingPane,
        layer: surface.layer.to_attach_layer(),
        z: surface.z,
        rect,
        content_rect: pane_content_rect(padding_config, pane, rect),
        interactive_regions: Vec::new(),
        opaque: surface.opaque,
        visible: surface.visible,
        accepts_input: surface.accepts_input,
        cursor_owner: surface.cursor_owner,
        pane_id: Some(surface.pane_id),
    }
}

fn focused_pane_for_scene(scene: &AttachScene, fallback: Uuid) -> Uuid {
    let mut best: Option<(AttachLayer, i32, usize, Uuid)> = None;
    for (index, surface) in scene.surfaces.iter().enumerate() {
        let Some(pane_id) = surface.pane_id else {
            continue;
        };
        if !surface.visible || !surface.accepts_input || !surface.cursor_owner {
            continue;
        }
        let candidate = (surface.layer, surface.z, index, pane_id);
        if best.as_ref().is_none_or(|current| {
            (candidate.0, candidate.1, candidate.2) > (current.0, current.1, current.2)
        }) {
            best = Some(candidate);
        }
    }
    best.map_or(fallback, |(_, _, _, pane_id)| pane_id)
}

fn runtime_pane_is_live(runtime: &SessionRuntimeHandle, pane_id: Uuid) -> bool {
    runtime
        .panes
        .get(&pane_id)
        .is_some_and(|pane| !pane.exited.load(Ordering::SeqCst))
}

fn next_live_focus_after_exit(
    runtime: &SessionRuntimeHandle,
    exited_pane_id: Uuid,
) -> Option<Uuid> {
    let floating_focus = runtime
        .floating_surfaces
        .iter()
        .enumerate()
        .filter(|(_, surface)| {
            surface.pane_id != exited_pane_id
                && surface.visible
                && surface.accepts_input
                && runtime_pane_is_live(runtime, surface.pane_id)
        })
        .max_by_key(|(index, surface)| (surface.layer.to_attach_layer(), surface.z, *index))
        .map(|(_, surface)| surface.pane_id);

    floating_focus
        .or_else(|| {
            ordered_tiled_pane_ids(runtime).into_iter().find(|pane_id| {
                *pane_id != exited_pane_id && runtime_pane_is_live(runtime, *pane_id)
            })
        })
        .or_else(|| {
            runtime.panes.keys().copied().find(|pane_id| {
                *pane_id != exited_pane_id && runtime_pane_is_live(runtime, *pane_id)
            })
        })
}

// Building the attach scene requires constructing every surface's
// `rect` + `content_rect` + `interactive_regions` literally; splitting
// this further would hurt readability more than it helps.
#[allow(clippy::too_many_lines)]
fn build_attach_scene(
    session_id: SessionId,
    runtime: &SessionRuntimeHandle,
    padding_config: &PanePaddingConfig,
    viewport: Option<AttachViewport>,
    client_id: ClientId,
    context_id: Option<Uuid>,
) -> AttachScene {
    let scene_root = scene_root_from_viewport(viewport);

    // When a pane is zoomed, produce a single-pane scene that fills the viewport.
    if let Some(zoomed_id) = runtime.zoomed_pane_id
        && runtime.panes.contains_key(&zoomed_id)
    {
        let zoomed_rect = attach_rect_from_layout_rect(scene_root);
        let zoomed_surface = AttachSurface {
            id: zoomed_id,
            kind: AttachSurfaceKind::Pane,
            layer: AttachLayer::Pane,
            z: 0,
            rect: zoomed_rect,
            content_rect: pane_content_rect(
                padding_config,
                runtime.panes.get(&zoomed_id).expect("zoomed pane exists"),
                zoomed_rect,
            ),
            interactive_regions: Vec::new(),
            opaque: true,
            visible: true,
            accepts_input: true,
            cursor_owner: true,
            pane_id: Some(zoomed_id),
        };

        let mut surfaces = vec![zoomed_surface];

        // Floating surfaces still render on top of the zoomed pane.
        surfaces.extend(
            runtime
                .floating_surfaces
                .iter()
                .filter(|surface| {
                    floating_surface_visible_for_attach(
                        session_id, session_id, runtime, surface, client_id, context_id,
                    )
                })
                .map(|surface| {
                    attach_surface_from_floating(
                        padding_config,
                        runtime
                            .panes
                            .get(&surface.pane_id)
                            .expect("floating pane exists"),
                        surface,
                    )
                }),
        );

        let scene = AttachScene {
            session_id: session_id.0,
            focus: AttachFocusTarget::Pane { pane_id: zoomed_id },
            surfaces,
        };
        let pane_id = focused_pane_for_scene(&scene, zoomed_id);
        return AttachScene {
            focus: AttachFocusTarget::Pane { pane_id },
            ..scene
        };
    }
    // Zoomed pane was removed; fall through to normal rendering.

    let mut rects = BTreeMap::new();
    if let Some(layout_root) = &runtime.layout_root {
        collect_layout_rects(layout_root, scene_root, &mut rects);
    }

    let pane_ids = ordered_tiled_pane_ids(runtime);

    let mut surfaces = pane_ids
        .into_iter()
        .enumerate()
        .filter_map(|(index, pane_id)| {
            rects.get(&pane_id).copied().map(|rect| {
                let attach_rect = attach_rect_from_layout_rect(rect);
                AttachSurface {
                    id: pane_id,
                    kind: AttachSurfaceKind::Pane,
                    layer: AttachLayer::Pane,
                    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
                    z: index as i32,
                    rect: attach_rect,
                    content_rect: pane_content_rect(
                        padding_config,
                        runtime.panes.get(&pane_id).expect("layout pane exists"),
                        attach_rect,
                    ),
                    interactive_regions: Vec::new(),
                    opaque: true,
                    visible: true,
                    accepts_input: true,
                    cursor_owner: pane_id == runtime.focused_pane_id,
                    pane_id: Some(pane_id),
                }
            })
        })
        .collect::<Vec<_>>();

    surfaces.extend(
        runtime
            .floating_surfaces
            .iter()
            .filter(|surface| {
                floating_surface_visible_for_attach(
                    session_id, session_id, runtime, surface, client_id, context_id,
                )
            })
            .map(|surface| {
                attach_surface_from_floating(
                    padding_config,
                    runtime
                        .panes
                        .get(&surface.pane_id)
                        .expect("floating pane exists"),
                    surface,
                )
            }),
    );

    let scene = AttachScene {
        session_id: session_id.0,
        focus: AttachFocusTarget::Pane {
            pane_id: runtime.focused_pane_id,
        },
        surfaces,
    };
    let pane_id = focused_pane_for_scene(&scene, runtime.focused_pane_id);
    AttachScene {
        focus: AttachFocusTarget::Pane { pane_id },
        ..scene
    }
}

fn pane_pty_size_for_metadata(
    padding_config: &PanePaddingConfig,
    metadata: &PaneRuntimeMeta,
    runtime_override: Option<PanePaddingSpec>,
    layout_rect: LayoutRect,
) -> (u16, u16) {
    let content = pane_content_rect_for_metadata(
        padding_config,
        PanePaddingMetadata {
            name: metadata.name.as_deref(),
            shell: &metadata.shell,
            active_command: metadata.resurrection.active_command.as_deref(),
        },
        runtime_override,
        attach_rect_from_layout_rect(layout_rect),
    );
    (content.h.max(1), content.w.max(1))
}

fn pane_pty_size(
    padding_config: &PanePaddingConfig,
    pane: &PaneRuntimeHandle,
    layout_rect: LayoutRect,
) -> (u16, u16) {
    let content = pane_content_rect(
        padding_config,
        pane,
        attach_rect_from_layout_rect(layout_rect),
    );
    let cols = content.w.max(1);
    let rows = content.h.max(1);
    (rows, cols)
}

/// Return shared geometry without taking sizing authority. Only an unset
/// viewport may be seeded from the explicit detached fallback.
pub(crate) fn observe_viewport(
    host: &PaneRuntimeHost,
    session_id: SessionId,
    client_id: ClientId,
    cols: u16,
    rows: u16,
) -> Result<AttachViewport, SessionRuntimeError> {
    let handle = host
        .pane_padding_handle()
        .ok_or(SessionRuntimeError::NotFound)?;
    let (result, seeded) = {
        let mut manager = handle.0.lock().map_err(|_| SessionRuntimeError::Closed)?;
        let unset = manager
            .runtimes
            .get(&session_id)
            .is_some_and(|runtime| runtime.attach_viewport.is_none());
        let result = manager.observe_viewport(session_id, client_id, cols, rows);
        let seeded = unset
            && result.is_ok()
            && !crate::handlers::attach_commands::observer_identity(host, client_id);
        (result, seeded)
    };
    if seeded {
        // Notify after releasing the manager; this path is used only when no
        // authoritative geometry existed yet, never for ordinary observation.
        host.emit_attach_view_changed_for_layout(session_id);
    }
    result
}

/// Open an already-running output stream without selection or PTY sizing.
pub(crate) fn begin_observer_attach(
    host: &PaneRuntimeHost,
    session: SessionId,
    client: ClientId,
) -> Result<(), SessionRuntimeError> {
    let handle = host
        .pane_padding_handle()
        .ok_or(SessionRuntimeError::NotFound)?;
    let mut manager = handle.0.lock().map_err(|_| SessionRuntimeError::Closed)?;
    manager.begin_observer_attach(session, client)
}

impl SessionRuntimeManager {
    fn begin_observer_attach(
        &mut self,
        session: SessionId,
        client: ClientId,
    ) -> Result<(), SessionRuntimeError> {
        let host = self.host.clone();
        let host = &host;
        let _admission = managed_idle::admit(host).map_err(|_| SessionRuntimeError::Closed)?;
        let runtime = self
            .runtimes
            .get_mut(&session)
            .ok_or(SessionRuntimeError::NotFound)?;
        // Register readers before exposing attached membership. Never restart,
        // select a pane, mutate shared geometry or resize any PTY here.
        for pane in runtime.panes.values_mut() {
            if !crate::private_input::register_producer(host, pane.meta.id, client.0) {
                return Err(SessionRuntimeError::Closed);
            }
            pane.output_buffer
                .lock()
                .map_err(|_| SessionRuntimeError::Closed)?
                .register_client_at_tail(client);
        }
        runtime.attached_clients.insert(client);
        if let Ok(mut permissions) = self.client_write_permissions.write() {
            permissions.entry(session).or_default().remove(&client);
        }
        Ok(())
    }

    fn observe_viewport(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
        cols: u16,
        rows: u16,
    ) -> Result<AttachViewport, SessionRuntimeError> {
        let host = self.host.clone();
        let host = &host;
        let runtime = self
            .runtimes
            .get_mut(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        if !runtime.attached_clients.contains(&client_id) {
            return Err(SessionRuntimeError::NotAttached);
        }
        if let Some(viewport) = runtime.attach_viewport {
            return Ok(viewport);
        }
        if cols == 0 || rows == 0 || usize::from(cols) * usize::from(rows) > 262_144 {
            return Err(SessionRuntimeError::Closed);
        }
        let viewport = AttachViewport::normalized(cols, rows, 0, 0, 0, 0);
        if crate::handlers::attach_commands::observer_identity(host, client_id) {
            // Detached fallback is a local projection, not sizing authority.
            return Ok(viewport);
        }
        runtime.attach_viewport = Some(viewport);
        resize_session_ptys(&self.padding_config, runtime);
        Ok(viewport)
    }
}

fn resize_session_ptys(padding_config: &PanePaddingConfig, runtime: &SessionRuntimeHandle) {
    let root = scene_root_from_viewport(runtime.attach_viewport);

    // When zoomed, only resize the zoomed pane to fill the viewport.
    if let Some(zoomed_id) = runtime.zoomed_pane_id {
        if let Some(pane) = runtime.panes.get(&zoomed_id)
            && !pane.exited.load(Ordering::SeqCst)
        {
            let (zoom_rows, zoom_cols) = pane_pty_size(padding_config, pane, root);
            pane.resize_pty(zoom_rows, zoom_cols);
        }
        return;
    }

    let mut rects = BTreeMap::new();
    if let Some(layout_root) = &runtime.layout_root {
        collect_layout_rects(layout_root, root, &mut rects);
    }
    for (pane_id, pane) in &runtime.panes {
        if pane.exited.load(Ordering::SeqCst) {
            continue;
        }
        let rect = rects.get(pane_id).copied().or_else(|| {
            runtime
                .floating_surfaces
                .iter()
                .find(|surface| surface.pane_id == *pane_id)
                .map(|surface| surface.rect)
        });
        if let Some(rect) = rect {
            let (rows, cols) = pane_pty_size(padding_config, pane, rect);
            pane.resize_pty(rows, cols);
        }
    }
}

fn layout_from_panes(panes: &[PaneRuntimeMeta]) -> Option<PaneLayoutNode> {
    let mut iter = panes.iter();
    let first = iter.next()?;
    let mut root = PaneLayoutNode::Leaf { pane_id: first.id };
    for pane in iter {
        root = PaneLayoutNode::Split {
            direction: PaneSplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(root),
            second: Box::new(PaneLayoutNode::Leaf { pane_id: pane.id }),
        };
    }
    Some(root)
}

// Process-wide bound on in-flight OS process-identity probes: they occupy OS
// threads shared by every activation in this process.
/// Process start identity probes running (see [`crate::run_shared`]).
enum IdentityProbes {}

fn identity_probes() -> Arc<AtomicUsize> {
    crate::run_shared::run_shared::<IdentityProbes, _>(|| AtomicUsize::new(0))
}

fn startup_stop_requested(stop: &mut oneshot::Receiver<()>) -> bool {
    !matches!(stop.try_recv(), Err(oneshot::error::TryRecvError::Empty))
}

async fn startup_identity_or_stop(
    identity: impl std::future::Future<Output = Option<ProcessStartIdentity>>,
    stop: &mut oneshot::Receiver<()>,
) -> Result<Option<ProcessStartIdentity>, ()> {
    switchy_async::select! {
        biased;
        _ = stop => Err(()),
        identity = identity => Ok(identity),
    }
}

async fn bounded_process_start_identity(pid: u32) -> Option<ProcessStartIdentity> {
    bounded_start_identity_probe(move || process_start_identity_for_pid(pid)).await
}

async fn bounded_start_identity_probe(
    probe: impl FnOnce() -> Option<ProcessStartIdentity> + Send + 'static,
) -> Option<ProcessStartIdentity> {
    struct Permit(Arc<AtomicUsize>);
    impl Drop for Permit {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    let probes = identity_probes();
    probes
        .try_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
            (count < 4).then_some(count + 1)
        })
        .ok()?;
    let permit = Permit(probes);
    let (sender, receiver) = oneshot::channel();
    switchy_thread::thread::Builder::new()
        .name("bmux-process-identity".into())
        .spawn(move || {
            let _permit = permit;
            let result = probe();
            let _ = sender.send(result);
        })
        .ok()?;
    // Dropping this future abandons the result, not the admission permit.
    // A stalled native query continues to occupy one of the four slots.
    switchy_async::time::timeout(Duration::from_millis(250), receiver)
        .await
        .ok()?
        .ok()?
}

fn observe_process_directory_for_identity(
    pid: u32,
    expected: ProcessStartIdentity,
) -> Option<String> {
    if process_start_identity_for_pid(pid)? != expected {
        return None;
    }
    let directory = crate::platform::process_directory(pid)?;
    (process_start_identity_for_pid(pid)? == expected).then_some(directory)
}

fn validate_initial_shell_options(
    options: Option<&bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::InitialShellOptions>,
) -> Result<()> {
    if let Some(cwd) = options.and_then(|options| options.cwd.as_deref())
        && (!Path::new(cwd).is_absolute() || !crate::files::is_dir(Path::new(cwd)))
    {
        anyhow::bail!("initial shell directory must be an available absolute directory: {cwd}");
    }
    Ok(())
}

fn pane_launch_spec_from_command(command: PaneLaunchCommand) -> Result<PaneLaunchSpec> {
    if command.program.trim().is_empty() {
        anyhow::bail!("pane launch command program cannot be empty");
    }
    Ok(PaneLaunchSpec {
        program: command.program,
        args: command.args,
        cwd: command.cwd,
        env: command.env,
    })
}

impl SessionRuntimeManager {
    #[allow(clippy::too_many_arguments)]
    fn new(
        host: PaneRuntimeHost,
        shell: String,
        pane_term: String,
        protocol_profile: ProtocolProfile,
        bracketed_paste_supported: bool,
        shell_integration_root: Option<std::path::PathBuf>,
        padding_config: PanePaddingConfig,
        pane_exit_tx: mpsc::UnboundedSender<PaneExitEvent>,
    ) -> Self {
        if let Some(root) = &shell_integration_root
            && let Err(error) = shell_callback::prepare(root)
        {
            warn!(%error, "failed retaining shell callback executable");
        }
        Self {
            runtimes: BTreeMap::new(),
            pane_input_index: Arc::new(RwLock::new(BTreeMap::new())),
            client_write_permissions: Arc::new(RwLock::new(BTreeMap::new())),
            retained_output_bytes: Arc::new(switchy_thread::sync::atomic::AtomicUsize::new(0)),
            retained_pin_bytes: Arc::new(AtomicUsize::new(0)),
            shell,
            pane_term,
            protocol_profile,
            bracketed_paste_supported,
            shell_integration_root,
            padding_config,
            restored_padding_overrides: BTreeMap::new(),
            padding_previews: BTreeMap::new(),
            pending_layout_revisions: BTreeMap::new(),
            pane_exit_tx,
            host,
        }
    }

    fn register_pane_input(&self, session_id: SessionId, pane: &PaneRuntimeHandle) {
        let handle = pane.input_handle(session_id);
        if let Ok(mut index) = self.pane_input_index.write() {
            index.insert(pane.meta.id, handle);
        }
    }

    fn unregister_pane_input(&self, pane_id: Uuid) {
        if let Ok(mut index) = self.pane_input_index.write() {
            index.remove(&pane_id);
        }
    }

    fn unregister_session_inputs(
        &self,
        session_id: SessionId,
        pane_ids: impl IntoIterator<Item = Uuid>,
    ) {
        if let Ok(mut index) = self.pane_input_index.write() {
            for pane_id in pane_ids {
                if index
                    .get(&pane_id)
                    .is_some_and(|handle| handle.session_id == session_id)
                {
                    index.remove(&pane_id);
                }
            }
        }
        if let Ok(mut permissions) = self.client_write_permissions.write() {
            permissions.remove(&session_id);
        }
    }

    fn prepare_spawn_directory_probe(
        &self,
        source: &bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SourceExecution,
    ) -> std::result::Result<
        SpawnDirectoryProbe,
        bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SpawnError,
    > {
        use bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SpawnError;
        let pane = self
            .runtimes
            .get(&SessionId(source.session_id))
            .and_then(|runtime| runtime.panes.get(&source.pane_id))
            .ok_or(SpawnError::SourceUnavailable)?;
        let observation = pane
            .resurrection_state
            .try_lock()
            .map_err(|error| match error {
                switchy_thread::sync::TryLockError::WouldBlock => SpawnError::Busy,
                switchy_thread::sync::TryLockError::Poisoned(_) => SpawnError::SourceUnavailable,
            })?;
        if observation.observation_execution != source.execution_token {
            return Err(SpawnError::StaleExecution);
        }
        if pane.exited.load(Ordering::SeqCst) {
            return Err(SpawnError::SourceUnavailable);
        }
        if let Some(path) = observation.observed_cwd.clone() {
            Ok(SpawnDirectoryProbe::Shell(path))
        } else {
            let pid = *pane.process_id.try_lock().map_err(|_| SpawnError::Busy)?;
            let identity = *pane
                .process_start_identity
                .try_lock()
                .map_err(|_| SpawnError::Busy)?;
            pid.zip(identity)
                .map(|(pid, identity)| SpawnDirectoryProbe::Process(pid, identity))
                .ok_or(SpawnError::ObservationUnavailable)
        }
    }

    fn commit_fresh_runtime(
        &mut self,
        session_id: SessionId,
        prepared: &mut PreparedFreshRuntime,
        publish: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        anyhow::ensure!(
            !self.runtimes.contains_key(&session_id),
            "runtime already exists for session {}",
            session_id.0
        );
        let pending = prepared.runtime.as_ref().expect("uncommitted runtime");
        for pane in pending.panes.values() {
            anyhow::ensure!(
                matches!(&*pane.startup.borrow(), Some(Ok(())))
                    && (!pane.exited.load(Ordering::SeqCst) || pane.meta.launch.is_some()),
                "pane is not ready for startup commit"
            );
        }
        // Validate before publishing the roster, under the same manager lock.
        // After publication there must be no fallible runtime validation that
        // can reject a session observers have already seen.
        publish()?;
        let runtime = prepared.runtime.take().expect("ready runtime ownership");
        self.runtimes.insert(session_id, runtime);
        for pane in self.runtimes[&session_id].panes.values() {
            self.register_pane_input(session_id, pane);
            pane.publication.decide(true);
        }
        Ok(())
    }

    #[cfg(test)]
    fn start_runtime_with_options(
        &mut self,
        session_id: SessionId,
        options: Option<bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::InitialShellOptions>,
    ) -> Result<()> {
        let runtime = self.prepare_fresh_runtime(session_id, options)?.take()?;
        self.runtimes.insert(session_id, runtime);
        for pane in self.runtimes[&session_id].panes.values() {
            self.register_pane_input(session_id, pane);
            pane.publication.decide(true);
        }
        Ok(())
    }

    // Preparation deliberately does not register input or publish the runtime
    // in the manager. Background-event gating is a separate transaction step.
    fn prepare_fresh_runtime(
        &self,
        session_id: SessionId,
        options: Option<bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::InitialShellOptions>,
    ) -> Result<PreparedFreshRuntime> {
        self.prepare_fresh_program_runtime(session_id, options, None, (24, 80))
    }

    fn prepare_fresh_program_runtime(
        &self,
        session_id: SessionId,
        options: Option<bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::InitialShellOptions>,
        launch: Option<PaneLaunchSpec>,
        size: (u16, u16),
    ) -> Result<PreparedFreshRuntime> {
        self.prepare_reserved_program_runtime(session_id, options, launch, size, None)
    }

    fn prepare_reserved_program_runtime(
        &self,
        session_id: SessionId,
        options: Option<bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::InitialShellOptions>,
        launch: Option<PaneLaunchSpec>,
        size: (u16, u16),
        reserved: Option<
            &bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalExecutionRef,
        >,
    ) -> Result<PreparedFreshRuntime> {
        reservations::check_authority(&self.host)?;
        validate_initial_shell_options(options.as_ref())?;
        if self.runtimes.contains_key(&session_id) {
            anyhow::bail!("runtime already exists for session {}", session_id.0);
        }

        let cleanup = PreparedCleanup::reserve(&self.host)?;
        let first_pane_id =
            reserved.map_or_else(crate::ids::new_uuid, |reference| reference.pane_id);
        let managed_program = launch.is_some();
        let pane_meta = PaneRuntimeMeta {
            id: first_pane_id,
            name: Some("pane-1".to_string()),
            shell: self.shell.clone(),
            launch,
            resurrection: PaneResurrectionSnapshot::default(),
        };
        let (launch, permit) = oneshot::channel();
        let first_pane = self.spawn_pane_runtime_gated(
            session_id,
            pane_meta,
            size,
            false,
            options,
            Some(permit),
        );
        if let Some(reference) = reserved {
            first_pane
                .resurrection_state
                .lock()
                .map_err(|_| anyhow::anyhow!("execution identity lock poisoned"))?
                .observation_execution = reference.execution_token;
        }
        first_pane
            .execution
            .managed_program
            .store(managed_program, Ordering::SeqCst);
        let mut panes = BTreeMap::new();
        panes.insert(first_pane_id, first_pane);

        Ok(PreparedFreshRuntime {
            cleanup,
            launch: Some(launch),
            runtime: Some(SessionRuntimeHandle {
                panes,
                layout_root: Some(PaneLayoutNode::Leaf {
                    pane_id: first_pane_id,
                }),
                focused_pane_id: first_pane_id,
                zoomed_pane_id: None,
                floating_surfaces: Vec::new(),
                attached_clients: BTreeSet::new(),
                scrollback_pins: BTreeMap::new(),
                next_scrollback_pin_id: 1,
                attach_viewport: None,
                attach_view_revision: 0,
            }),
        })
    }

    // Restore validation, initial geometry, pane spawning, and final invariant
    // checks form one ordered transaction; splitting it would obscure the rule
    // that restored overrides must affect PTY size before processes start.
    #[allow(clippy::too_many_lines)]
    fn restore_runtime(
        &mut self,
        session_id: SessionId,
        panes: &[PaneRuntimeMeta],
        request: RestoreRuntimeRequest,
    ) -> Result<()> {
        reservations::check_authority(&self.host)?;
        let _admission = managed_idle::admit(&self.host)?;
        let RestoreRuntimeRequest {
            layout_root,
            focused_pane_id,
            zoomed_pane_id,
            floating_surfaces,
            attach_viewport,
        } = request;

        if self.runtimes.contains_key(&session_id) {
            anyhow::bail!("runtime already exists for session {}", session_id.0);
        }

        if panes.is_empty() {
            anyhow::bail!("restored runtime must include panes");
        }
        if !panes.iter().any(|pane| pane.id == focused_pane_id) {
            anyhow::bail!("focused pane missing from restored runtime");
        }
        // Heal persisted shell/cwd/command metadata before resolving padding rules
        // and initial PTY geometry, so shell/command matchers see the same metadata
        // the spawned pane will own.
        let panes = panes
            .iter()
            .cloned()
            .map(|mut pane| {
                if let Ok(resolved) = resolve_pane_launch(&pane, &self.shell) {
                    apply_resolved_pane_launch(&mut pane, &resolved);
                }
                pane
            })
            .collect::<Vec<_>>();

        let runtime_layout_root = layout_root.or_else(|| layout_from_panes(&panes));

        // Re-validate the persisted zoom against the same invariants
        // `toggle_zoom` enforces: the pane must be a tiled pane in this
        // layout, and zoom is meaningless below two tiled panes. A
        // snapshot written by an older build (or mutated offline) simply
        // restores unzoomed rather than producing an inconsistent scene.
        let mut tiled_pane_ids = Vec::new();
        if let Some(layout_root) = &runtime_layout_root {
            layout_root.pane_order(&mut tiled_pane_ids);
        }
        let restored_zoomed_pane_id = zoomed_pane_id
            .filter(|zoomed_id| tiled_pane_ids.len() >= 2 && tiled_pane_ids.contains(zoomed_id));
        if zoomed_pane_id.is_some() && restored_zoomed_pane_id.is_none() {
            warn!(
                session_id = %session_id.0,
                "dropping persisted zoom that no longer satisfies zoom invariants"
            );
        }

        let mut initial_pane_sizes = BTreeMap::new();
        if let (Some(layout_root), Some(viewport)) = (&runtime_layout_root, attach_viewport) {
            let scene_root = scene_root_from_viewport(Some(viewport));
            if let Some(zoomed_id) = restored_zoomed_pane_id {
                // A zoomed pane fills the scene root, mirroring the zoom
                // branch of `resize_session_ptys`, so its PTY must start
                // full-size instead of at its tiled rect.
                let restored_override = self
                    .restored_padding_overrides
                    .get(&(session_id, zoomed_id))
                    .copied();
                initial_pane_sizes.insert(
                    zoomed_id,
                    pane_pty_size_for_metadata(
                        &self.padding_config,
                        panes
                            .iter()
                            .find(|pane| pane.id == zoomed_id)
                            .expect("zoomed pane exists"),
                        restored_override,
                        scene_root,
                    ),
                );
            } else {
                let mut rects = BTreeMap::new();
                collect_layout_rects(layout_root, scene_root, &mut rects);
                for (pane_id, rect) in rects {
                    let pane = panes
                        .iter()
                        .find(|pane| pane.id == pane_id)
                        .expect("layout pane exists");
                    initial_pane_sizes.insert(
                        pane_id,
                        pane_pty_size_for_metadata(
                            &self.padding_config,
                            pane,
                            self.restored_padding_overrides
                                .get(&(session_id, pane_id))
                                .copied(),
                            rect,
                        ),
                    );
                }
            }
        }

        let mut runtime_panes = BTreeMap::new();
        for pane_meta in panes {
            let initial_size = initial_pane_sizes
                .get(&pane_meta.id)
                .copied()
                .unwrap_or((24, 80));
            let pane = self.spawn_pane_runtime_with_restore(
                session_id,
                pane_meta.clone(),
                initial_size,
                true,
                None,
            );
            let restored_meta = pane.meta.clone();
            runtime_panes.insert(restored_meta.id, pane);
        }

        self.runtimes.insert(
            session_id,
            SessionRuntimeHandle {
                panes: runtime_panes,
                layout_root: runtime_layout_root,
                focused_pane_id,
                zoomed_pane_id: restored_zoomed_pane_id,
                floating_surfaces,
                attached_clients: BTreeSet::new(),
                scrollback_pins: BTreeMap::new(),
                next_scrollback_pin_id: 1,
                attach_viewport,
                attach_view_revision: 0,
            },
        );
        let runtime = self
            .runtimes
            .get(&session_id)
            .expect("runtime inserted before validation");
        validate_runtime_layout_matches_panes(runtime)?;
        for pane in runtime.panes.values() {
            self.register_pane_input(session_id, pane);
        }

        Ok(())
    }

    fn spawn_pane_runtime(
        &self,
        session_id: SessionId,
        pane_meta: PaneRuntimeMeta,
        initial_size: (u16, u16),
    ) -> PaneRuntimeHandle {
        self.spawn_pane_runtime_with_restore(session_id, pane_meta, initial_size, false, None)
    }

    fn spawn_pane_runtime_with_restore(
        &self,
        session_id: SessionId,
        pane_meta: PaneRuntimeMeta,
        initial_size: (u16, u16),
        restoring: bool,
        initial_shell: Option<
            bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::InitialShellOptions,
        >,
    ) -> PaneRuntimeHandle {
        self.spawn_pane_runtime_gated(
            session_id,
            pane_meta,
            initial_size,
            restoring,
            initial_shell,
            None,
        )
    }

    // The launch permit is owned by fresh preparation; existing restore and
    // split paths start immediately. Keep startup ordering in one state machine.
    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    fn spawn_pane_runtime_gated(
        &self,
        session_id: SessionId,
        mut pane_meta: PaneRuntimeMeta,
        initial_size: (u16, u16),
        restoring: bool,
        initial_shell: Option<
            bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::InitialShellOptions,
        >,
        launch_permit: Option<oneshot::Receiver<()>>,
    ) -> PaneRuntimeHandle {
        let host = self.host.clone();
        let host = &host;
        let host = host.clone();
        let program_execution = pane_meta.launch.is_some();
        let execution = Arc::new(execution::ExecutionCapture::default());
        let execution_for_reader = Arc::clone(&execution);
        let execution_for_waiter = Arc::clone(&execution);
        let execution_for_task = Arc::clone(&execution);
        let publication = Arc::new(StartupPublication::default());
        if launch_permit.is_none() {
            publication.decide(true);
        }
        let publication_for_task = Arc::clone(&publication);
        let resolved_launch = resolve_pane_launch(&pane_meta, &self.shell);
        if let Ok(resolved) = &resolved_launch {
            if resolved.shell != pane_meta.shell {
                warn!(
                    pane_id = %pane_meta.id,
                    persisted_shell = %pane_meta.shell,
                    resolved_shell = %resolved.shell,
                    "replacing unavailable persisted pane shell"
                );
            }
            if pane_meta.resurrection.active_command.is_some() && resolved.replay_command.is_none()
            {
                warn!(
                    pane_id = %pane_meta.id,
                    persisted_shell = %pane_meta.shell,
                    resolved_shell = %resolved.shell,
                    "suppressing resurrected command replay across incompatible shells"
                );
            }
            apply_resolved_pane_launch(&mut pane_meta, resolved);
            if resolved.metadata_changed {
                host.mark_snapshot_dirty();
            }
        }
        let (stop_tx, mut stop_rx) = oneshot::channel();
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<PaneRuntimeCommand>();
        let output_buffer = Arc::new(switchy_thread::sync::Mutex::new(
            OutputFanoutBuffer::with_accounting(
                MAX_WINDOW_OUTPUT_BUFFER_BYTES,
                Arc::clone(&self.retained_output_bytes),
            ),
        ));
        let (initial_rows, initial_cols) = sanitize_pty_size(initial_size.0, initial_size.1);
        let terminal_grid = Arc::new(switchy_thread::sync::Mutex::new(
            TerminalGridStream::new(initial_cols, initial_rows, GridLimits::default())
                .expect("initial pane terminal grid dimensions are valid"),
        ));
        let terminal_grid_for_reader = Arc::clone(&terminal_grid);
        let frame_replicas: Arc<switchy_thread::sync::Mutex<BTreeMap<ClientId, PaneFrameReplica>>> =
            Arc::new(switchy_thread::sync::Mutex::new(BTreeMap::new()));
        let frame_replicas_for_reader = Arc::clone(&frame_replicas);
        let signals = Arc::new(switchy_thread::sync::Mutex::new(
            RetainedPaneSignals::default(),
        ));
        let signals_for_reader = Arc::clone(&signals);
        let last_requested_size = Arc::new(switchy_thread::sync::Mutex::new((
            initial_rows,
            initial_cols,
        )));
        let shell = pane_meta.shell.clone();
        let launch = pane_meta.launch.clone();
        let launch_resolution_error = resolved_launch.err().map(|error| error.to_string());
        let pane_term = self.pane_term.clone();
        let protocol_profile = self.protocol_profile;
        let bracketed_paste_supported = self.bracketed_paste_supported;
        let pane_id = pane_meta.id;
        let replay_command = pane_meta.resurrection.active_command.clone();
        let initial_cwd = initial_shell
            .and_then(|options| options.cwd)
            .or_else(|| pane_meta.resurrection.last_known_cwd.clone());
        let pane_exit_tx = self.pane_exit_tx.clone();
        let shell_marker_token = crate::ids::new_uuid().simple().to_string();
        let shell_integration_root = self
            .shell_integration_root
            .as_ref()
            .map(|root| root.join(&shell_marker_token));
        let shell_callback = self
            .shell_integration_root
            .as_deref()
            .map(|root| shell_callback::prepare(root).map_err(|error| error.to_string()));
        let output_buffer_for_reader = Arc::clone(&output_buffer);
        let process_start_identity = Arc::new(switchy_thread::sync::Mutex::new(None));
        let process_start_identity_for_task = Arc::clone(&process_start_identity);
        let process_id = Arc::new(switchy_thread::sync::Mutex::new(None));
        let process_id_for_task = Arc::clone(&process_id);
        let process_group_id = Arc::new(switchy_thread::sync::Mutex::new(None));
        let process_group_id_for_task = Arc::clone(&process_group_id);
        let resurrection_state = Arc::new(switchy_thread::sync::Mutex::new(
            PaneResurrectionRuntime::from_snapshot(&pane_meta.resurrection),
        ));
        let resurrection_state_for_reader = Arc::clone(&resurrection_state);
        let resurrection_state_for_waiter_seed = Arc::clone(&resurrection_state);
        let pending_prompt_replay = Arc::new(switchy_thread::sync::Mutex::new(None::<String>));
        let pending_prompt_replay_for_reader = Arc::clone(&pending_prompt_replay);
        let exit_reason = Arc::new(switchy_thread::sync::Mutex::new(None::<String>));
        let exit_reason_for_task = Arc::clone(&exit_reason);
        let exited = Arc::new(AtomicBool::new(false));
        let exited_for_task = Arc::clone(&exited);

        let output_dirty = Arc::new(AtomicBool::new(false));
        let output_dirty_for_reader = Arc::clone(&output_dirty);
        let last_requested_size_for_reader = Arc::clone(&last_requested_size);
        let last_requested_size_for_input = Arc::clone(&last_requested_size);
        let mouse_protocol_state = Arc::new(switchy_thread::sync::Mutex::new(
            AttachMouseProtocolState::default(),
        ));
        let mouse_protocol_state_for_reader = Arc::clone(&mouse_protocol_state);
        let input_mode_state = Arc::new(switchy_thread::sync::Mutex::new(
            AttachInputModeState::default(),
        ));
        let input_mode_state_for_reader = Arc::clone(&input_mode_state);

        #[cfg(feature = "image-registry")]
        let image_config = bmux_config::BmuxConfig::load()
            .unwrap_or_default()
            .behavior
            .images;
        #[cfg(feature = "image-registry")]
        let pane_image_protocol = image_config.enabled.then_some("kitty");
        #[cfg(not(feature = "image-registry"))]
        let pane_image_protocol: Option<&str> = None;

        #[cfg(feature = "image-registry")]
        let image_registry = {
            let img_config = image_config.clone();
            Arc::new(switchy_thread::sync::Mutex::new(
                (if img_config.enabled {
                    #[allow(clippy::cast_possible_truncation)]
                    bmux_image::ImageRegistry::new(
                        img_config.max_images_per_pane as usize,
                        img_config.max_image_bytes as usize,
                    )
                } else {
                    // Disabled: zero-capacity registry that drops everything.
                    bmux_image::ImageRegistry::new(0, 0)
                })
                .with_viewport_height(initial_rows),
            ))
        };
        #[cfg(feature = "image-registry")]
        let image_registry_for_reader = Arc::clone(&image_registry);
        #[cfg(feature = "image-registry")]
        let cell_pixel_size = Arc::new(switchy_thread::sync::Mutex::new((0u16, 0u16)));
        #[cfg(feature = "image-registry")]
        let cell_pixel_size_for_reader = Arc::clone(&cell_pixel_size);
        #[cfg(feature = "image-registry")]
        let image_dirty = Arc::new(AtomicBool::new(false));
        #[cfg(feature = "image-registry")]
        let image_dirty_for_reader = Arc::clone(&image_dirty);

        let (startup_tx, startup) = watch::channel(None);
        let (cleanup_tx, cleanup) = watch::channel(None);
        let fence_input_for_reader = input_tx.clone();
        let task_host = host.clone();
        let task = switchy_async::task::spawn(async move {
            let host = task_host;
            let _publication_guard = PendingPublicationGuard(Arc::clone(&publication_for_task));
            if let Some(permit) = launch_permit {
                switchy_async::select! {
                    biased;
                    _ = &mut stop_rx => { cleanup_tx.send_replace(Some(Ok(()))); return; },
                    result = permit => if result.is_err() { cleanup_tx.send_replace(Some(Ok(()))); return; },
                }
            }
            let mut child_spawned = false;
            macro_rules! fail_startup {
                ($reason:expr) => {{
                    let reason = $reason;
                    // Before spawn there is no process or supervision thread to
                    // reap. Later failures must supply their actual cleanup result.
                    if !child_spawned {
                        cleanup_tx.send_replace(Some(Ok(())));
                    }
                    startup_tx.send_replace(Some(Err(reason.clone())));
                    mark_pane_startup_failed(
                        &host,
                        session_id,
                        pane_id,
                        &reason,
                        &PaneFailureTargets {
                            process_id: &process_id_for_task,
                            process_group_id: &process_group_id_for_task,
                            exit_reason: &exit_reason_for_task,
                            exited: &exited_for_task,
                            output_buffer: &output_buffer_for_reader,
                            terminal_grid: &terminal_grid_for_reader,
                            output_dirty: &output_dirty_for_reader,
                            pane_exit_tx: &pane_exit_tx,
                        },
                    );
                }};
            }
            if let Some(reason) = launch_resolution_error {
                fail_startup!(reason);
                return;
            }
            let pty_pair = match bmux_pty::PtyPair::open(PtySize {
                rows: initial_rows,
                cols: initial_cols,
            }) {
                Ok(pair) => pair,
                Err(error) => {
                    fail_startup!(format!("failed to allocate PTY: {error}"));
                    return;
                }
            };

            let managed_program = execution_for_task.managed_program.load(Ordering::SeqCst);
            let token = resurrection_state_for_reader
                .lock()
                .expect("execution identity lock")
                .observation_execution;
            let session_preparation = match session_control::SessionControl::prepare() {
                Ok(preparation) => preparation,
                Err(error) => {
                    fail_startup!(format!("terminal session preparation failed: {error:#}"));
                    return;
                }
            };
            let application_resume = if managed_program
                || (launch.is_none()
                    && !matches!(
                        shell_kind_for_path(&shell),
                        ShellKind::Bash | ShellKind::Zsh | ShellKind::Fish | ShellKind::Nu
                    )) {
                None
            } else {
                match crate::application_resurrection::prepare(
                    &host,
                    pane_id.to_string(),
                    restoring,
                    launch.is_none(),
                    replay_command.is_some(),
                    resurrection_state_for_reader
                        .lock()
                        .ok()
                        .and_then(|state| state.execution_token.clone()),
                ) {
                    Ok(value) => value,
                    Err(error) => {
                        fail_startup!(format!("application resurrection failed: {error}"));
                        return;
                    }
                }
            };
            if let Some(resume) = &application_resume {
                if let Ok(mut state) = resurrection_state_for_reader.lock() {
                    state.launch = Some(resume.clone());
                    state.execution_token = Some(resume.invocation.clone());
                }
                host.mark_snapshot_dirty();
            }
            let mut replay_on_prompt = false;

            let (mut command, failed_spawn_label) = if let Some(launch) = launch.as_ref() {
                if let Err(error) = validate_explicit_launch(launch) {
                    fail_startup!(error.to_string());
                    return;
                }
                let mut command = SpawnCommand::new(&launch.program);
                configure_pane_terminal_env(&mut command, &pane_term, pane_image_protocol);
                for arg in &launch.args {
                    command.arg(arg);
                }
                for (key, value) in &launch.env {
                    command.env(key, value);
                }
                if execution_for_task
                    .exact_environment
                    .lock()
                    .ok()
                    .is_none_or(|env| env.is_none())
                {
                    configure_device_seal_broker_env(&mut command);
                }
                if let Some(cwd) = launch.cwd.as_deref().or(initial_cwd.as_deref()) {
                    command.cwd(cwd);
                }
                (command, format!("command {}", launch.program))
            } else {
                let mut command = SpawnCommand::new(&shell);
                configure_pane_terminal_env(&mut command, &pane_term, pane_image_protocol);
                configure_device_seal_broker_env(&mut command);
                if let Some(cwd) = initial_cwd.as_deref()
                    && !cwd.is_empty()
                {
                    command.cwd(cwd);
                }
                let integration_configured = configure_shell_integration_command(
                    &mut command,
                    &shell,
                    shell_integration_root.as_deref(),
                    &shell_marker_token,
                )
                .map_or_else(
                    |error| {
                        warn!("failed configuring shell integration for pane {pane_id}: {error:#}");
                        false
                    },
                    |()| shell_integration_root.is_some(),
                );
                replay_on_prompt = integration_configured
                    && matches!(
                        shell_kind_for_path(&shell),
                        ShellKind::Bash | ShellKind::Zsh | ShellKind::Fish | ShellKind::Nu
                    );
                (command, format!("shell '{shell}'"))
            };
            clear_managed_launch_env(&mut command);
            command.env_remove("APPLICATION_RESURRECTION_HOST");
            command.env_remove("BMUX_ROOT_SHELL_PID");
            command.env_remove("BMUX_SHELL_CALLBACK");
            command.env_remove("__BMUX_NESTED_PID");
            command.env("BMUX_SHELL_MARKER", &shell_marker_token);
            if let Some(root) = &shell_integration_root {
                let share = root.join("nested-share");
                let config = share.join("nushell/vendor/autoload/bmux.nu");
                if let Err(error) =
                    write_shell_integration_file(&config, shell_integration_nested_nu())
                {
                    warn!(%error, "nested shell integration unavailable");
                }
                let fish_config = share.join("fish/vendor_conf.d/bmux.fish");
                if let Err(error) = write_shell_integration_file(
                    &fish_config,
                    "if status is-interactive; and set -q BMUX_SHELL_CALLBACK; and set -q APPLICATION_RESURRECTION_HOST\n  $BMUX_SHELL_CALLBACK __shell-integration fish | source\nend\n",
                ) {
                    warn!(%error, "nested Fish integration unavailable");
                }
                match &shell_callback {
                    Some(Ok(executable)) => {
                        command.env("BMUX_SHELL_CALLBACK", executable);
                    }
                    Some(Err(error)) => {
                        fail_startup!(format!("shell callback executable unavailable: {error}"));
                        return;
                    }
                    None => unreachable!("shell integration requires a retained callback"),
                }
                let dirs = pane_xdg_data_dirs(share, switchy_env::var_os("XDG_DATA_DIRS"));
                if let Ok(value) = std::env::join_paths(dirs) {
                    command.env("XDG_DATA_DIRS", value);
                }
            }
            if let Some(resume) = &application_resume {
                command.env("APPLICATION_RESURRECTION_HOST", &resume.endpoint);
            }
            // One conventional owner path for explicit programs and prepared
            // initial shells. Preserve the prepared argv/cwd/environment rather
            // than constructing a second shell command or idle ctty anchor.
            let argv = match command
                .argv()
                .iter()
                .map(|arg| {
                    arg.to_str().map(str::to_owned).ok_or_else(|| {
                        anyhow::anyhow!(
                            "non-UTF8 program argument unsupported by execution contract"
                        )
                    })
                })
                .collect::<Result<Vec<_>>>()
            {
                Ok(argv) if !argv.is_empty() => argv,
                Ok(_) => {
                    fail_startup!("prepared execution argv is empty".to_string());
                    return;
                }
                Err(error) => {
                    fail_startup!(error.to_string());
                    return;
                }
            };
            let exact_environment = execution_for_task
                .exact_environment
                .lock()
                .ok()
                .and_then(|env| env.clone())
                .map(|env| environment::terminal_environment(env, &command));
            if let Some(entries) = &exact_environment
                && let Err(error) = environment::apply_exact(&mut command, entries)
            {
                fail_startup!(format!("prepared execution environment invalid: {error}"));
                return;
            }
            let launch_spec = PaneLaunchSpec {
                program: argv[0].clone(),
                args: argv[1..].to_vec(),
                cwd: command
                    .get_cwd()
                    .and_then(|path| path.to_str())
                    .map(str::to_owned),
                env: command
                    .utf8_environment()
                    .map(|(key, value)| (key.to_owned(), value.to_owned()))
                    .collect(),
            };
            if let Err(error) = validate_explicit_launch(&launch_spec) {
                fail_startup!(format!("prepared execution command invalid: {error}"));
                return;
            }
            // PTY allocation and command preparation may have overlapped an
            // abandoned request. Check again at the process side-effect boundary.
            if startup_stop_requested(&mut stop_rx) {
                if let Some(resume) = &application_resume {
                    resume.close();
                }
                fail_startup!("pane startup cancelled before spawn".to_string());
                return;
            }
            let spawn_authorization = Arc::clone(&execution_for_task);
            let authorization =
                switchy_async::task::spawn_blocking(move || spawn_authorization.authorize_spawn())
                    .await
                    .map_err(|error| anyhow::anyhow!("spawn authorization worker failed: {error}"))
                    .and_then(std::convert::identity);
            if let Err(error) = authorization {
                fail_startup!(format!("execution spawn authorization rejected: {error}"));
                return;
            }
            if startup_stop_requested(&mut stop_rx) {
                fail_startup!("pane startup cancelled during authorization".to_string());
                return;
            }
            let raw_reader = match pty_pair.master.try_clone_reader() {
                Ok(reader) => Box::new(reader) as Box<dyn Read + Send>,
                Err(error) => {
                    fail_startup!(format!("failed to install PTY capture: {error}"));
                    return;
                }
            };
            // A pseudo-console (ConPTY) asks for its inherited cursor before the
            // program it hosts can run. Own this PTY's writer before starting the
            // raw drain so the bootstrap reader can answer before the spawn below.
            let console_query = bmux_pty::behavior().startup_cursor_query;
            let early_writer = if console_query {
                match pty_pair.master.take_writer() {
                    Ok(writer) => Some(Arc::new(switchy_thread::sync::Mutex::new(
                        Box::new(writer) as Box<dyn std::io::Write + Send>,
                    ))),
                    Err(error) => {
                        fail_startup!(format!("failed to open startup PTY writer: {error}"));
                        return;
                    }
                }
            } else {
                None
            };
            let raw_reader = match &early_writer {
                Some(writer) => Box::new(console_startup::BootstrapReader::new(
                    raw_reader,
                    Arc::clone(writer),
                    pane_id,
                )) as Box<dyn Read + Send>,
                None => raw_reader,
            };
            // Reserved capture was installed before the launch permit opened.
            // Transfer the sole disk owner before starting any raw PTY reader.
            let pending_capture = execution_for_task
                .durable_capture
                .lock()
                .expect("capture authority lock")
                .take();
            let mut writer_owner = None;
            let capture_writer = match pending_capture {
                Some(capture) => match capture_writer::CaptureWriter::start(capture) {
                    Ok((writer, owner)) => {
                        *execution_for_task
                            .capture_writer
                            .lock()
                            .expect("capture writer lock") = Some(writer.clone());
                        writer_owner = Some(owner);
                        let mut changes = writer.subscribe();
                        let execution = Arc::downgrade(&execution_for_task);
                        let observed = writer.clone();
                        switchy_async::task::spawn(async move {
                            loop {
                                let Some(execution) = execution.upgrade() else {
                                    break;
                                };
                                let terminal = match observed.progress() {
                                    Ok(progress) => {
                                        if let Some(error) = progress
                                            .failure
                                            .as_ref()
                                            .or(progress.boundary.error.as_ref())
                                        {
                                            execution.record_error(error.clone());
                                        }
                                        progress.failure.is_some()
                                            || progress.boundary.eof
                                            || progress.boundary.error.is_some()
                                    }
                                    Err(error) => {
                                        execution.record_error(error.to_string());
                                        true
                                    }
                                };
                                execution.notify();
                                drop(execution);
                                if terminal || changes.changed().await.is_err() {
                                    break;
                                }
                            }
                        });
                        Some(writer)
                    }
                    Err(error) => {
                        fail_startup!(format!("failed to start durable capture writer: {error}"));
                        return;
                    }
                },
                None => None,
            };
            let capture_started = match &capture_writer {
                Some(writer) => startup_capture::start_ordered(
                    raw_reader,
                    Arc::clone(&execution_for_task),
                    writer.clone(),
                ),
                None => {
                    startup_capture::start_captured(raw_reader, Arc::clone(&execution_for_task))
                }
            };
            let (mut reader, capture_thread) = match capture_started {
                Ok(capture) => capture,
                Err(error) => {
                    let mut reason = format!("failed to start PTY capture: {error}");
                    if let (Some(mut owner), Some(writer)) =
                        (writer_owner.take(), capture_writer.as_ref())
                    {
                        writer.fail(&reason);
                        reason = startup_capture::acknowledge_cleanup(
                            &host,
                            vec![owner.take()],
                            &cleanup_tx,
                            Some(Ok(())),
                            reason,
                        )
                        .await;
                        child_spawned = true;
                    }
                    fail_startup!(reason);
                    return;
                }
            };
            let mut capture_owner = startup_capture::CaptureOwner::new(capture_thread);
            if let (Some(owner), Some(writer)) = (writer_owner.take(), capture_writer.as_ref()) {
                capture_owner.add_writer(owner, writer.clone());
            }
            macro_rules! fail_captured_startup {
                ($reason:expr, $previous:expr) => {{
                    let reason = $reason;
                    let previous = $previous;
                    if let Some(writer) = &capture_writer { writer.fail("startup rejected before capture drain"); }
                    drop(reader);
                    drop(pty_pair);
                    // `inject_yields` cannot see inside a macro: yield here by hand.
                    let reason = switchy_async::task::yield_after(startup_capture::acknowledge_cleanup(&host, capture_owner.take(), &cleanup_tx, previous, reason)).await;
                    child_spawned = true; // Capture cleanup has supplied the actual acknowledgement.
                    fail_startup!(reason);
                }};
            }
            let (startup_cleanup_tx, _) = watch::channel(None);
            let spawned = match execution_for_task.authorize_spawn() {
                Ok(()) => session_preparation
                    .spawn(&pty_pair.slave, &command)
                    .map(|child| StartupChild::new(&host, child))
                    .map_err(anyhow::Error::from),
                Err(error) => Err(error),
            };
            let mut child = match spawned {
                Ok(child) => child,
                Err(error) => {
                    fail_captured_startup!(
                        format!("failed to spawn {failed_spawn_label}: {error}"),
                        Some(Ok(()))
                    );
                    return;
                }
            };
            // Only the later writer-installation failure still needs this
            // flag. A pseudo-console's writer is already owned; all later failure
            // paths supply capture cleanup explicitly in fail_captured_startup.
            if early_writer.is_none() {
                child_spawned = true;
            }
            let target_pid = child.process_id();
            // Adopt the session before anything else can observe the pane, so
            // every later stop goes through the session, never a bare pid.
            let adopted = target_pid
                .ok_or_else(|| anyhow::anyhow!("terminal target has no process id"))
                .and_then(|pid| {
                    session_control::SessionControl::adopt(
                        &host,
                        session_preparation,
                        token,
                        pid,
                        &pty_pair.master,
                    )
                });
            match adopted {
                Ok(control) => {
                    *execution_for_task
                        .session
                        .lock()
                        .expect("terminal session lock") = Some(Arc::new(control));
                }
                Err(error) => {
                    // The leader exited before it could be adopted: what it left in its
                    // session (a child holding the terminal, say) must not outlive the pane.
                    // Before the leader is reaped, so the session's id cannot be reused yet.
                    if let Some(pid) = target_pid
                        && let Err(sweep) = session_control::sweep_unadopted(pid)
                    {
                        execution_for_task.record_cleanup_error(format!(
                            "unadopted session sweep failed: {sweep}"
                        ));
                    }
                    let reason = cleanup_failed_startup_child_acknowledged(
                        &host,
                        child.take(),
                        format!("terminal session adoption failed: {error:#}"),
                        &startup_cleanup_tx,
                    )
                    .await;
                    fail_captured_startup!(reason, startup_cleanup_tx.borrow().clone());
                    return;
                }
            }
            if let Some(resume) = &application_resume {
                let result = target_pid
                    .ok_or_else(|| "terminal target has no acknowledged PID".to_owned())
                    .and_then(|pid| resume.bind(pid));
                if let Err(error) = result {
                    // Kill the adopted session as a whole, not just its leader.
                    if let Err(cleanup) = execution_for_task.release_session() {
                        execution_for_task.record_cleanup_error(format!(
                            "startup session cleanup failed: {cleanup:#}"
                        ));
                    }
                    let reason = cleanup_failed_startup_child_acknowledged(
                        &host,
                        child.take(),
                        format!("application resurrection bind failed: {error}"),
                        &startup_cleanup_tx,
                    )
                    .await;
                    resume.close();
                    fail_captured_startup!(reason, startup_cleanup_tx.borrow().clone());
                    return;
                }
            }
            let start_identity = startup_identity_or_stop(
                async {
                    match target_pid {
                        Some(pid) => bounded_process_start_identity(pid).await,
                        None => None,
                    }
                },
                &mut stop_rx,
            )
            .await;
            let Ok(start_identity) = start_identity else {
                // Kill the adopted session as a whole, not just its leader.
                if let Err(cleanup) = execution_for_task.release_session() {
                    execution_for_task.record_cleanup_error(format!(
                        "startup session cleanup failed: {cleanup:#}"
                    ));
                }
                let reason = cleanup_failed_startup_child_acknowledged(
                    &host,
                    child.take(),
                    "pane startup cancelled".into(),
                    &startup_cleanup_tx,
                )
                .await;
                if let Some(resume) = &application_resume {
                    resume.close();
                }
                fail_captured_startup!(reason, startup_cleanup_tx.borrow().clone());
                return;
            };
            if let Ok(mut identity) = process_start_identity_for_task.lock() {
                *identity = start_identity;
            }
            if let Ok(mut pid) = process_id_for_task.lock() {
                *pid = target_pid;
            }
            if let Ok(mut pgid) = process_group_id_for_task.lock() {
                // The leader leads its own session and therefore its own group.
                *pgid = target_pid.and_then(|pid| i32::try_from(pid).ok());
            }
            let mut child_killer = child.clone_killer();
            drop(pty_pair.slave);

            let master = pty_pair.master;

            let writer = match early_writer.map_or_else(
                || {
                    master.take_writer().map(|writer| {
                        Arc::new(switchy_thread::sync::Mutex::new(
                            Box::new(writer) as Box<dyn std::io::Write + Send>
                        ))
                    })
                },
                Ok,
            ) {
                Ok(writer) => writer,
                Err(error) => {
                    // Kill the adopted session as a whole, not just its leader.
                    if let Err(cleanup) = execution_for_task.release_session() {
                        execution_for_task.record_cleanup_error(format!(
                            "startup session cleanup failed: {cleanup:#}"
                        ));
                    }
                    let reason = cleanup_failed_startup_child_acknowledged(
                        &host,
                        child.take(),
                        format!("failed to open PTY writer: {error}"),
                        &startup_cleanup_tx,
                    )
                    .await;
                    if let Some(writer) = &capture_writer {
                        writer.fail("PTY writer installation failed");
                    }
                    drop(reader);
                    drop(master);
                    let previous = startup_cleanup_tx.borrow().clone();
                    let reason = startup_capture::acknowledge_cleanup(
                        &host,
                        capture_owner.take(),
                        &cleanup_tx,
                        previous,
                        reason,
                    )
                    .await;
                    fail_startup!(reason);
                    return;
                }
            };
            *execution_for_task
                .master
                .lock()
                .expect("new execution master lock") = Some(master);

            if launch.is_none()
                && let Some(command_text) = replay_command.as_deref()
            {
                if replay_on_prompt {
                    if let Ok(mut pending) = pending_prompt_replay.lock() {
                        *pending = Some(command_text.to_string());
                    }
                } else if let Ok(mut writer_guard) = writer.lock() {
                    let mut replay_bytes = command_text.as_bytes().to_vec();
                    replay_bytes.push(b'\n');
                    if writer_guard.write_all(&replay_bytes).is_ok() {
                        let _ = writer_guard.flush();
                    }
                }
            }

            let (child_exit_tx, mut child_exit_rx) =
                mpsc::unbounded_channel::<ExecutionExitNotice>();
            let exited_for_waiter = Arc::clone(&exited_for_task);
            let exit_reason_for_waiter = Arc::clone(&exit_reason_for_task);
            let output_buffer_for_waiter = Arc::clone(&output_buffer_for_reader);
            let terminal_grid_for_waiter = Arc::clone(&terminal_grid_for_reader);
            let output_dirty_for_waiter = Arc::clone(&output_dirty_for_reader);
            let resurrection_state_for_waiter = Arc::clone(&resurrection_state_for_waiter_seed);
            let child_waiter_done = Arc::new(AtomicBool::new(false));
            let child_waiter_done_for_thread = Arc::clone(&child_waiter_done);
            let publication_for_waiter = Arc::clone(&publication_for_task);
            // The waiter's work, held here until its thread takes it: when that thread cannot
            // start, the startup failure below still runs it, so the leader's exit is observed,
            // reaped and announced rather than lost with the closure.
            let waiter_work: PaneWaiterWork = Arc::new(Mutex::new(Some(Box::new({
                let host = host.clone();
                move || {
                    let _thread_guard = PaneBlockingThreadGuard::new(pane_id, "waiter");
                    let _completion = SupervisionCompletion {
                        done: Arc::clone(&child_waiter_done_for_thread),
                        execution: Arc::clone(&execution_for_waiter),
                    };
                    // Observe the exit without reaping, fence the leader's pid,
                    // then reap: from here on nothing signals that pid again.
                    if let Some(pid) = child.process_id() {
                        crate::platform::wait_for_exit_without_reaping(pid);
                    }
                    execution_for_waiter.mark_leader_exited();
                    let wait_result = child.wait();
                    if wait_result.is_ok() {
                        drop(child.take());
                    }
                    if let Ok(mut resurrection) = resurrection_state_for_waiter.lock() {
                        resurrection.close_launch();
                        if !resurrection.frozen {
                            resurrection.active_command = None;
                            resurrection.active_command_source = None;
                        }
                    }
                    execution_for_waiter.record_exit(&wait_result);
                    execution_for_waiter.settle_after_leader_exit(
                        program_execution || host.sessions().sweep_on_exit(),
                    );
                    exited_for_waiter.store(true, Ordering::SeqCst);
                    if let Ok(mut reason) = exit_reason_for_waiter.lock()
                        && reason.is_none()
                    {
                        *reason = Some(match wait_result {
                            Ok(status) => format_pane_exit_reason(&status),
                            Err(error) => format!("process wait failed: {error}"),
                        });
                    }
                    if !publication_for_waiter.wait() {
                        let _ = child_exit_tx.send(ExecutionExitNotice::OwnerExited);
                        child_waiter_done_for_thread.store(true, Ordering::SeqCst);
                        execution_for_waiter.notify();
                        return;
                    }
                    if !program_execution {
                        push_pane_runtime_notice(
                            &host,
                            session_id,
                            pane_id,
                            &output_buffer_for_waiter,
                            &terminal_grid_for_waiter,
                            &output_dirty_for_waiter,
                            "\r\n[bmux] pane process exited; layout preserved. Use restart pane or close pane.\r\n",
                        );
                    }
                    if !program_execution {
                        let _ = pane_exit_tx.send(PaneExitEvent {
                            session_id,
                            pane_id,
                            exited: Arc::clone(&exited_for_waiter),
                        });
                    }
                    let _ = child_exit_tx.send(ExecutionExitNotice::OwnerExited);
                    child_waiter_done_for_thread.store(true, Ordering::SeqCst);
                    execution_for_waiter.notify();
                }
            }))));
            let waiter_work_for_thread = Arc::clone(&waiter_work);
            let child_waiter = switchy_thread::thread::Builder::new()
                .name(format!("bmux-server-pane-{pane_id}-wait"))
                .spawn(move || {
                    let work = waiter_work_for_thread
                        .lock()
                        .ok()
                        .and_then(|mut work| work.take());
                    if let Some(work) = work {
                        work();
                    }
                })
                .ok();
            if child_waiter.is_none() {
                warn!(%pane_id, "failed spawning pane process waiter thread");
            }

            let reader_output = Arc::clone(&output_buffer_for_reader);
            let writer_for_reader = Arc::clone(&writer);
            let reader_thread_done = Arc::new(AtomicBool::new(false));
            let reader_thread_done_for_thread = Arc::clone(&reader_thread_done);
            let publication_for_reader = Arc::clone(&publication_for_task);
            // Fence checkpoint writers before terminating the shell. Its final
            // prompt/output can otherwise reset the saved invocation after the
            // server has finalized the restoration snapshot.
            let resurrection_state_for_shutdown = Arc::clone(&resurrection_state_for_reader);
            let reader_thread = switchy_thread::thread::Builder::new()
                .name(format!("bmux-server-pane-{pane_id}"))
                .spawn({ let host = host.clone(); move || {
                    let _thread_guard = PaneBlockingThreadGuard::new(pane_id, "reader");
                    let _completion = SupervisionCompletion { done: Arc::clone(&reader_thread_done_for_thread), execution: Arc::clone(&execution_for_reader) };
                    if !publication_for_reader.wait() {
                        reader_thread_done_for_thread.store(true, Ordering::SeqCst);
                        execution_for_reader.notify();
                        return;
                    }
                    tracing::debug!(pane_id = %pane_id, "execution reader publication released");
                    let mut buffer = [0_u8; 8192];
                    let mut conpty_projection =
                        console_query.then(console_startup::PrefixQuery::default);
                    let mut protocol_engine = TerminalProtocolEngine::new(protocol_profile)
                        .with_bracketed_paste_support(bracketed_paste_supported);
                    let mut capture_handshake = crate::capture_handshake::CaptureHandshake::default();
                    let mut reader_state = PaneReader::new(
                        protocol_profile,
                        Some(PaneShellMetadataParser {
                            token: Some(shell_marker_token),
                            ..PaneShellMetadataParser::default()
                        }),
                    )
                    .with_filters(pane_output_filters(host.states()));

                    // The image string in progress (Sixel, Kitty, iTerm2), which the grid's parse
                    // recognises and streams here, kept from read to read. Feature-gated to
                    // compile away when no image protocols are enabled.
                    #[cfg(feature = "image-registry")]
                    let mut image_collector =
                        bmux_image::ImageCollector::with_max_encoded_bytes(
                            usize::try_from(image_config.max_image_bytes.saturating_mul(2))
                                .unwrap_or(usize::MAX),
                        );

                    loop {
                        #[cfg(test)]
                        if execution_for_reader.inject_reader_failure.load(Ordering::SeqCst) {
                            execution_for_reader.record_error("injected PTY reader failure".into());
                            break;
                        }
                        match reader.read(&mut buffer) {
                            Ok(0) => {
                                if let (Ok(Some(writer)), Some(ticket)) = (execution_for_reader.writer(), reader.publication_ticket())
                                    && let Err(error) = writer.projected(ticket) {
                                    execution_for_reader.fail_capture(&error.to_string());
                                    break;
                                }
                                execution_for_reader.output_drained.store(true, Ordering::SeqCst);
                        execution_for_reader.notify();
                                break;
                            }
                            Err(error) => {
                                // Unix PTY masters report EIO when the last slave closes.
                                if error.raw_os_error() == Some(5) {
                                    execution_for_reader.output_drained.store(true, Ordering::SeqCst);
                        execution_for_reader.notify();
                                } else {
                                    execution_for_reader.record_error(format!("PTY read failed: {error}"));
                                }
                                break;
                            }
                            Ok(bytes_read) => {
                                // Serialize the watermark with geometry and the
                                // entire retained parser publication, not merely read.
                                let Ok(requested_size) = last_requested_size_for_reader.lock() else { break; };
                                let publication_ticket = reader.publication_ticket();
                                // The raw bootstrap responder already answered this
                                // execution's leading transport query. Keep it in
                                // durable capture, but never replay its reply after
                                // publication or interpret it as application input.
                                let projected = conpty_projection
                                    .as_mut()
                                    .map(|projection| projection.project(&buffer[..bytes_read]).0);
                                let protocol_chunk =
                                    projected.as_deref().unwrap_or(&buffer[..bytes_read]);
                                let (capture_output, capture_reply) = capture_handshake.process_with_fence(&host, protocol_chunk, pane_id, || {
                                    let (sent, received) = switchy_thread::sync::mpsc::sync_channel(1);
                                    fence_input_for_reader.send(PaneRuntimeCommand::InputFence(sent)).is_ok()
                                        && received.recv_timeout(Duration::from_secs(2)).is_ok()
                                });
                                if !capture_reply.is_empty() {
                                    if let Ok(mut writer) = writer_for_reader.lock() {
                                        if writer.write_all(&capture_reply).is_err() || writer.flush().is_err() { break; }
                                    } else { break; }
                                }
                                let chunk = capture_output.as_slice();
                                host.record_lazily(
                                    RecordingEventKind::PaneOutputRaw,
                                    RecordMeta { session_id: Some(session_id.0), pane_id: Some(pane_id), client_id: None },
                                    || RecordingPayload::Bytes { data: chunk.to_vec() },
                                );
                                // The size lock (held through the chunk) keeps resizes out
                                // until image state and the grid have consumed this chunk.
                                // With image support the grid's parse also recognises image
                                // sequences, placed in order with the rest of the output.
                                let mut reply = Vec::new();
                                #[cfg(feature = "image-registry")]
                                let Ok(mut registry) = image_registry_for_reader.lock() else { break; };
                                #[cfg(feature = "image-registry")]
                                let images_before = registry.sequence();
                                // All combined captures and writes acquire grid before output.
                                // The grid guard is held from the parse through the push, so a
                                // snapshot (grid, then output) never pairs parsed state with
                                // another chunk's offset; the output guard is taken only for
                                // the push. Replies are only collected: PTY writes happen
                                // after the capture locks release.
                                let Ok(mut grid) = terminal_grid_for_reader.lock() else { break; };
                                #[cfg(feature = "image-registry")]
                                let mut images = {
                                    let (cpw, cph) =
                                        cell_pixel_size_for_reader.lock().map_or((8, 16), |s| *s);
                                    ChunkImages {
                                        registry: &mut registry,
                                        collector: &mut image_collector,
                                        cell_pixels: (
                                            if cpw == 0 { 8 } else { cpw },
                                            if cph == 0 { 16 } else { cph },
                                        ),
                                        events: Vec::new(),
                                    }
                                };
                                #[cfg(feature = "image-registry")]
                                if let Err(error) = follow_chunk(
                                    &mut reader_state,
                                    &mut grid,
                                    &mut protocol_engine,
                                    &mut images,
                                    chunk,
                                    &mut reply,
                                ) {
                                    warn!(%pane_id, %error, "image projection failed; stopping output reader");
                                    break;
                                }
                                #[cfg(not(feature = "image-registry"))]
                                follow_chunk(
                                    &mut reader_state,
                                    &mut grid,
                                    &mut protocol_engine,
                                    chunk,
                                    &mut reply,
                                );
                                #[cfg(feature = "image-registry")]
                                let image_events = std::mem::take(&mut images.events);
                                let chunk = reader_state.visible.as_slice();
                                let markers = note_replay_markers_read(chunk);
                                if !markers.is_empty()
                                    && let Ok(mut replicas) = frame_replicas_for_reader.lock()
                                {
                                    for replica in replicas.values_mut() {
                                        replica.replay_markers.extend_from_slice(&markers);
                                    }
                                }

                                // Publish the pane's modes, as the grid now holds them, BEFORE
                                // making the chunk visible in the output buffer, so per-pane mode
                                // snapshots are always consistent with or ahead of the buffered data.
                                let modes = grid.grid().protocol_state();
                                if let Ok(mut protocol) = mouse_protocol_state_for_reader.lock() {
                                    *protocol = attach_mouse_protocol(modes);
                                }
                                if let Ok(mut mode_state) = input_mode_state_for_reader.lock() {
                                    *mode_state = attach_input_modes(modes);
                                }
                                let Ok(mut output) = reader_output.lock() else { break; };
                                output.push_parsed(chunk, ChunkBoundary::of(&grid));
                                drop(output);
                                drop(grid);
                                reply.extend_from_slice(&reader_state.pointer_replies);
                                #[cfg(feature = "image-registry")]
                                {
                                    if registry.sequence() != images_before
                                        && image_dirty_for_reader
                                            .compare_exchange(
                                                false,
                                                true,
                                                Ordering::SeqCst,
                                                Ordering::SeqCst,
                                            )
                                            .is_ok()
                                    {
                                        host.publish_pane_event(PaneEvent::ImageAvailable {
                                            session_id: session_id.0,
                                            pane_id,
                                        });
                                    }
                                    for event in &image_events {
                                        host.record(
                                            RecordingEventKind::PaneImage,
                                            image_event_to_recording_payload(event),
                                            RecordMeta {
                                                session_id: Some(session_id.0),
                                                pane_id: Some(pane_id),
                                                client_id: None,
                                            },
                                        );
                                    }
                                    drop(registry);
                                }
                                let (reports, metadata_events): (Vec<_>, Vec<_>) =
                                    std::mem::take(&mut reader_state.metadata_events)
                                        .into_iter()
                                        .partition(|event| matches!(event, PaneShellMetadataEvent::AgentReport { .. }));
                                execution_for_reader.output_watermark.fetch_add(
                                    u64::try_from(bytes_read).unwrap_or(u64::MAX), Ordering::SeqCst,
                                );
                                if let (Ok(Some(writer)), Some(ticket)) = (execution_for_reader.writer(), publication_ticket)
                                    && let Err(error) = writer.projected(ticket) {
                                    execution_for_reader.fail_capture(&error.to_string());
                                    break;
                                }
                                if execution_for_reader.commit_projection().is_err() { break; }
                                execution_for_reader.notify();
                                drop(requested_size);
                                for report in reports {
                                    if let PaneShellMetadataEvent::AgentReport { state, agent, message } = report {
                                        if let Ok(mut retained) = signals_for_reader.lock() {
                                            retained.report(&state, agent.as_deref(), message.as_deref());
                                        }
                                        host.publish_terminal_event(
                                            pane_runtime_terminal_events::TerminalEvent::AgentReport {
                                                session_id: session_id.0,
                                                pane_id,
                                                state,
                                                agent,
                                                message,
                                            },
                                        );
                                    }
                                }
                                // Metadata callbacks can acquire the runtime manager. Never
                                // invoke them while holding image/grid/output capture locks.
                                if !metadata_events.is_empty() {
                                    let padding_before = pane_padding_state(&host, session_id, pane_id);
                                    let mut replay_command = None;
                                    if let Ok(mut resurrection_state) =
                                        resurrection_state_for_reader.lock()
                                    {
                                        if let Some(resume) = resurrection_state.launch.clone()
                                            && !resurrection_state.frozen
                                        {
                                            for event in &metadata_events {
                                                if matches!(event, PaneShellMetadataEvent::NestedReplay) { continue; }
                                                match resume.boundary(matches!(event, PaneShellMetadataEvent::Prompt { .. })) {
                                                    Ok(identity) => resurrection_state.execution_token = Some(identity),
                                                    Err(error) => {
                                                        warn!(%error, "resurrection lifecycle update failed");
                                                        resurrection_state.close_launch();
                                                    }
                                                }
                                            }
                                        }
                                        if let Ok(mut pending_replay) =
                                            pending_prompt_replay_for_reader.lock()
                                        {
                                            replay_command =
                                                apply_shell_metadata_events_and_take_prompt_replay(
                                                    &mut resurrection_state,
                                                    &mut pending_replay,
                                                    metadata_events,
                                                );
                                        } else {
                                            for event in metadata_events {
                                                resurrection_state.apply_event(event);
                                            }
                                        }
                                    }
                                    if let Some(command_text) = replay_command
                                        && let Ok(mut writer) = writer_for_reader.lock()
                                    {
                                        let mut replay_bytes = command_text.as_bytes().to_vec();
                                        replay_bytes.push(b'\n');
                                        if writer.write_all(&replay_bytes).is_ok() {
                                            let _ = writer.flush();
                                        }
                                    }
                                    host.mark_snapshot_dirty();
                                    if let Some(before) = padding_before {
                                        notify_pane_metadata_changed(&host, session_id, pane_id, before);
                                    }
                                }
                                // Notify streaming clients that new output is available.
                                // Only emit when transitioning false→true to coalesce
                                // thousands of per-chunk writes into ~1 event per fetch cycle.
                                if output_dirty_for_reader
                                    .compare_exchange(
                                        false,
                                        true,
                                        Ordering::SeqCst,
                                        Ordering::SeqCst,
                                    )
                                    .is_ok()
                                {
                                    host.publish_pane_event(PaneEvent::OutputAvailable {
                                        session_id: session_id.0,
                                        pane_id,
                                    });
                                    crate::handlers::output_push::pane_output_available(&host,
                                        session_id.0,
                                        pane_id,
                                    );
                                }
                                for signal in reader_state.signals.drain(..) {
                                    if let PaneSignal::ClipboardWrite { selection, text } = signal {
                                        // Only for the clients viewing the pane, in their frames.
                                        if let Ok(mut replicas) = frame_replicas_for_reader.lock() {
                                            for replica in replicas.values_mut() {
                                                replica
                                                    .clipboard_writes
                                                    .push((selection.clone(), text.clone()));
                                            }
                                        }
                                        continue;
                                    }
                                    // Retain before publishing, so anyone woken by
                                    // the event reads the new value.
                                    if let Ok(mut retained) = signals_for_reader.lock() {
                                        retained.apply(&signal);
                                    }
                                    host.publish_terminal_event(
                                        signal.into_event(session_id.0, pane_id),
                                    );
                                }
                                if !reply.is_empty() {
                                    host.record(
                                        RecordingEventKind::ProtocolReplyRaw,
                                        RecordingPayload::Bytes {
                                            data: reply.clone(),
                                        },
                                        RecordMeta {
                                            session_id: Some(session_id.0),
                                            pane_id: Some(pane_id),
                                            client_id: None,
                                        },
                                    );
                                    if let Ok(mut writer) = writer_for_reader.lock() {
                                        if writer.write_all(&reply).is_err() {
                                            break;
                                        }
                                        let _ = writer.flush();
                                    } else {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    if !execution_for_reader.output_drained.load(Ordering::SeqCst) {
                        execution_for_reader.fail_capture("PTY reader stopped before EOF");
                    }
                    reader_thread_done_for_thread.store(true, Ordering::SeqCst);
                        execution_for_reader.notify();
                } })
                .ok();
            if reader_thread.is_none() {
                reader_thread_done.store(true, Ordering::SeqCst);
                warn!(%pane_id, "failed spawning pane PTY reader thread");
            }

            let startup_ready = child_waiter.is_some() && reader_thread.is_some();
            if startup_ready {
                startup_tx.send_replace(Some(Ok(())));
            } else {
                startup_tx.send_replace(Some(Err(
                    "failed to start pane I/O supervision threads".into()
                )));
                // Why the pane ended, before the waiter records how its leader did (killed).
                if let Ok(mut reason) = exit_reason_for_task.lock()
                    && reason.is_none()
                {
                    *reason = Some("pane I/O threads could not be started".into());
                }
                if let Err(error) =
                    execution_for_task.signal_owned_group(&host, crate::platform::StopAction::Force)
                {
                    execution_for_task.record_cleanup_error(format!(
                        "startup-thread group cleanup failed: {error}"
                    ));
                }
                kill_unreaped_leader(&execution_for_task, &mut child_killer);
                // A waiter that never started leaves its work here: observe, reap and announce
                // the leader's exit on the blocking pool, as its thread would have.
                let orphaned = waiter_work.lock().ok().and_then(|mut work| work.take());
                if let Some(work) = orphaned
                    && let Err(error) = switchy_async::task::spawn_blocking(work).await
                {
                    execution_for_task
                        .record_cleanup_error(format!("pane waiter fallback failed: {error}"));
                }
            }

            let mut owner_exited = false;
            let mut execution_changes = execution_for_task.subscribe();
            execution_for_task.notify();
            // Stop the pane: end everything in its session, release its lifetime, and reap
            // its leader.
            macro_rules! stop_pane {
                () => {{
                    let resurrection = Arc::clone(&resurrection_state_for_shutdown);
                    let execution = Arc::clone(&execution_for_task);
                    // Checkpoint/service IO and helper cleanup must not block
                    // executor workers, otherwise multipane shutdown is serial
                    // even when every pane received stop concurrently.
                    // `inject_yields` cannot see inside a macro: yield here by hand.
                    let stopped =
                        switchy_async::task::yield_after(switchy_async::task::spawn_blocking({
                            let host = host.clone();
                            move || {
                                let frozen = resurrection
                                    .lock()
                                    .map_err(|_| anyhow::anyhow!("resurrection lock poisoned"))
                                    .and_then(|mut state| state.freeze());
                                if let Err(error) = frozen {
                                    execution.record_cleanup_error(format!(
                                        "checkpoint freeze failed: {error}"
                                    ));
                                }
                                if let Err(error) = execution
                                    .signal_owned_group(&host, crate::platform::StopAction::Force)
                                {
                                    execution.record_cleanup_error(format!(
                                        "group stop failed: {error}"
                                    ));
                                }
                                let lifetime = PaneLifetime {
                                    execution,
                                    resurrection,
                                };
                                lifetime.revoke();
                            }
                        }))
                        .await;
                    if let Err(error) = stopped {
                        execution_for_task
                            .record_cleanup_error(format!("pane stop worker failed: {error}"));
                    }
                    kill_unreaped_leader(&execution_for_task, &mut child_killer);
                }};
            }
            loop {
                if !startup_ready {
                    break;
                }
                switchy_async::select! {
                    changed = execution_changes.changed() => {
                        if changed.is_err() {
                            execution_for_task.record_error("execution lifecycle publisher disconnected".into());
                            break;
                        }
                        if execution_for_task.terminal_owner_retired.load(Ordering::SeqCst) {
                            break;
                        }
                        if owner_exited && reader_thread_done.load(Ordering::SeqCst) {
                            // The pane is being stopped (closed, or refused before its
                            // commit, which also ends its reader): what its leader left
                            // running ends with it, rather than being released.
                            if startup_stop_requested(&mut stop_rx) {
                                stop_pane!();
                                break;
                            }
                            // Members deliberately left running after the
                            // leader exited no longer hold the terminal.
                            execution_for_task.retire_released_session();
                            break;
                        }
                        if execution_for_task.capture_failure().is_some()
                            || (reader_thread_done.load(Ordering::SeqCst) && !execution_for_task.output_drained.load(Ordering::SeqCst)) {
                            let lifetime = PaneLifetime {
                                execution: Arc::clone(&execution_for_task),
                                resurrection: Arc::clone(&resurrection_state_for_shutdown),
                            };
                            // `inject_yields` cannot see inside `select!`: yield here by hand.
                            let cleanup = switchy_async::task::yield_after(switchy_async::task::spawn_blocking({ let host = host.clone(); move || {
                                if let Err(error) = lifetime.execution.signal_owned_group(&host, crate::platform::StopAction::Force) {
                                    lifetime.execution.record_cleanup_error(format!("reader-failure group cleanup failed: {error}"));
                                }
                                lifetime.revoke();
                            } })).await;
                            if let Err(error) = cleanup {
                                execution_for_task.record_cleanup_error(format!("reader cleanup worker failed: {error}"));
                            }
                            kill_unreaped_leader(&execution_for_task, &mut child_killer);
                            break;
                        }
                    }
                    _ = &mut stop_rx => {
                        stop_pane!();
                        break;
                    }
                    // Once the owner exited its waiter is gone: the closed channel says nothing more.
                    child_exit = child_exit_rx.recv(), if !owner_exited => {
                        match child_exit {
                            Some(ExecutionExitNotice::OwnerExited) => {
                                if execution_for_task.terminal_owner_retired.load(Ordering::SeqCst) {
                                    break;
                                }
                                // Members were left running; keep serving the
                                // terminal until they release it or the pane stops.
                                owner_exited = true;
                            }
                            None => break,
                        }
                    }
                    input = input_rx.recv() => {
                        let (input, _permit) = match input {
                            Some(PaneRuntimeCommand::GuardedResize { rows, cols, _permit: permit }) =>
                                (Some(PaneRuntimeCommand::Resize { rows, cols }), Some(permit)),
                            other => (other, None),
                        };
                        match input {
                            Some(PaneRuntimeCommand::AdmittedInput(bytes, acknowledgement, _permit, authority)) => {
                                let result = if execution_for_task.terminal_session_ended.load(Ordering::SeqCst)
                                    || execution_for_task.terminal_owner_retired.load(Ordering::SeqCst)
                                {
                                    Err("PTY input target retired".to_string())
                                } else {
                                    writer.lock().map_err(|_| "PTY writer lock poisoned".to_string()).and_then(|mut writer| authority.write(&mut *writer, &bytes).map_err(|error| format!("PTY input write failed ({error}); do not replay")))
                                };
                                let _ = acknowledgement.send(result);
                            }
                            Some(PaneRuntimeCommand::AuthorizedInput(bytes, authority)) => {
                                if !execution_for_task.terminal_session_ended.load(Ordering::SeqCst)
                                    && !execution_for_task.terminal_owner_retired.load(Ordering::SeqCst)
                                    && let Ok(mut writer) = writer.lock()
                                {
                                    let _ = authority.write(&mut *writer, &bytes);
                                }
                            }
                            Some(PaneRuntimeCommand::InputFence(ack)) => {
                                // This task owns FIFO writes. The mutex also serializes
                                // concurrent protocol replies from the output reader.
                                if let Ok(_writer) = writer.lock() { let _ = ack.send(()); }
                            }
                            Some(PaneRuntimeCommand::Input(bytes)) => {
                                if let Ok(mut writer) = writer.lock()
                                    && writer.write_all(&bytes).is_ok()
                                {
                                    let _ = writer.flush();
                                } else {
                                    execution_for_task.record_error("PTY input write failed".into());
                                    // Sweep while the live PTY/session still
                                    // supplies group authority; killing its leader
                                    // first can strand descendants on direct paths.
                                    if let Err(error) = execution_for_task.signal_owned_group(&host, crate::platform::StopAction::Force) {
                                        execution_for_task.record_cleanup_error(format!("input-failure group cleanup failed: {error}"));
                                    }
                                    kill_unreaped_leader(&execution_for_task, &mut child_killer);
                                    break;
                                }
                            }
                            Some(PaneRuntimeCommand::Resize { rows, cols }) => {
                                // Serialize with synchronous guarded sizing. A queued
                                // legacy request superseded by newer published geometry
                                // must not restore stale PTY dimensions after its ack.
                                if let Err(error) = execution_for_task.resize_if_current(&last_requested_size_for_input, rows, cols) {
                                    execution_for_task.record_error(format!("PTY resize failed: {error}"));
                                }
                            }
                            Some(PaneRuntimeCommand::GuardedResize { .. }) => unreachable!("guarded command normalized"),
                            None => break,
                        }
                    }
                }
            }

            // A forced stop ends the job at once, but the owner retires only
            // after the child waiter reaps the leader. Wait for that before
            // deciding about the console below: ConPTY output ends only at
            // ClosePseudoConsole, so keeping the console here would leave the
            // reader, and the supervision join after it, waiting forever.
            let console_owns_output = bmux_pty::behavior().output_ends_with_console;
            while console_owns_output
                && execution_for_task.group_kill_issued.load(Ordering::SeqCst)
                && !execution_for_task
                    .terminal_owner_retired
                    .load(Ordering::SeqCst)
                && !execution_for_task.cleanup_failed.load(Ordering::SeqCst)
                && !reader_thread_done.load(Ordering::SeqCst)
            {
                if execution_changes.changed().await.is_err() {
                    break;
                }
            }
            // ConPTY owns its output pipe until ClosePseudoConsole, even after
            // every hosted process has exited. Once the authenticated terminal
            // owner has retired, close the console concurrently with the live
            // raw/projection readers: ClosePseudoConsole itself may wait for
            // their drain. Never infer EOF or an exit outcome from this close.
            if console_owns_output
                && execution_for_task
                    .terminal_owner_retired
                    .load(Ordering::SeqCst)
            {
                let master = execution_for_task
                    .master
                    .lock()
                    .expect("master lock")
                    .take();
                if let Err(error) = console_startup::close_retired_console(master).await {
                    execution_for_task
                        .record_cleanup_error(format!("retired console close failed: {error}"));
                }
            }
            // A program's exit is not EOF: preserve the master while the
            // reader drains buffered bytes and descendants still hold the slave.
            if program_execution && startup_ready {
                while !reader_thread_done.load(Ordering::SeqCst) {
                    if !execution_for_task.output_drained.load(Ordering::SeqCst)
                        && execution_for_task
                            .outcome
                            .lock()
                            .is_ok_and(|outcome| outcome.2.is_some())
                    {
                        if let Err(error) = execution_for_task
                            .signal_owned_group(&host, crate::platform::StopAction::Force)
                        {
                            execution_for_task.record_cleanup_error(format!(
                                "failed-reader group cleanup failed: {error}"
                            ));
                        }
                        kill_unreaped_leader(&execution_for_task, &mut child_killer);
                    }
                    if !reader_thread_done.load(Ordering::SeqCst) {
                        let _ = execution_changes.changed().await;
                    }
                }
                if !execution_for_task.output_drained.load(Ordering::SeqCst) {
                    if let Err(error) = execution_for_task
                        .signal_owned_group(&host, crate::platform::StopAction::Force)
                    {
                        execution_for_task.record_cleanup_error(format!(
                            "reader-failure group cleanup failed: {error}"
                        ));
                    }
                    kill_unreaped_leader(&execution_for_task, &mut child_killer);
                }
            }
            drop(writer);
            // A stopped reader can never drain again. After authoritative
            // retirement, release its remaining master even when capture is
            // incomplete: Darwin session-leader exit waits in ttywait() for
            // unread output until the last master closes. Waiting for EOF here
            // would retain that master while joining the child that needs it
            // closed. This does not turn cancelled capture into observed EOF.
            // A live reader (including an escaped slave holder) keeps its master.
            if (execution_for_task.output_drained.load(Ordering::SeqCst)
                || reader_thread_done.load(Ordering::SeqCst))
                && (!program_execution
                    || execution_for_task
                        .terminal_owner_retired
                        .load(Ordering::SeqCst))
            {
                execution_for_task
                    .master
                    .lock()
                    .expect("master lock")
                    .take();
            }
            // Never occupy a blocking-pool worker with an unbounded join.
            // A timeout is an explicit failure, not acknowledged restoration.
            if !execution_for_task.output_drained.load(Ordering::SeqCst)
                && let Some(writer) = &capture_writer
            {
                writer.fail("projection stopped before real EOF");
            }
            let mut threads = vec![child_waiter, reader_thread];
            threads.extend(capture_owner.take().into_iter().map(Some));
            let mut join = supervision_join(&mut threads);
            // Shutdown observers own inactivity policy; elapsed time alone does
            // not make a retained cleanup join a failure.
            let cleanup_result = (&mut join)
                .await
                .map_err(|error| format!("pane supervision join failed: {error}"))
                .and_then(std::convert::identity);
            let cleanup_result = cleanup_result.and_then(|()| {
                if let Some(error) = execution_for_task.capture_failure() {
                    Err(format!("capture cleanup incomplete: {error}"))
                } else if let Some(error) = execution_for_task.cleanup_error() {
                    Err(error)
                } else {
                    Ok(())
                }
            });
            cleanup_tx.send_replace(Some(cleanup_result));

            exited_for_task.store(true, Ordering::SeqCst);
            execution_for_task.notify();
        });

        let padding_override = self
            .restored_padding_overrides
            .get(&(session_id, pane_meta.id))
            .copied();
        PaneRuntimeHandle {
            host: host.clone(),
            lifetime: PaneLifetime {
                execution: Arc::clone(&execution),
                resurrection: Arc::clone(&resurrection_state),
            },
            execution,
            publication,
            startup,
            cleanup,
            session_id,
            meta: pane_meta,
            process_start_identity,
            process_id,
            process_group_id,
            resurrection_state,
            exit_reason,
            stop_tx: Some(stop_tx),
            task,
            input_tx,
            output_buffer,
            terminal_grid,
            signals,
            frame_replicas,
            exited,
            last_requested_size,
            output_dirty,
            mouse_protocol_state,
            input_mode_state,
            padding_override,
            padding_preview: None,
            padding_override_persistence: PanePaddingOverridePersistence::Snapshot,
            #[cfg(feature = "image-registry")]
            image_registry,
            #[cfg(feature = "image-registry")]
            cell_pixel_size,
            #[cfg(feature = "image-registry")]
            image_dirty,
        }
    }

    fn prepare_split(
        &self,
        session_id: SessionId,
        target: Option<PaneSelector>,
        direction: PaneSplitDirection,
        options: Option<bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::InitialShellOptions>,
    ) -> Result<PreparedSplit> {
        validate_initial_shell_options(options.as_ref())?;
        let session = self
            .runtimes
            .get(&session_id)
            .ok_or_else(|| anyhow::anyhow!("session runtime unavailable"))?;
        let target =
            resolve_pane_id_from_selector(session, &target.unwrap_or(PaneSelector::Active))
                .ok_or_else(|| anyhow::anyhow!("split target unavailable"))?;
        let source = session
            .panes
            .get(&target)
            .ok_or_else(|| anyhow::anyhow!("split target unavailable"))?;
        let target_execution = source
            .resurrection_state
            .lock()
            .map_err(|_| anyhow::anyhow!("split target metadata poisoned"))?
            .observation_execution;
        let cleanup = PreparedCleanup::reserve(&self.host)?;
        let prefix = match direction {
            PaneSplitDirection::Vertical => "v",
            PaneSplitDirection::Horizontal => "h",
        };
        let (launch, permit) = oneshot::channel();
        let pane = self.spawn_pane_runtime_gated(
            session_id,
            PaneRuntimeMeta {
                id: crate::ids::new_uuid(),
                name: Some(format!("{prefix}-pane-{}", session.panes.len() + 1)),
                shell: source.meta.shell.clone(),
                launch: None,
                resurrection: PaneResurrectionSnapshot::default(),
            },
            (24, 80),
            false,
            options,
            Some(permit),
        );
        Ok(PreparedSplit {
            cleanup,
            pane: Some(pane),
            launch: Some(launch),
            target,
            target_execution,
        })
    }

    fn commit_split(
        &mut self,
        session_id: SessionId,
        prepared: &mut PreparedSplit,
        direction: PaneSplitDirection,
        ratio: f32,
    ) -> Result<Uuid> {
        let host = self.host.clone();
        let host = &host;
        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("split session disappeared"))?;
        let source = session
            .panes
            .get(&prepared.target)
            .ok_or_else(|| anyhow::anyhow!("split target disappeared"))?;
        anyhow::ensure!(
            !source.exited.load(Ordering::SeqCst),
            "split target execution exited"
        );
        anyhow::ensure!(
            source
                .resurrection_state
                .lock()
                .map_err(|_| anyhow::anyhow!("split target metadata poisoned"))?
                .observation_execution
                == prepared.target_execution,
            "split target execution changed"
        );
        let pane = prepared.pane.as_ref().expect("prepared split owns pane");
        anyhow::ensure!(
            matches!(&*pane.startup.borrow(), Some(Ok(()))) && !pane.exited.load(Ordering::SeqCst),
            "split startup unavailable"
        );
        let pane_id = pane.meta.id;
        let mut layout = session.layout_root.clone();
        if let Some(layout) = &mut layout {
            anyhow::ensure!(
                layout.replace_leaf_with_split(prepared.target, direction, ratio, pane_id),
                "split target layout changed"
            );
        } else {
            anyhow::bail!("split layout unavailable");
        }
        let pane = prepared.pane.take().expect("prepared split owns pane");
        for client in &session.attached_clients {
            anyhow::ensure!(
                crate::private_input::register_producer(host, pane_id, client.0),
                "input producer budget exhausted"
            );
            if let Ok(mut output) = pane.output_buffer.lock() {
                output.register_client_at_tail(*client);
            }
        }
        session.panes.insert(pane_id, pane);
        session.layout_root = layout;
        session.zoomed_pane_id = None;
        session.focused_pane_id = pane_id;
        self.register_pane_input(session_id, &self.runtimes[&session_id].panes[&pane_id]);
        self.runtimes[&session_id].panes[&pane_id]
            .publication
            .decide(true);
        self.apply_stored_attach_viewport(session_id);
        Ok(pane_id)
    }

    #[cfg(test)]
    fn split_pane(
        &mut self,
        session_id: SessionId,
        target: Option<PaneSelector>,
        direction: PaneSplitDirection,
    ) -> Result<Uuid> {
        self.create_split_pane(session_id, target, direction, None, None, None)
    }

    fn launch_pane(
        &mut self,
        session_id: SessionId,
        target: Option<PaneSelector>,
        direction: PaneSplitDirection,
        name: Option<String>,
        command: PaneLaunchCommand,
    ) -> Result<Uuid> {
        let launch = pane_launch_spec_from_command(command)?;
        self.create_split_pane(session_id, target, direction, name, Some(launch), None)
    }

    fn create_split_pane(
        &mut self,
        session_id: SessionId,
        target: Option<PaneSelector>,
        direction: PaneSplitDirection,
        name: Option<String>,
        launch: Option<PaneLaunchSpec>,
        initial_shell: Option<
            bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::InitialShellOptions,
        >,
    ) -> Result<Uuid> {
        let host = self.host.clone();
        let host = &host;
        validate_initial_shell_options(initial_shell.as_ref())?;
        let (target_pane_id, next_pane_name, shell, client_ids) = {
            let session = self
                .runtimes
                .get(&session_id)
                .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
            let target_pane_id =
                resolve_pane_id_from_selector(session, &target.unwrap_or(PaneSelector::Active))
                    .ok_or_else(|| anyhow::anyhow!("target pane not found"))?;
            let focused = session
                .panes
                .get(&target_pane_id)
                .ok_or_else(|| anyhow::anyhow!("target pane not found"))?;
            let name_prefix = match direction {
                PaneSplitDirection::Vertical => "v",
                PaneSplitDirection::Horizontal => "h",
            };
            (
                target_pane_id,
                name.or_else(|| Some(format!("{name_prefix}-pane-{}", session.panes.len() + 1))),
                focused.meta.shell.clone(),
                session.attached_clients.iter().copied().collect::<Vec<_>>(),
            )
        };

        let pane_id = crate::ids::new_uuid();
        // Prepare geometry before spawning so a stale target cannot create an
        // orphan execution or alter focus/zoom. The manager is exclusively held.
        let mut next_layout = self.runtimes[&session_id].layout_root.clone();
        if let Some(layout) = &mut next_layout {
            if !layout.replace_leaf_with_split(target_pane_id, direction, 0.5, pane_id) {
                anyhow::bail!("failed to apply split to layout tree");
            }
        } else {
            next_layout = Some(PaneLayoutNode::Leaf { pane_id });
        }
        let pane_meta = PaneRuntimeMeta {
            id: pane_id,
            name: next_pane_name,
            shell,
            launch,
            resurrection: PaneResurrectionSnapshot::default(),
        };
        let handle = self.spawn_pane_runtime_with_restore(
            session_id,
            pane_meta,
            (24, 80),
            false,
            initial_shell,
        );
        for client_id in client_ids {
            anyhow::ensure!(
                crate::private_input::register_producer(host, pane_id, client_id.0),
                "input producer budget exhausted"
            );
            if let Ok(mut output) = handle.output_buffer.lock() {
                output.register_client_at_tail(client_id);
            }
        }

        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        session.panes.insert(pane_id, handle);
        session.layout_root = next_layout;
        session.zoomed_pane_id = None;
        session.focused_pane_id = pane_id;
        self.register_pane_input(session_id, &self.runtimes[&session_id].panes[&pane_id]);
        self.apply_stored_attach_viewport(session_id);
        Ok(pane_id)
    }

    fn focus_pane(&mut self, session_id: SessionId, direction: PaneFocusDirection) -> Result<Uuid> {
        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        // If zoomed, stay zoomed but update to the new focused pane.
        let was_zoomed = session.zoomed_pane_id.is_some();
        let pane_ids = ordered_pane_ids(session);
        if pane_ids.is_empty() {
            anyhow::bail!("no panes in session runtime")
        }
        let current_index = pane_ids
            .iter()
            .position(|id| *id == session.focused_pane_id)
            .unwrap_or(0);
        let len = pane_ids.len();
        let next_index = match direction {
            PaneFocusDirection::Next => (current_index + 1) % len,
            PaneFocusDirection::Prev => {
                if current_index == 0 {
                    len - 1
                } else {
                    current_index - 1
                }
            }
        };
        session.focused_pane_id = pane_ids[next_index];
        if was_zoomed {
            session.zoomed_pane_id = Some(pane_ids[next_index]);
            self.apply_stored_attach_viewport(session_id);
        }
        Ok(self.runtimes[&session_id].focused_pane_id)
    }

    fn focus_pane_target(&mut self, session_id: SessionId, target: &PaneSelector) -> Result<Uuid> {
        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        // If zoomed, stay zoomed but update to the new focused pane.
        let was_zoomed = session.zoomed_pane_id.is_some();
        let pane_id = resolve_pane_id_from_selector(session, target)
            .ok_or_else(|| anyhow::anyhow!("target pane not found"))?;
        session.focused_pane_id = pane_id;
        if was_zoomed {
            session.zoomed_pane_id = Some(pane_id);
            self.apply_stored_attach_viewport(session_id);
        }
        Ok(pane_id)
    }

    #[allow(clippy::too_many_lines)] // Pane teardown atomically removes layout, floating dependents, inputs, scrollback pins, and preview targets.
    fn close_pane(
        &mut self,
        session_id: SessionId,
        target: Option<PaneSelector>,
    ) -> Result<(Uuid, Option<RemovedRuntime>)> {
        let host = self.host.clone();
        let host = &host;
        // Auto-unzoom on layout mutation.
        if let Some(session) = self.runtimes.get_mut(&session_id) {
            session.zoomed_pane_id = None;
        }
        let (pane_id, remove_runtime) = {
            let session = self
                .runtimes
                .get_mut(&session_id)
                .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
            let pane_id =
                resolve_pane_id_from_selector(session, &target.unwrap_or(PaneSelector::Active))
                    .ok_or_else(|| anyhow::anyhow!("target pane not found"))?;
            let remove_count = 1 + session
                .floating_surfaces
                .iter()
                .filter(|surface| {
                    surface.anchor_pane_id == Some(pane_id) && surface.pane_id != pane_id
                })
                .count();
            (pane_id, session.panes.len() <= remove_count)
        };

        if remove_runtime {
            anyhow::bail!("cannot close the final pane without an explicit session action");
        }

        let pane_input_index = Arc::clone(&self.pane_input_index);
        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        let next_focus = session.layout_root.as_ref().and_then(|layout_root| {
            next_focus_after_close(
                layout_root,
                session.focused_pane_id,
                pane_id,
                session.attach_viewport,
            )
        });
        let pane = session
            .panes
            .remove(&pane_id)
            .ok_or_else(|| anyhow::anyhow!("focused pane not found"))?;
        session
            .scrollback_pins
            .retain(|(_, pinned_pane_id), _| *pinned_pane_id != pane_id);
        if let Ok(mut index) = pane_input_index.write() {
            index.remove(&pane_id);
        }
        let anchored_floating_panes = session
            .floating_surfaces
            .iter()
            .filter(|surface| surface.anchor_pane_id == Some(pane_id))
            .map(|surface| surface.pane_id)
            .filter(|floating_pane_id| *floating_pane_id != pane_id)
            .collect::<Vec<_>>();
        let closed_pane_ids = std::iter::once(pane_id)
            .chain(anchored_floating_panes.iter().copied())
            .collect::<BTreeSet<_>>();
        Self::drop_padding_preview_targets(&mut self.padding_previews, &closed_pane_ids);
        session.floating_surfaces.retain(|surface| {
            surface.pane_id != pane_id && surface.anchor_pane_id != Some(pane_id)
        });
        for floating_pane_id in anchored_floating_panes {
            if let Some(floating_pane) = session.panes.remove(&floating_pane_id) {
                session
                    .scrollback_pins
                    .retain(|(_, pinned_pane_id), _| *pinned_pane_id != floating_pane_id);
                if let Ok(mut index) = pane_input_index.write() {
                    index.remove(&floating_pane_id);
                }
                let shutdown_host = host.clone();
                switchy_async::task::spawn(async move {
                    shutdown_pane_handle(&shutdown_host, floating_pane).await;
                });
            }
        }
        if let Some(layout_root) = &mut session.layout_root {
            let _ = layout_root.remove_leaf(pane_id);
        }
        let layout_ids = session
            .layout_root
            .as_ref()
            .map(|layout| {
                let mut ids = Vec::new();
                layout.pane_order(&mut ids);
                ids
            })
            .unwrap_or_default();
        if layout_ids.iter().any(|id| !session.panes.contains_key(id)) {
            session.layout_root = None;
        }
        if session.focused_pane_id == pane_id
            || !session.panes.contains_key(&session.focused_pane_id)
        {
            let next_focus = next_focus.or_else(|| ordered_pane_ids(session).first().copied());
            if let Some(next_id) = next_focus {
                session.focused_pane_id = next_id;
            }
        }

        let shutdown_host = host.clone();
        switchy_async::task::spawn(async move {
            shutdown_pane_handle(&shutdown_host, pane).await;
        });
        self.apply_stored_attach_viewport(session_id);
        Ok((pane_id, None))
    }

    fn restart_pane(
        &mut self,
        session_id: SessionId,
        target: Option<PaneSelector>,
    ) -> Result<Uuid> {
        let host = self.host.clone();
        let host = &host;
        let _admission = managed_idle::admit(host)?;
        reservations::check_authority(host)?;
        let (pane_meta, padding_override, padding_override_persistence) = {
            let session = self
                .runtimes
                .get(&session_id)
                .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
            let pane_id =
                resolve_pane_id_from_selector(session, &target.unwrap_or(PaneSelector::Active))
                    .ok_or_else(|| anyhow::anyhow!("target pane not found"))?;
            let pane = session
                .panes
                .get(&pane_id)
                .ok_or_else(|| anyhow::anyhow!("target pane not found"))?;
            let preserved_cwd = pane
                .resurrection_state
                .lock()
                .ok()
                .and_then(|state| state.last_known_cwd.clone());
            (
                PaneRuntimeMeta {
                    id: pane_id,
                    name: pane.meta.name.clone(),
                    shell: pane.meta.shell.clone(),
                    launch: pane.meta.launch.clone(),
                    resurrection: PaneResurrectionSnapshot {
                        execution_token: None,
                        active_command: None,
                        active_command_source: None,
                        last_known_cwd: preserved_cwd,
                    },
                },
                pane.padding_override,
                pane.padding_override_persistence,
            )
        };

        let old_pane = {
            let session = self
                .runtimes
                .get_mut(&session_id)
                .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
            session
                .panes
                .remove(&pane_meta.id)
                .ok_or_else(|| anyhow::anyhow!("target pane not found"))?
        };
        self.unregister_pane_input(pane_meta.id);
        let shutdown_host = host.clone();
        switchy_async::task::spawn(async move {
            shutdown_pane_handle(&shutdown_host, old_pane).await;
        });

        let mut new_pane = self.spawn_pane_runtime(session_id, pane_meta, (24, 80));
        new_pane.padding_override = padding_override;
        new_pane.padding_override_persistence = padding_override_persistence;
        let client_ids = {
            let session = self
                .runtimes
                .get(&session_id)
                .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
            session.attached_clients.iter().copied().collect::<Vec<_>>()
        };
        for client_id in client_ids {
            if let Ok(mut output) = new_pane.output_buffer.lock() {
                output.register_client_at_tail(client_id);
            }
        }
        if let Ok(mut reason) = new_pane.exit_reason.lock() {
            *reason = None;
        }

        let restarted_pane_id = new_pane.meta.id;
        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        session.panes.insert(restarted_pane_id, new_pane);
        session.focused_pane_id = restarted_pane_id;
        self.register_pane_input(
            session_id,
            &self.runtimes[&session_id].panes[&restarted_pane_id],
        );
        self.apply_stored_attach_viewport(session_id);
        Ok(restarted_pane_id)
    }

    fn resize_pane(
        &mut self,
        session_id: SessionId,
        target: Option<PaneSelector>,
        direction: PaneResizeDirection,
        cells: u16,
    ) -> Result<()> {
        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        // Auto-unzoom on layout mutation.
        session.zoomed_pane_id = None;
        let pane_id =
            resolve_pane_id_from_selector(session, &target.unwrap_or(PaneSelector::Active))
                .ok_or_else(|| anyhow::anyhow!("target pane not found"))?;
        let root = scene_root_from_viewport(session.attach_viewport);
        if let Some(layout_root) = &mut session.layout_root {
            let _ = layout_root.resize_focused(pane_id, direction, root, cells.max(1));
        }
        self.apply_stored_attach_viewport(session_id);
        Ok(())
    }

    fn toggle_zoom(&mut self, session_id: SessionId) -> Result<(Uuid, bool)> {
        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        let focused = session.focused_pane_id;
        if session.zoomed_pane_id.is_some() {
            session.zoomed_pane_id = None;
            self.apply_stored_attach_viewport(session_id);
            self.host.mark_snapshot_dirty();
            Ok((focused, false))
        } else {
            // Only zoom if there are at least 2 tiled panes (zooming a single pane is a no-op).
            let pane_ids = ordered_tiled_pane_ids(session);
            if pane_ids.len() < 2 {
                return Ok((focused, false));
            }
            session.zoomed_pane_id = Some(focused);
            self.apply_stored_attach_viewport(session_id);
            self.host.mark_snapshot_dirty();
            Ok((focused, true))
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "Floating pane creation mirrors the BPDL command fields and avoids a plugin-private options type at this boundary."
    )]
    fn create_floating_pane(
        &mut self,
        session_id: SessionId,
        target: Option<PaneSelector>,
        rect: LayoutRect,
        scope: FloatingPaneScope,
        layer: FloatingPaneLayer,
        z: i32,
        name: Option<String>,
        command: Option<PaneLaunchCommand>,
        anchor_pane_id: Option<Uuid>,
        context_id: Option<Uuid>,
        client_id: Option<ClientId>,
    ) -> Result<FloatingPaneRuntimeSummary> {
        let _admission = managed_idle::admit(&self.host)?;
        reservations::check_authority(&self.host)?;
        let (target_pane_id, next_pane_name, shell, client_ids) = {
            let session = self
                .runtimes
                .get_mut(&session_id)
                .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
            let target_pane_id =
                resolve_pane_id_from_selector(session, &target.unwrap_or(PaneSelector::Active))
                    .unwrap_or(session.focused_pane_id);
            let shell = session
                .panes
                .get(&target_pane_id)
                .map_or_else(|| self.shell.clone(), |pane| pane.meta.shell.clone());
            (
                target_pane_id,
                name.or_else(|| Some(format!("floating-pane-{}", session.panes.len() + 1))),
                shell,
                session.attached_clients.iter().copied().collect::<Vec<_>>(),
            )
        };

        let pane_id = crate::ids::new_uuid();
        let pane_meta = PaneRuntimeMeta {
            id: pane_id,
            name: next_pane_name,
            shell,
            launch: command.map(pane_launch_spec_from_command).transpose()?,
            resurrection: PaneResurrectionSnapshot::default(),
        };
        let handle = self.spawn_pane_runtime(session_id, pane_meta, (24, 80));
        for client_id in client_ids {
            if let Ok(mut output) = handle.output_buffer.lock() {
                output.register_client_at_tail(client_id);
            }
        }

        let surface = FloatingSurfaceRuntime {
            id: crate::ids::new_uuid(),
            pane_id,
            anchor_pane_id: matches!(scope, FloatingPaneScope::PerPane)
                .then_some(anchor_pane_id.unwrap_or(target_pane_id)),
            context_id: matches!(scope, FloatingPaneScope::PerWindow)
                .then(|| {
                    context_id.or_else(|| self.host.current_context_id_for_session(session_id))
                })
                .flatten(),
            client_id: matches!(scope, FloatingPaneScope::ClientGlobal)
                .then_some(client_id)
                .flatten(),
            rect,
            scope,
            layer,
            z,
            visible: true,
            opaque: true,
            accepts_input: true,
            cursor_owner: true,
        };
        let session = self
            .runtimes
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        let _ = target_pane_id;
        session.panes.insert(pane_id, handle);
        session
            .floating_surfaces
            .iter_mut()
            .for_each(|surface| surface.cursor_owner = false);
        session.floating_surfaces.push(surface);
        session.focused_pane_id = pane_id;
        self.register_pane_input(session_id, &self.runtimes[&session_id].panes[&pane_id]);
        self.apply_stored_attach_viewport(session_id);
        Ok(floating_summary(surface))
    }

    fn list_floating_panes(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<FloatingPaneRuntimeSummary>> {
        let session = self
            .runtimes
            .get(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        Ok(session
            .floating_surfaces
            .iter()
            .copied()
            .filter(|surface| session.panes.contains_key(&surface.pane_id))
            .map(floating_summary)
            .collect())
    }

    fn update_floating_pane(
        &mut self,
        _session_id: SessionId,
        pane_id: Uuid,
        update: impl FnOnce(&mut FloatingSurfaceRuntime, i32, i32),
    ) -> Result<FloatingPaneRuntimeSummary> {
        let owner_session_id = self
            .runtimes
            .iter()
            .find_map(|(owner_session_id, runtime)| {
                runtime
                    .floating_surfaces
                    .iter()
                    .any(|surface| surface.pane_id == pane_id)
                    .then_some(*owner_session_id)
            })
            .ok_or_else(|| anyhow::anyhow!("floating pane not found"))?;
        let session = self.runtimes.get_mut(&owner_session_id).ok_or_else(|| {
            anyhow::anyhow!("runtime not found for session {}", owner_session_id.0)
        })?;
        let max_z = session
            .floating_surfaces
            .iter()
            .map(|surface| surface.z)
            .max()
            .unwrap_or(0);
        let min_z = session
            .floating_surfaces
            .iter()
            .map(|surface| surface.z)
            .min()
            .unwrap_or(0);
        let surface = session
            .floating_surfaces
            .iter_mut()
            .find(|surface| surface.pane_id == pane_id)
            .ok_or_else(|| anyhow::anyhow!("floating pane not found"))?;
        update(surface, min_z, max_z);
        let summary = floating_summary(*surface);
        self.apply_stored_attach_viewport(owner_session_id);
        Ok(summary)
    }

    fn move_floating_pane(
        &mut self,
        session_id: SessionId,
        pane_id: Uuid,
        x: u16,
        y: u16,
    ) -> Result<FloatingPaneRuntimeSummary> {
        self.update_floating_pane(session_id, pane_id, |surface, _, _| {
            surface.rect.x = x;
            surface.rect.y = y;
        })
    }

    fn resize_floating_pane(
        &mut self,
        session_id: SessionId,
        pane_id: Uuid,
        w: u16,
        h: u16,
    ) -> Result<FloatingPaneRuntimeSummary> {
        self.update_floating_pane(session_id, pane_id, |surface, _, _| {
            surface.rect.w = w.max(1);
            surface.rect.h = h.max(1);
        })
    }

    fn focus_floating_pane(
        &mut self,
        _session_id: SessionId,
        pane_id: Uuid,
    ) -> Result<FloatingPaneRuntimeSummary> {
        let mut found = None;
        let mut owner_session_id = None;
        for (candidate_session_id, session) in &mut self.runtimes {
            for surface in &mut session.floating_surfaces {
                surface.cursor_owner = surface.pane_id == pane_id;
                if surface.cursor_owner {
                    found = Some(*surface);
                    owner_session_id = Some(*candidate_session_id);
                }
            }
            if session.panes.contains_key(&pane_id) {
                session.focused_pane_id = pane_id;
            }
        }
        let surface = found.ok_or_else(|| anyhow::anyhow!("floating pane not found"))?;
        if let Some(owner_session_id) = owner_session_id {
            self.apply_stored_attach_viewport(owner_session_id);
        }
        Ok(floating_summary(surface))
    }

    fn raise_floating_pane(
        &mut self,
        session_id: SessionId,
        pane_id: Uuid,
    ) -> Result<FloatingPaneRuntimeSummary> {
        self.update_floating_pane(session_id, pane_id, |surface, _, max_z| {
            surface.z = max_z.saturating_add(1);
        })
    }

    fn lower_floating_pane(
        &mut self,
        session_id: SessionId,
        pane_id: Uuid,
    ) -> Result<FloatingPaneRuntimeSummary> {
        self.update_floating_pane(session_id, pane_id, |surface, min_z, _| {
            surface.z = min_z.saturating_sub(1);
        })
    }

    fn set_floating_pane_z(
        &mut self,
        session_id: SessionId,
        pane_id: Uuid,
        z: i32,
    ) -> Result<FloatingPaneRuntimeSummary> {
        self.update_floating_pane(session_id, pane_id, |surface, _, _| {
            surface.z = z;
        })
    }

    fn set_floating_pane_layer(
        &mut self,
        session_id: SessionId,
        pane_id: Uuid,
        layer: FloatingPaneLayer,
    ) -> Result<FloatingPaneRuntimeSummary> {
        self.update_floating_pane(session_id, pane_id, |surface, _, _| {
            surface.layer = layer;
        })
    }

    fn close_floating_pane(
        &mut self,
        session_id: SessionId,
        pane_id: Uuid,
    ) -> Result<(Uuid, Option<RemovedRuntime>)> {
        let owner_session_id = self
            .pane_session_for_attach(session_id, pane_id)
            .unwrap_or(session_id);
        let remove_runtime = {
            let session = self.runtimes.get(&owner_session_id).ok_or_else(|| {
                anyhow::anyhow!("runtime not found for session {}", owner_session_id.0)
            })?;
            if !session
                .floating_surfaces
                .iter()
                .any(|surface| surface.pane_id == pane_id)
            {
                anyhow::bail!("floating pane not found");
            }
            let remove_count = 1 + session
                .floating_surfaces
                .iter()
                .filter(|surface| {
                    surface.anchor_pane_id == Some(pane_id) && surface.pane_id != pane_id
                })
                .count();
            session.panes.len() <= remove_count
        };
        if remove_runtime {
            anyhow::bail!("cannot close the final pane without an explicit session action");
        }
        self.close_pane(owner_session_id, Some(PaneSelector::ById(pane_id)))
    }

    #[allow(clippy::cast_possible_truncation)]
    fn list_panes(&self, session_id: SessionId) -> Result<Vec<PaneSummary>> {
        let session = self
            .runtimes
            .get(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        let pane_ids = ordered_pane_ids(session);
        let panes = pane_ids
            .iter()
            .enumerate()
            .filter_map(|(index, pane_id)| {
                session.panes.get(pane_id).map(|pane| PaneSummary {
                    id: *pane_id,
                    index: (index + 1) as u32,
                    name: pane.meta.name.clone(),
                    focused: *pane_id == session.focused_pane_id,
                    state: pane_state_for_handle(pane),
                    state_reason: pane_state_reason_for_handle(pane),
                })
            })
            .collect::<Vec<_>>();
        Ok(panes)
    }

    fn build_attach_scene_for_client(
        &self,
        session_id: SessionId,
        client_id: ClientId,
    ) -> Result<AttachScene, SessionRuntimeError> {
        let session = self
            .runtimes
            .get(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        let context_id = self.host.current_context_id_for_session(session_id);
        let mut scene = build_attach_scene(
            session_id,
            session,
            &self.padding_config,
            session.attach_viewport,
            client_id,
            context_id,
        );
        for (owner_session_id, owner_runtime) in &self.runtimes {
            if *owner_session_id == session_id {
                continue;
            }
            scene.surfaces.extend(
                owner_runtime
                    .floating_surfaces
                    .iter()
                    .filter(|surface| {
                        floating_surface_visible_for_attach(
                            *owner_session_id,
                            session_id,
                            owner_runtime,
                            surface,
                            client_id,
                            context_id,
                        )
                    })
                    .map(|surface| {
                        attach_surface_from_floating(
                            &self.padding_config,
                            owner_runtime
                                .panes
                                .get(&surface.pane_id)
                                .expect("floating pane exists"),
                            surface,
                        )
                    }),
            );
        }
        let pane_id = focused_pane_for_scene(&scene, session.focused_pane_id);
        Ok(AttachScene {
            focus: AttachFocusTarget::Pane { pane_id },
            ..scene
        })
    }

    fn pane_session_for_attach(
        &self,
        attach_session_id: SessionId,
        pane_id: Uuid,
    ) -> Option<SessionId> {
        if self
            .runtimes
            .get(&attach_session_id)
            .is_some_and(|runtime| runtime.panes.contains_key(&pane_id))
        {
            return Some(attach_session_id);
        }
        self.runtimes.iter().find_map(|(session_id, runtime)| {
            runtime.panes.contains_key(&pane_id).then_some(*session_id)
        })
    }

    fn attach_scene_pane_refs(
        &self,
        attach_session_id: SessionId,
        scene: &AttachScene,
    ) -> Vec<(SessionId, Uuid)> {
        let mut seen = BTreeSet::new();
        scene
            .surfaces
            .iter()
            .filter(|surface| surface.visible && surface.pane_id.is_some())
            .filter_map(|surface| surface.pane_id)
            .filter_map(|pane_id| {
                let owner_session_id = self.pane_session_for_attach(attach_session_id, pane_id)?;
                seen.insert(pane_id).then_some((owner_session_id, pane_id))
            })
            .collect()
    }

    fn attach_scene_data_snapshot(
        &self,
        session_id: SessionId,
        client_id: ClientId,
    ) -> Result<AttachSceneDataSnapshot, SessionRuntimeError> {
        let session = self
            .runtimes
            .get(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        if !session.attached_clients.contains(&client_id) {
            return Err(SessionRuntimeError::NotAttached);
        }
        let scene = self.build_attach_scene_for_client(session_id, client_id)?;
        let pane_refs = self.attach_scene_pane_refs(session_id, &scene);
        let focused_pane_id = focused_pane_for_scene(&scene, session.focused_pane_id);
        let panes = pane_refs
            .into_iter()
            .filter_map(|(owner_session_id, pane_id)| {
                self.runtimes
                    .get(&owner_session_id)?
                    .panes
                    .get(&pane_id)
                    .map(|pane| pane_attach_data_ref(owner_session_id, pane_id, pane))
            })
            .collect::<Vec<_>>();
        Ok(AttachSceneDataSnapshot {
            focused_pane_id,
            panes,
            layout_root: fallback_ipc_layout(session),
            scene,
            zoomed: session.zoomed_pane_id.is_some(),
        })
    }

    fn attach_pane_data_refs_for_scene_panes(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        pane_ids: &[Uuid],
    ) -> Result<Vec<PaneAttachDataRef>, SessionRuntimeError> {
        let session = self
            .runtimes
            .get(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        if !session.attached_clients.contains(&client_id) {
            return Err(SessionRuntimeError::NotAttached);
        }
        Ok(pane_ids
            .iter()
            .filter_map(|pane_id| {
                let owner_session_id = self.pane_session_for_attach(session_id, *pane_id)?;
                self.runtimes
                    .get(&owner_session_id)?
                    .panes
                    .get(pane_id)
                    .map(|pane| pane_attach_data_ref(owner_session_id, *pane_id, pane))
            })
            .collect())
    }

    fn attach_pane_data_refs_for_session_panes(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        pane_ids: &[Uuid],
    ) -> Result<Vec<PaneAttachDataRef>, SessionRuntimeError> {
        let session = self
            .runtimes
            .get(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        if !session.attached_clients.contains(&client_id) {
            return Err(SessionRuntimeError::NotAttached);
        }
        Ok(pane_ids
            .iter()
            .filter_map(|pane_id| {
                session
                    .panes
                    .get(pane_id)
                    .map(|pane| pane_attach_data_ref(session_id, *pane_id, pane))
            })
            .collect())
    }

    /// The panes of `targets` (session, pane) that exist, whoever is attached.
    fn pane_data_refs_for_targets(&self, targets: &[(SessionId, Uuid)]) -> Vec<PaneAttachDataRef> {
        targets
            .iter()
            .filter_map(|(session_id, pane_id)| {
                let pane = self.runtimes.get(session_id)?.panes.get(pane_id)?;
                Some(pane_attach_data_ref(*session_id, *pane_id, pane))
            })
            .collect()
    }

    fn attach_pane_data_ref_for_push(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
        client_id: ClientId,
    ) -> Option<PaneAttachDataRef> {
        let session = self.runtimes.get(&session_id)?;
        if !session.attached_clients.contains(&client_id) {
            return None;
        }
        let pane = session.panes.get(&pane_id)?;
        Some(pane_attach_data_ref(session_id, pane_id, pane))
    }

    fn pane_data_refs_for_images(
        &self,
        session_id: SessionId,
        pane_ids: &[Uuid],
    ) -> Vec<PaneAttachDataRef> {
        let Some(session) = self.runtimes.get(&session_id) else {
            return Vec::new();
        };
        pane_ids
            .iter()
            .filter_map(|pane_id| {
                session
                    .panes
                    .get(pane_id)
                    .map(|pane| pane_attach_data_ref(session_id, *pane_id, pane))
            })
            .collect()
    }

    fn persistence_snapshot_parts(
        &self,
        session_id: SessionId,
    ) -> anyhow::Result<Option<PersistenceSnapshotParts>> {
        let Some(runtime) = self.runtimes.get(&session_id) else {
            return Ok(None);
        };
        validate_runtime_layout_matches_panes(runtime).with_context(|| {
            format!(
                "cannot snapshot inconsistent layout for session {}",
                session_id.0
            )
        })?;
        let pane_ids = ordered_pane_ids(runtime);
        let mut panes = Vec::with_capacity(pane_ids.len());
        for pane_id in pane_ids {
            let Some(pane) = runtime.panes.get(&pane_id) else {
                anyhow::bail!(
                    "layout references missing pane {pane_id} in session {}",
                    session_id.0
                );
            };
            panes.push(pane_persistence_data_ref(pane));
        }
        Ok(Some(PersistenceSnapshotParts {
            focused_pane_id: runtime.focused_pane_id,
            zoomed_pane_id: runtime.zoomed_pane_id,
            layout_root: runtime.layout_root.clone(),
            floating_surfaces: runtime.floating_surfaces.clone(),
            attached_clients: runtime.attached_clients.clone(),
            attach_viewport: runtime.attach_viewport,
            panes,
        }))
    }

    #[allow(dead_code, clippy::cast_possible_truncation)]
    fn attach_snapshot_state(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
        max_bytes_per_pane: usize,
    ) -> Result<AttachSnapshotState, SessionRuntimeError> {
        if !self
            .runtimes
            .get(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?
            .attached_clients
            .contains(&client_id)
        {
            return Err(SessionRuntimeError::NotAttached);
        }
        let scene = self.build_attach_scene_for_client(session_id, client_id)?;
        let pane_refs = self.attach_scene_pane_refs(session_id, &scene);
        let focused_pane_id = focused_pane_for_scene(
            &scene,
            self.runtimes
                .get(&session_id)
                .map_or(Uuid::nil(), |runtime| runtime.focused_pane_id),
        );
        let panes = pane_refs
            .iter()
            .enumerate()
            .filter_map(|(index, (owner_session_id, pane_id))| {
                self.runtimes
                    .get(owner_session_id)?
                    .panes
                    .get(pane_id)
                    .map(|pane| PaneSummary {
                        id: *pane_id,
                        index: (index + 1) as u32,
                        name: pane.meta.name.clone(),
                        focused: *pane_id == focused_pane_id,
                        state: pane_state_for_handle(pane),
                        state_reason: pane_state_reason_for_handle(pane),
                    })
            })
            .collect::<Vec<_>>();

        let mut chunks = Vec::new();
        let mut pane_mouse_protocols = Vec::new();
        let mut pane_input_modes = Vec::new();
        let num_panes = pane_refs.len().max(1);
        let per_pane_budget = (RESPONSE_OUTPUT_BUDGET / num_panes).min(max_bytes_per_pane);
        let mut budget_remaining = RESPONSE_OUTPUT_BUDGET;
        for (owner_session_id, pane_id) in pane_refs {
            let Some(pane) = self
                .runtimes
                .get_mut(&owner_session_id)
                .and_then(|runtime| runtime.panes.get_mut(&pane_id))
            else {
                continue;
            };
            let protocol = pane
                .mouse_protocol_state
                .lock()
                .map(|state| *state)
                .unwrap_or_default();
            pane_mouse_protocols.push(AttachPaneMouseProtocol { pane_id, protocol });
            let mode = pane
                .input_mode_state
                .lock()
                .map(|state| *state)
                .unwrap_or_default();
            pane_input_modes.push(AttachPaneInputMode { pane_id, mode });
            let allowed = per_pane_budget.min(budget_remaining);
            let mut output = pane
                .output_buffer
                .lock()
                .map_err(|_| SessionRuntimeError::Closed)?;
            let read = output.read_recent_with_offsets(allowed);
            output.advance_client_to_end(client_id);
            let sync_at_read_end = output.sync_update_open_at(read.stream_end);
            drop(output);

            budget_remaining = budget_remaining.saturating_sub(read.bytes.len());
            pane.output_dirty.store(false, Ordering::SeqCst);
            let sync_update_active = sync_at_read_end;
            chunks.push(AttachPaneChunk {
                pane_id,
                data: read.bytes,
                stream_start: read.stream_start,
                stream_end: read.stream_end,
                stream_gap: read.stream_gap,
                sync_update_active,
            });
        }

        let session = self
            .runtimes
            .get(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        Ok(AttachSnapshotState {
            focused_pane_id,
            panes,
            layout_root: fallback_ipc_layout(session),
            scene,
            chunks,
            pane_mouse_protocols,
            pane_input_modes,
            zoomed: session.zoomed_pane_id.is_some(),
        })
    }

    #[allow(dead_code)]
    fn read_pane_output_batch(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
        pane_ids: &[Uuid],
        max_bytes: usize,
    ) -> Result<Vec<AttachPaneChunk>, SessionRuntimeError> {
        let chunks = {
            if !self
                .runtimes
                .get(&session_id)
                .ok_or(SessionRuntimeError::NotFound)?
                .attached_clients
                .contains(&client_id)
            {
                return Err(SessionRuntimeError::NotAttached);
            }

            let mut chunks = Vec::new();
            let num_panes = pane_ids.len().max(1);
            let per_pane_budget = (RESPONSE_OUTPUT_BUDGET / num_panes).min(max_bytes);
            let mut budget_remaining = RESPONSE_OUTPUT_BUDGET;
            for pane_id in pane_ids {
                let Some(owner_session_id) = self.pane_session_for_attach(session_id, *pane_id)
                else {
                    continue;
                };
                let Some(pane) = self
                    .runtimes
                    .get_mut(&owner_session_id)
                    .and_then(|runtime| runtime.panes.get_mut(pane_id))
                else {
                    continue;
                };
                let allowed = per_pane_budget.min(budget_remaining);
                let mut output = pane
                    .output_buffer
                    .lock()
                    .map_err(|_| SessionRuntimeError::Closed)?;
                let read = output.read_for_client(client_id, allowed);
                let sync_at_read_end = output.sync_update_open_at(read.stream_end);
                drop(output);
                budget_remaining = budget_remaining.saturating_sub(read.bytes.len());
                let sync_update_active = sync_at_read_end;
                chunks.push(AttachPaneChunk {
                    pane_id: *pane_id,
                    data: read.bytes,
                    stream_start: read.stream_start,
                    stream_end: read.stream_end,
                    stream_gap: read.stream_gap,
                    sync_update_active,
                });
            }
            chunks
        };

        Ok(chunks)
    }

    #[allow(dead_code)]
    fn attach_pane_snapshot_state(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
        pane_ids: &[Uuid],
        max_bytes_per_pane: usize,
    ) -> Result<AttachPaneSnapshotState, SessionRuntimeError> {
        if !self
            .runtimes
            .get(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?
            .attached_clients
            .contains(&client_id)
        {
            return Err(SessionRuntimeError::NotAttached);
        }

        let num_panes = pane_ids.len().max(1);
        let per_pane_budget = (RESPONSE_OUTPUT_BUDGET / num_panes).min(max_bytes_per_pane);
        let mut budget_remaining = RESPONSE_OUTPUT_BUDGET;
        let mut chunks = Vec::new();
        let mut pane_mouse_protocols = Vec::new();
        let mut pane_input_modes = Vec::new();
        let mut seen = BTreeSet::new();

        for pane_id in pane_ids {
            if !seen.insert(*pane_id) {
                continue;
            }

            let Some(owner_session_id) = self.pane_session_for_attach(session_id, *pane_id) else {
                continue;
            };
            let Some(pane) = self
                .runtimes
                .get_mut(&owner_session_id)
                .and_then(|runtime| runtime.panes.get_mut(pane_id))
            else {
                continue;
            };

            let protocol = pane
                .mouse_protocol_state
                .lock()
                .map(|state| *state)
                .unwrap_or_default();
            pane_mouse_protocols.push(AttachPaneMouseProtocol {
                pane_id: *pane_id,
                protocol,
            });
            let mode = pane
                .input_mode_state
                .lock()
                .map(|state| *state)
                .unwrap_or_default();
            pane_input_modes.push(AttachPaneInputMode {
                pane_id: *pane_id,
                mode,
            });

            let allowed = per_pane_budget.min(budget_remaining);
            let mut output = pane
                .output_buffer
                .lock()
                .map_err(|_| SessionRuntimeError::Closed)?;
            let read = output.read_recent_with_offsets(allowed);
            output.advance_client_to_end(client_id);
            let sync_at_read_end = output.sync_update_open_at(read.stream_end);
            drop(output);

            budget_remaining = budget_remaining.saturating_sub(read.bytes.len());
            pane.output_dirty.store(false, Ordering::SeqCst);
            let sync_update_active = sync_at_read_end;
            chunks.push(AttachPaneChunk {
                pane_id: *pane_id,
                data: read.bytes,
                stream_start: read.stream_start,
                stream_end: read.stream_end,
                stream_gap: read.stream_gap,
                sync_update_active,
            });
        }

        Ok(AttachPaneSnapshotState {
            chunks,
            pane_mouse_protocols,
            pane_input_modes,
        })
    }

    fn remove_runtime(&mut self, session_id: SessionId) -> Result<RemovedRuntime> {
        let removed_pane_ids = self
            .runtimes
            .get(&session_id)
            .map(|runtime| runtime.panes.keys().copied().collect::<BTreeSet<_>>())
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        Self::drop_padding_preview_targets(&mut self.padding_previews, &removed_pane_ids);
        let runtime = self
            .runtimes
            .remove(&session_id)
            .ok_or_else(|| anyhow::anyhow!("runtime not found for session {}", session_id.0))?;
        self.unregister_session_inputs(session_id, runtime.panes.keys().copied());

        Ok(RemovedRuntime {
            session_id,
            handle: runtime,
        })
    }

    fn remove_all_runtimes(&mut self) -> Vec<RemovedRuntime> {
        self.padding_previews.clear();
        let runtimes = std::mem::take(&mut self.runtimes);
        if let Ok(mut index) = self.pane_input_index.write() {
            index.clear();
        }
        if let Ok(mut permissions) = self.client_write_permissions.write() {
            permissions.clear();
        }
        runtimes
            .into_iter()
            .map(|(session_id, runtime)| RemovedRuntime {
                session_id,
                handle: runtime,
            })
            .collect()
    }

    fn begin_attach(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
    ) -> Result<(), SessionRuntimeError> {
        let host = self.host.clone();
        let host = &host;
        let _admission = managed_idle::admit(host).map_err(|_| SessionRuntimeError::Closed)?;
        if crate::handlers::attach_commands::observer_identity(host, client_id) {
            return Err(SessionRuntimeError::NotAttached);
        }
        let runtime = self
            .runtimes
            .get_mut(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;

        let _pane = runtime
            .panes
            .get(&runtime.focused_pane_id)
            .ok_or(SessionRuntimeError::NotFound)?;

        let pane_ids = runtime.panes.keys().copied().collect::<Vec<_>>();
        // Lock every output before publishing any input or attachment membership.
        // Failed admission must not leave a partially attached producer behind.
        let mut outputs = runtime
            .panes
            .values()
            .map(|pane| {
                pane.output_buffer
                    .lock()
                    .map_err(|_| SessionRuntimeError::Closed)
            })
            .collect::<Result<Vec<_>, _>>()?;
        if !crate::private_input::register_producers(host, &pane_ids, client_id.0) {
            return Err(SessionRuntimeError::Closed);
        }
        runtime.attached_clients.insert(client_id);
        for output in &mut outputs {
            output.register_client_at_tail(client_id);
        }
        drop(outputs);
        if runtime.attach_viewport.is_some() {
            resize_session_ptys(&self.padding_config, runtime);
        }
        Ok(())
    }

    fn cancel_previews_for_client(&mut self, client_id: ClientId) {
        let tokens = self
            .padding_previews
            .iter()
            .filter_map(|(token, preview)| (preview.owner_client_id == client_id).then_some(*token))
            .collect::<Vec<_>>();
        for token in tokens {
            let _ = self.cancel_padding_preview(client_id, token);
        }
    }

    fn cancel_expired_padding_previews(&mut self, now: Instant) {
        let expired = self
            .padding_previews
            .iter()
            .filter_map(|(token, preview)| {
                (preview.lease_expires_at <= now).then_some((*token, preview.owner_client_id))
            })
            .collect::<Vec<_>>();
        for (token, owner_client_id) in expired {
            let _ = self.cancel_padding_preview(owner_client_id, token);
        }
    }

    fn cancel_all_padding_previews(&mut self) {
        let previews = self
            .padding_previews
            .iter()
            .map(|(token, preview)| (*token, preview.owner_client_id))
            .collect::<Vec<_>>();
        for (token, owner_client_id) in previews {
            let _ = self.cancel_padding_preview(owner_client_id, token);
        }
    }

    fn end_attach(&mut self, session_id: SessionId, client_id: ClientId) {
        let host = self.host.clone();
        let host = &host;
        self.cancel_previews_for_client(client_id);
        if let Some(runtime) = self.runtimes.get_mut(&session_id) {
            let removed = runtime.attached_clients.remove(&client_id);
            if removed {
                runtime
                    .scrollback_pins
                    .retain(|(pin_client_id, _), _| *pin_client_id != client_id);
                for pane in runtime.panes.values_mut() {
                    crate::private_input::unregister_producer(host, pane.meta.id, client_id.0);
                    if let Ok(mut output) = pane.output_buffer.lock() {
                        output.unregister_client(client_id);
                    }
                    if let Ok(mut replicas) = pane.frame_replicas.lock() {
                        replicas.remove(&client_id);
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn set_attach_viewport(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
        cols: u16,
        rows: u16,
        top_inset: u16,
        right_inset: u16,
        bottom_inset: u16,
        left_inset: u16,
        cell_pixel_width: u16,
        cell_pixel_height: u16,
    ) -> Result<(u16, u16, u16, u16, u16, u16), SessionRuntimeError> {
        let host = self.host.clone();
        let host = &host;
        if crate::handlers::attach_commands::observer_identity(host, client_id) {
            return Err(SessionRuntimeError::NotAttached);
        }
        let runtime = self
            .runtimes
            .get_mut(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;

        if !runtime.attached_clients.contains(&client_id) {
            return Err(SessionRuntimeError::NotAttached);
        }

        let viewport = AttachViewport::normalized(
            cols,
            rows,
            top_inset,
            right_inset,
            bottom_inset,
            left_inset,
        );
        runtime.attach_viewport = Some(viewport);
        resize_session_ptys(&self.padding_config, runtime);

        // Update cell pixel dimensions for image placement sizing.
        #[cfg(feature = "image-registry")]
        if cell_pixel_width > 0 && cell_pixel_height > 0 {
            for pane in runtime.panes.values() {
                if let Ok(mut size) = pane.cell_pixel_size.lock() {
                    *size = (cell_pixel_width, cell_pixel_height);
                }
            }
        }
        #[cfg(not(feature = "image-registry"))]
        let _ = (cell_pixel_width, cell_pixel_height);

        Ok((
            viewport.cols,
            viewport.rows,
            viewport.top_inset,
            viewport.right_inset,
            viewport.bottom_inset,
            viewport.left_inset,
        ))
    }

    fn apply_stored_attach_viewport(&mut self, session_id: SessionId) {
        let Some(runtime) = self.runtimes.get_mut(&session_id) else {
            return;
        };
        let Some(_viewport) = runtime.attach_viewport else {
            return;
        };
        resize_session_ptys(&self.padding_config, runtime);
    }

    fn write_input(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
        data: Vec<u8>,
    ) -> Result<(usize, Uuid), SessionRuntimeError> {
        let host = self.host.clone();
        let host = &host;
        if crate::handlers::attach_commands::observer_identity(host, client_id) {
            return Err(SessionRuntimeError::NotAttached);
        }
        let fallback_focused_pane_id = {
            let runtime = self
                .runtimes
                .get(&session_id)
                .ok_or(SessionRuntimeError::NotFound)?;
            if !runtime.attached_clients.contains(&client_id) {
                return Err(SessionRuntimeError::NotAttached);
            }
            runtime.focused_pane_id
        };

        let scene = self.build_attach_scene_for_client(session_id, client_id)?;
        let focused_pane_id = match scene.focus {
            AttachFocusTarget::Pane { pane_id } => pane_id,
            AttachFocusTarget::None | AttachFocusTarget::Surface { .. } => fallback_focused_pane_id,
        };
        let owner_session_id = self
            .pane_session_for_attach(session_id, focused_pane_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        let pane = self
            .runtimes
            .get_mut(&owner_session_id)
            .and_then(|owner_runtime| owner_runtime.panes.get_mut(&focused_pane_id))
            .ok_or(SessionRuntimeError::NotFound)?;

        if pane.exited.load(Ordering::SeqCst) {
            return Err(SessionRuntimeError::Closed);
        }

        // Scene visibility and write authorization above establish this producer's
        // target. Cross-session surfaces need not belong to the attached runtime.
        // Registration still rejects new producers during an active private scope
        // and invalidates an in-flight barrier when membership changes.
        if !crate::private_input::register_producer(host, focused_pane_id, client_id.0) {
            return Err(SessionRuntimeError::Closed);
        }
        let bytes = data.len();
        let recording_data = crate::private_input::with_producer_admission(
            host,
            focused_pane_id,
            client_id.0,
            || {
                crate::private_input::with_capture_barrier(host, focused_pane_id, |capture| {
                    let recording_data = capture
                        .then(|| self.host.pane_input_recording_data(&data))
                        .flatten();
                    pane.send_input(data, owner_session_id)?;
                    if let Some((handle, data)) = recording_data {
                        record_pane_input_raw(
                            &handle,
                            data,
                            owner_session_id,
                            focused_pane_id,
                            client_id,
                        );
                    }
                    Ok::<(), SessionRuntimeError>(())
                })
            },
        )
        .flatten();
        match recording_data {
            Some(result) => result?,
            None => return Err(SessionRuntimeError::Closed),
        }
        Ok((bytes, focused_pane_id))
    }

    fn read_output(
        &mut self,
        session_id: SessionId,
        client_id: ClientId,
        max_bytes: usize,
    ) -> Result<Vec<u8>, SessionRuntimeError> {
        let runtime = self
            .runtimes
            .get_mut(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;

        if !runtime.attached_clients.contains(&client_id) {
            return Err(SessionRuntimeError::NotAttached);
        }

        let pane = runtime
            .panes
            .get_mut(&runtime.focused_pane_id)
            .ok_or(SessionRuntimeError::NotFound)?;

        if max_bytes == 0 {
            return Ok(Vec::new());
        }

        let mut output = pane
            .output_buffer
            .lock()
            .map_err(|_| SessionRuntimeError::Closed)?;
        let read = output.read_for_client(client_id, max_bytes);
        drop(output);

        Ok(read.bytes)
    }
}

fn resolve_pane_id_from_selector(
    runtime: &SessionRuntimeHandle,
    selector: &PaneSelector,
) -> Option<Uuid> {
    match selector {
        PaneSelector::Active => runtime
            .panes
            .contains_key(&runtime.focused_pane_id)
            .then_some(runtime.focused_pane_id),
        PaneSelector::ById(id) => runtime.panes.contains_key(id).then_some(*id),
        PaneSelector::ByIndex(index) => {
            if *index == 0 {
                return None;
            }
            let pane_ids = ordered_pane_ids(runtime);
            let position = usize::try_from(index.saturating_sub(1)).ok()?;
            let pane_id = pane_ids.get(position).copied()?;
            runtime.panes.contains_key(&pane_id).then_some(pane_id)
        }
    }
}

fn pane_state_for_handle(pane: &PaneRuntimeHandle) -> PaneState {
    if pane.exited.load(Ordering::SeqCst) {
        PaneState::Exited
    } else {
        PaneState::Running
    }
}

fn pane_state_reason_for_handle(pane: &PaneRuntimeHandle) -> Option<String> {
    pane.exit_reason
        .lock()
        .ok()
        .and_then(|reason| reason.clone())
}

//
// Inline adapter implementing `SessionRuntimeManagerApi` over the
// server's own `Arc<Mutex<SessionRuntimeManager>>`. Registered into
// the plugin state registry during `BmuxServer::new` so
// `host.session_runtime_handle()` lookups reach this single manager
// instance.
//
// Kept in `lib.rs` to access the many private fields/methods of
// `SessionRuntimeManager`, `SessionRuntimeHandle`, and `PaneRuntimeHandle`
// without widening their visibility to `pub(crate)`. When the
// `SessionRuntimeManager` implementation finally relocates into the
// pane-runtime plugin crate, this adapter and its registration move
// with it.

struct ManagerLockHolder {
    operation: &'static str,
    /// The call that took the lock.
    site: &'static std::panic::Location<'static>,
    started: switchy_time::Instant,
}

/// How a fresh-runtime start ended (`None` while it runs), shared by every
/// caller waiting on it.
type RuntimeStartOutcome = Option<Result<(), String>>;

/// Fresh-runtime starts in flight, by session. A start runs to its end even
/// when every caller stopped waiting for it, and a later start of the same
/// session joins it instead of spawning a second first pane.
type RuntimeStartsInFlight = Arc<Mutex<BTreeMap<SessionId, watch::Receiver<RuntimeStartOutcome>>>>;

#[derive(Clone)]
struct ServerSessionRuntimeAdapter {
    inner: Arc<Mutex<SessionRuntimeManager>>,
    host: PaneRuntimeHost,
    pane_input_index: Arc<RwLock<BTreeMap<Uuid, PaneInputHandle>>>,
    client_write_permissions: Arc<RwLock<BTreeMap<SessionId, BTreeSet<ClientId>>>>,
    current_holder: Arc<Mutex<Option<ManagerLockHolder>>>,
    retained_pin_bytes: Arc<AtomicUsize>,
    starts: RuntimeStartsInFlight,
}

/// Ends a start's entry in [`RuntimeStartsInFlight`] however its task ends.
struct RuntimeStartEntry {
    starts: RuntimeStartsInFlight,
    session_id: SessionId,
}

impl Drop for RuntimeStartEntry {
    fn drop(&mut self) {
        if let Ok(mut starts) = self.starts.lock() {
            starts.remove(&self.session_id);
        }
    }
}

impl ServerSessionRuntimeAdapter {
    fn split_fresh_shell(
        &self,
        session_id: SessionId,
        target: Option<PaneSelector>,
        direction: PaneSplitDirection,
        options: Option<bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::InitialShellOptions>,
        ratio: f32,
        authorize: impl Fn() -> Result<()>,
    ) -> Result<Uuid> {
        let runtime = switchy_async::runtime::Handle::try_current()?;
        anyhow::ensure!(
            runtime.runtime_flavor() == switchy_async::RuntimeFlavor::MultiThread,
            "synchronous split requires the server's multi-thread runtime"
        );
        authorize()?;
        let mut prepared = self
            .with_lock(|m| m.prepare_split(session_id, target, direction, options))
            .ok_or_else(Self::lock_poisoned_anyhow)??;
        switchy_async::task::block_in_place(|| {
            runtime.block_on(async {
                let readiness = async {
                    prepared
                        .launch
                        .take()
                        .expect("split launch")
                        .send(())
                        .map_err(|()| anyhow::anyhow!("split startup task ended"))?;
                    let mut startup = prepared.pane.as_ref().expect("split owner").startup.clone();
                    switchy_async::time::timeout(Duration::from_secs(10), async {
                        loop {
                            if let Some(result) = startup.borrow_and_update().clone() {
                                return result.map_err(anyhow::Error::msg);
                            }
                            startup
                                .changed()
                                .await
                                .map_err(|_| anyhow::anyhow!("split startup ended"))?;
                        }
                    })
                    .await
                    .map_err(|_| anyhow::anyhow!("split startup timed out"))??;
                    authorize()?;
                    self.with_lock(|m| m.commit_split(session_id, &mut prepared, direction, ratio))
                        .ok_or_else(Self::lock_poisoned_anyhow)?
                }
                .await;
                if let Err(error) = readiness {
                    stop_unpublished_pane(prepared.pane.as_mut().expect("rejected split owner"))
                        .await
                        .map_err(|cleanup| spawn_failure(
                            bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SpawnError::CleanupFailed {
                                reason: format!("{error}; {cleanup}"),
                            }
                        ))?;
                    return Err(error);
                }
                readiness
            })
        })
    }

    async fn start_fresh_runtime(&self, session_id: SessionId) -> Result<()> {
        self.start_fresh_runtime_with_options(session_id, None, || Ok(()))
            .await
    }

    async fn start_fresh_runtime_with_options(
        &self,
        session_id: SessionId,
        options: Option<bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::InitialShellOptions>,
        authorize: impl Fn() -> Result<()>,
    ) -> Result<()> {
        self.start_fresh_runtime_transaction(session_id, options, authorize, || Ok(()))
            .await
    }

    async fn start_fresh_runtime_transaction(
        &self,
        session_id: SessionId,
        options: Option<bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::InitialShellOptions>,
        authorize: impl Fn() -> Result<()>,
        publish: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        authorize()?;
        let mut prepared = self
            .with_lock(|manager| manager.prepare_fresh_runtime(session_id, options))
            .ok_or_else(Self::lock_poisoned_anyhow)??;
        // Startup must be driven without holding the manager: readers and
        // unrelated service requests need to make progress while the PTY starts.
        let startup =
            switchy_async::time::timeout(Duration::from_secs(10), prepared.wait_until_ready())
                .await
                .map_err(|_| anyhow::anyhow!("pane startup readiness timed out"))
                .and_then(std::convert::identity);
        if let Err(error) = startup {
            let mut cleanup_errors = Vec::new();
            for pane in prepared
                .runtime
                .as_mut()
                .expect("startup owner")
                .panes
                .values_mut()
            {
                if let Err(cleanup) = stop_unpublished_pane(pane).await {
                    cleanup_errors.push(cleanup.to_string());
                }
            }
            return Err(spawn_error_after_cleanup(error, cleanup_errors));
        }
        let commit = authorize().and_then(|()| {
            self.with_lock(|manager| {
                manager.commit_fresh_runtime(session_id, &mut prepared, publish)
            })
            .ok_or_else(Self::lock_poisoned_anyhow)
            .and_then(std::convert::identity)
        });
        if let Err(error) = commit {
            let mut cleanup_errors = Vec::new();
            for pane in prepared
                .runtime
                .as_mut()
                .expect("rejected commit owner")
                .panes
                .values_mut()
            {
                if let Err(cleanup) = stop_unpublished_pane(pane).await {
                    cleanup_errors.push(cleanup.to_string());
                }
            }
            return Err(spawn_error_after_cleanup(error, cleanup_errors));
        }
        Ok(())
    }

    fn new(inner: Arc<Mutex<SessionRuntimeManager>>) -> Self {
        let (pane_input_index, client_write_permissions, retained_pin_bytes, host) =
            inner.lock().map_or_else(
                |_| {
                    (
                        Arc::new(RwLock::new(BTreeMap::new())),
                        Arc::new(RwLock::new(BTreeMap::new())),
                        Arc::new(AtomicUsize::new(0)),
                        PaneRuntimeHost::detached(),
                    )
                },
                |manager| {
                    (
                        Arc::clone(&manager.pane_input_index),
                        Arc::clone(&manager.client_write_permissions),
                        Arc::clone(&manager.retained_pin_bytes),
                        manager.host.clone(),
                    )
                },
            );
        Self {
            inner,
            host,
            pane_input_index,
            client_write_permissions,
            current_holder: Arc::new(Mutex::new(None)),
            retained_pin_bytes,
            starts: Arc::default(),
        }
    }

    /// The start of `session_id`'s fresh runtime: the one in flight, or a new
    /// one. Its task owns the start, so it completes (and publishes the
    /// runtime) whether or not anyone still waits.
    fn join_runtime_start(
        &self,
        session_id: SessionId,
    ) -> Result<watch::Receiver<RuntimeStartOutcome>> {
        let mut starts = self
            .starts
            .lock()
            .map_err(|_| anyhow::anyhow!("runtime starts lock poisoned"))?;
        if let Some(start) = starts.get(&session_id) {
            return Ok(start.clone());
        }
        let (finished, start) = watch::channel(None);
        starts.insert(session_id, start.clone());
        drop(starts);
        let adapter = self.clone();
        let entry = RuntimeStartEntry {
            starts: Arc::clone(&self.starts),
            session_id,
        };
        drop(switchy_async::task::spawn(async move {
            let outcome = adapter
                .start_fresh_runtime(session_id)
                .await
                .map_err(|error| format!("{error:#}"));
            // Published before the entry ends: a caller joining in between
            // reads this outcome instead of starting again.
            let _ = finished.send(Some(outcome));
            drop(entry);
        }));
        Ok(start)
    }

    fn reserve_grid_working_set(&self) -> Result<PinReservation, SessionRuntimeError> {
        PinReservation::acquire(&self.retained_pin_bytes, 2 * RESPONSE_OUTPUT_BUDGET)
    }

    fn current_holder_snapshot(
        &self,
    ) -> Option<(&'static str, &'static std::panic::Location<'static>, u128)> {
        self.current_holder.lock().ok().and_then(|holder| {
            holder
                .as_ref()
                .map(|h| (h.operation, h.site, h.started.elapsed().as_millis()))
        })
    }

    fn mark_holder_start(
        &self,
        operation: &'static str,
        site: &'static std::panic::Location<'static>,
    ) {
        if let Ok(mut holder) = self.current_holder.lock() {
            *holder = Some(ManagerLockHolder {
                operation,
                site,
                started: switchy_time::Instant::now(),
            });
        }
    }

    fn mark_holder_end(&self) {
        if let Ok(mut holder) = self.current_holder.lock() {
            *holder = None;
        }
    }

    // Warnings name the calling site, so every holder is identifiable.
    #[track_caller]
    fn with_lock<R>(&self, f: impl FnOnce(&mut SessionRuntimeManager) -> R) -> Option<R> {
        self.with_lock_named("unknown", f)
    }

    #[track_caller]
    fn with_lock_named<R>(
        &self,
        operation: &'static str,
        f: impl FnOnce(&mut SessionRuntimeManager) -> R,
    ) -> Option<R> {
        let site = std::panic::Location::caller();
        let wait_started = switchy_time::Instant::now();
        let mut guard = self.inner.lock().ok()?;
        let wait = wait_started.elapsed();
        if wait > Duration::from_millis(10) {
            let holder = self.current_holder_snapshot();
            warn!(
                operation,
                %site,
                wait_ms = wait.as_millis(),
                holder_operation = holder.map(|(op, ..)| op),
                holder_site = holder.map(|(_, site, _)| site.to_string()),
                holder_ms = holder.map(|(.., ms)| ms),
                "pane runtime manager lock wait exceeded hot-path budget"
            );
        }
        self.mark_holder_start(operation, site);
        let hold_started = switchy_time::Instant::now();
        let result = f(&mut guard);
        let revisions = std::mem::take(&mut guard.pending_layout_revisions);
        let hold = hold_started.elapsed();
        self.mark_holder_end();
        drop(guard);
        publish_layout_revisions(&self.host, revisions);
        if hold > Duration::from_millis(10) {
            warn!(
                operation,
                %site,
                hold_ms = hold.as_millis(),
                "pane runtime manager lock hold exceeded hot-path budget"
            );
        }
        Some(result)
    }

    #[track_caller]
    fn with_lock_read<R>(&self, f: impl FnOnce(&SessionRuntimeManager) -> R) -> Option<R> {
        self.with_lock_read_named("unknown", f)
    }

    #[track_caller]
    fn with_lock_read_named<R>(
        &self,
        operation: &'static str,
        f: impl FnOnce(&SessionRuntimeManager) -> R,
    ) -> Option<R> {
        let site = std::panic::Location::caller();
        let wait_started = switchy_time::Instant::now();
        let guard = self.inner.lock().ok()?;
        let wait = wait_started.elapsed();
        if wait > Duration::from_millis(10) {
            let holder = self.current_holder_snapshot();
            warn!(
                operation,
                %site,
                wait_ms = wait.as_millis(),
                holder_operation = holder.map(|(op, ..)| op),
                holder_site = holder.map(|(_, site, _)| site.to_string()),
                holder_ms = holder.map(|(.., ms)| ms),
                "pane runtime manager read lock wait exceeded hot-path budget"
            );
        }
        self.mark_holder_start(operation, site);
        let hold_started = switchy_time::Instant::now();
        let result = f(&guard);
        let hold = hold_started.elapsed();
        self.mark_holder_end();
        if hold > Duration::from_millis(10) {
            warn!(
                operation,
                %site,
                hold_ms = hold.as_millis(),
                "pane runtime manager read lock hold exceeded hot-path budget"
            );
        }
        Some(result)
    }

    fn remove_to_info(
        session_id: SessionId,
        handle: SessionRuntimeHandle,
    ) -> bmux_pane_runtime_state::RemovedRuntimeInfo {
        let attached = handle.attached_clients.clone();
        let boxed: Box<dyn std::any::Any + Send + 'static> = Box::new(handle);
        bmux_pane_runtime_state::RemovedRuntimeInfo {
            session_id,
            attached_clients: attached,
            // Core's server type holds the token behind std's lock.
            // simulation-safe: taken once by its single owner
            shutdown_token: Arc::new(std::sync::Mutex::new(Some(boxed))),
        }
    }

    fn lock_poisoned_anyhow() -> anyhow::Error {
        anyhow::anyhow!("session runtime manager lock poisoned")
    }

    fn take_shutdown_handle(
        info: &bmux_pane_runtime_state::RemovedRuntimeInfo,
    ) -> Option<SessionRuntimeHandle> {
        let boxed = {
            let mut guard = info.shutdown_token.lock().ok()?;
            guard.take()?
        };
        boxed.downcast::<SessionRuntimeHandle>().ok().map(|b| *b)
    }
}

/// Frames bringing `client_id`'s replicas of `panes` level with their grids (see
/// `attach_pane_frames`), and whether more remain for the next call.
fn frames_for_panes(
    panes: &[PaneAttachDataRef],
    client_id: ClientId,
    max_history_lines: usize,
) -> Result<(Vec<bmux_pane_runtime_state::AttachPaneFrame>, bool), SessionRuntimeError> {
    // As an output read does: output after this wakes the client again.
    for pane in panes {
        pane.output_dirty.store(false, Ordering::SeqCst);
    }
    let mut frames = Vec::new();
    let mut remaining = RESPONSE_OUTPUT_BUDGET;
    let mut still_pending = false;
    let mut seen = BTreeSet::new();
    for pane in panes {
        if !seen.insert(pane.pane_id) {
            continue;
        }
        let mut grid = pane
            .terminal_grid
            .lock()
            .map_err(|_| SessionRuntimeError::Closed)?;
        let mut replicas = pane
            .frame_replicas
            .lock()
            .map_err(|_| SessionRuntimeError::Closed)?;
        let replica = replicas.entry(client_id).or_default();
        let revision = grid.grid().revision();
        if replica.cursor.epoch != 0
            && replica.revision == revision
            && replica.clipboard_writes.is_empty()
        {
            continue;
        }
        // Sealing a frame not sent loses nothing: the next one is built from the same base.
        let mut encoded = Vec::new();
        let frame =
            grid.grid_mut()
                .write_frame_since(replica.cursor, max_history_lines, &mut encoded);
        let sync_update_active = grid.grid().synchronized_update();
        drop(grid);
        if encoded.len() > remaining && !frames.is_empty() {
            still_pending = true;
            continue;
        }
        remaining = remaining.saturating_sub(encoded.len());
        replica.cursor = frame.next;
        replica.revision = revision;
        let replay_markers = replica
            .replay_markers
            .drain(..)
            .flat_map(|(frame, written)| bmux_bench::corpus::marker(frame, written))
            .collect();
        let clipboard_writes = replica
            .clipboard_writes
            .drain(..)
            .map(
                |(selection, text)| bmux_pane_runtime_state::PaneClipboardWrite { selection, text },
            )
            .collect();
        frames.push(bmux_pane_runtime_state::AttachPaneFrame {
            pane_id: pane.pane_id,
            sync_update_active,
            encoded,
            replay_markers,
            clipboard_writes,
        });
    }
    // Output that arrived while these frames were built is sent next.
    let still_pending = still_pending
        || panes
            .iter()
            .any(|pane| pane.output_dirty.load(Ordering::SeqCst));
    Ok((frames, still_pending))
}

impl bmux_pane_runtime_state::SessionRuntimeManagerApi for ServerSessionRuntimeAdapter {
    fn start_runtime(&self, session_id: SessionId) -> anyhow::Result<()> {
        self.start_runtime_within(session_id, None)
    }

    fn start_runtime_within(
        &self,
        session_id: SessionId,
        budget: Option<Duration>,
    ) -> anyhow::Result<()> {
        let runtime = switchy_async::runtime::Handle::try_current()?;
        anyhow::ensure!(
            runtime.runtime_flavor() == switchy_async::RuntimeFlavor::MultiThread,
            "synchronous runtime creation requires the server's multi-thread runtime"
        );
        let mut start = self.join_runtime_start(session_id)?;
        let outcome = switchy_async::task::block_in_place(|| {
            runtime.block_on(async move {
                let finished = async {
                    start
                        .wait_for(Option::is_some)
                        .await
                        .ok()
                        .and_then(|outcome| outcome.clone())
                };
                match budget {
                    None => Ok(finished.await),
                    Some(budget) => switchy_async::time::timeout(budget, finished).await,
                }
            })
        });
        match outcome {
            Ok(Some(Ok(()))) => Ok(()),
            Ok(Some(Err(error))) => Err(anyhow::anyhow!(error)),
            Ok(None) => anyhow::bail!("runtime start for session {} ended unfinished", session_id.0),
            Err(_) => Err(bmux_pane_runtime_state::RuntimeStartPending { session_id }.into()),
        }
    }

    fn restore_runtime(
        &self,
        session_id: SessionId,
        panes: &[bmux_pane_runtime_state::PaneRuntimeMeta],
        request: RestoreRuntimeRequest,
    ) -> anyhow::Result<()> {
        self.with_lock(|m| m.restore_runtime(session_id, panes, request))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn remove_runtime(
        &self,
        session_id: SessionId,
    ) -> Option<bmux_pane_runtime_state::RemovedRuntimeInfo> {
        let removed = self
            .with_lock(|m| m.remove_runtime(session_id).ok())
            .flatten()?;
        Some(Self::remove_to_info(removed.session_id, removed.handle))
    }

    fn remove_all_runtimes(&self) -> Vec<bmux_pane_runtime_state::RemovedRuntimeInfo> {
        let Some(removed) = self.with_lock(SessionRuntimeManager::remove_all_runtimes) else {
            return Vec::new();
        };
        removed
            .into_iter()
            .map(|r| Self::remove_to_info(r.session_id, r.handle))
            .collect()
    }

    fn session_exists(&self, session_id: SessionId) -> bool {
        self.with_lock_read(|m| m.runtimes.contains_key(&session_id))
            .unwrap_or(false)
    }

    fn active_session_ids(&self) -> Vec<SessionId> {
        self.with_lock_read(SessionRuntimeManager::active_session_ids)
            .unwrap_or_default()
    }

    fn split_pane(
        &self,
        session_id: SessionId,
        target: Option<PaneSelector>,
        direction: PaneSplitDirection,
    ) -> anyhow::Result<Uuid> {
        self.split_fresh_shell(session_id, target, direction, None, 0.5, || Ok(()))
    }

    fn launch_pane(
        &self,
        session_id: SessionId,
        target: Option<PaneSelector>,
        direction: PaneSplitDirection,
        name: Option<String>,
        command: PaneLaunchCommand,
    ) -> anyhow::Result<Uuid> {
        self.with_lock(|m| m.launch_pane(session_id, target, direction, name, command))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn focus_pane(
        &self,
        session_id: SessionId,
        direction: PaneFocusDirection,
    ) -> anyhow::Result<Uuid> {
        self.with_lock(|m| m.focus_pane(session_id, direction))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn focus_pane_target(
        &self,
        session_id: SessionId,
        target: &PaneSelector,
    ) -> anyhow::Result<Uuid> {
        self.with_lock(|m| m.focus_pane_target(session_id, target))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn resize_pane(
        &self,
        session_id: SessionId,
        target: Option<PaneSelector>,
        direction: PaneResizeDirection,
        cells: u16,
    ) -> anyhow::Result<()> {
        self.with_lock(|m| m.resize_pane(session_id, target, direction, cells))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn set_pane_pty_size(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
        rows: u16,
        cols: u16,
    ) -> anyhow::Result<()> {
        self.with_lock_read_named("set_pane_pty_size", |manager| {
            let runtime = manager
                .runtimes
                .get(&session_id)
                .ok_or_else(|| anyhow::anyhow!("session {} not found", session_id.0))?;
            let pane = runtime
                .panes
                .get(&pane_id)
                .ok_or_else(|| anyhow::anyhow!("pane {pane_id} not found"))?;
            pane.resize_pty(rows, cols);
            Ok(())
        })
        .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn signal_pane_process(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
        signal: bmux_pane_runtime_state::PaneProcessSignal,
    ) -> anyhow::Result<()> {
        let process = self
            .pane_process_identity(session_id, pane_id)
            .ok_or_else(|| anyhow::anyhow!("pane {pane_id} not found"))?;
        send_process_signal(&process, signal)
    }

    fn close_pane(
        &self,
        session_id: SessionId,
        target: Option<PaneSelector>,
    ) -> anyhow::Result<(Uuid, Option<bmux_pane_runtime_state::RemovedRuntimeInfo>)> {
        let (pane_id, removed) = self
            .with_lock(|m| m.close_pane(session_id, target))
            .ok_or_else(Self::lock_poisoned_anyhow)??;
        let info = removed.map(|r| Self::remove_to_info(r.session_id, r.handle));
        Ok((pane_id, info))
    }

    fn restart_pane(
        &self,
        session_id: SessionId,
        target: Option<PaneSelector>,
    ) -> anyhow::Result<Uuid> {
        self.with_lock(|m| m.restart_pane(session_id, target))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn toggle_zoom(&self, session_id: SessionId) -> anyhow::Result<(Uuid, bool)> {
        self.with_lock(|m| m.toggle_zoom(session_id))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn create_floating_pane(
        &self,
        session_id: SessionId,
        target: Option<PaneSelector>,
        rect: LayoutRect,
        scope: FloatingPaneScope,
        layer: FloatingPaneLayer,
        z: i32,
        name: Option<String>,
        command: Option<PaneLaunchCommand>,
        anchor_pane_id: Option<Uuid>,
        context_id: Option<Uuid>,
        client_id: Option<ClientId>,
    ) -> anyhow::Result<FloatingPaneRuntimeSummary> {
        self.with_lock(|m| {
            m.create_floating_pane(
                session_id,
                target,
                rect,
                scope,
                layer,
                z,
                name,
                command,
                anchor_pane_id,
                context_id,
                client_id,
            )
        })
        .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn list_floating_panes(
        &self,
        session_id: SessionId,
    ) -> anyhow::Result<Vec<FloatingPaneRuntimeSummary>> {
        self.with_lock_read(|m| m.list_floating_panes(session_id))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn move_floating_pane(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
        x: u16,
        y: u16,
    ) -> anyhow::Result<FloatingPaneRuntimeSummary> {
        self.with_lock(|m| m.move_floating_pane(session_id, pane_id, x, y))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn resize_floating_pane(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
        w: u16,
        h: u16,
    ) -> anyhow::Result<FloatingPaneRuntimeSummary> {
        self.with_lock(|m| m.resize_floating_pane(session_id, pane_id, w, h))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn focus_floating_pane(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
    ) -> anyhow::Result<FloatingPaneRuntimeSummary> {
        self.with_lock(|m| m.focus_floating_pane(session_id, pane_id))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn raise_floating_pane(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
    ) -> anyhow::Result<FloatingPaneRuntimeSummary> {
        self.with_lock(|m| m.raise_floating_pane(session_id, pane_id))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn lower_floating_pane(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
    ) -> anyhow::Result<FloatingPaneRuntimeSummary> {
        self.with_lock(|m| m.lower_floating_pane(session_id, pane_id))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn set_floating_pane_z(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
        z: i32,
    ) -> anyhow::Result<FloatingPaneRuntimeSummary> {
        self.with_lock(|m| m.set_floating_pane_z(session_id, pane_id, z))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn set_floating_pane_layer(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
        layer: FloatingPaneLayer,
    ) -> anyhow::Result<FloatingPaneRuntimeSummary> {
        self.with_lock(|m| m.set_floating_pane_layer(session_id, pane_id, layer))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn close_floating_pane(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
    ) -> anyhow::Result<(Uuid, Option<bmux_pane_runtime_state::RemovedRuntimeInfo>)> {
        let (pane_id, removed) = self
            .with_lock(|m| m.close_floating_pane(session_id, pane_id))
            .ok_or_else(Self::lock_poisoned_anyhow)??;
        let info = removed.map(|r| Self::remove_to_info(r.session_id, r.handle));
        Ok((pane_id, info))
    }

    fn list_panes(&self, session_id: SessionId) -> anyhow::Result<Vec<PaneSummary>> {
        self.with_lock_read(|m| m.list_panes(session_id))
            .ok_or_else(Self::lock_poisoned_anyhow)?
    }

    fn pane_scrollback_metadata(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
    ) -> Option<bmux_pane_runtime_state::PaneScrollbackMetadata> {
        self.with_lock_read(|manager| {
            let pane = manager.runtimes.get(&session_id)?.panes.get(&pane_id)?;
            let (active_command, last_known_cwd) = pane
                .resurrection_state
                .lock()
                .ok()
                .map(|state| (state.active_command.clone(), state.last_known_cwd.clone()))
                .unwrap_or_default();
            Some(bmux_pane_runtime_state::PaneScrollbackMetadata {
                pane_id,
                name: pane.meta.name.clone(),
                shell: pane.meta.shell.clone(),
                active_command,
                last_known_cwd,
            })
        })
        .flatten()
    }

    fn foreground_jobs(&self) -> Vec<bmux_pane_runtime_state::ForegroundJob> {
        // The live terminal names its foreground group: a job is any group
        // but the shell's own (a launched program is its pane's job). Copy
        // the panes' handles out so no terminal is asked under the manager.
        let panes = self
            .with_lock_read_named("foreground_jobs", |manager| {
                manager
                    .runtimes
                    .iter()
                    .flat_map(|(session_id, runtime)| {
                        runtime.panes.iter().map(|(pane_id, pane)| {
                            (
                                *session_id,
                                *pane_id,
                                Arc::clone(&pane.execution),
                                Arc::clone(&pane.process_group_id),
                                Arc::clone(&pane.exited),
                                pane.meta.launch.is_some(),
                            )
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let groups = panes
            .into_iter()
            .filter(|(.., exited, _)| !exited.load(Ordering::SeqCst))
            .filter_map(|(session_id, pane_id, execution, own_group, _, launched)| {
                let group = execution.master.lock().ok().and_then(|master| {
                    master
                        .as_ref()?
                        .foreground_process_group()
                        .filter(|group| *group > 0)
                })?;
                let own = own_group.lock().ok().and_then(|group| *group);
                (launched || own != Some(group)).then_some((session_id, pane_id, group))
            })
            .collect::<Vec<_>>();
        if groups.is_empty() {
            return Vec::new();
        }
        // One process-table read for every pane, outside the manager lock.
        let Some(table) = crate::platform::process_table() else {
            return Vec::new();
        };
        groups
            .into_iter()
            .filter_map(|(session_id, pane_id, group)| {
                let commands = foreground_group_commands(&table, group);
                (!commands.is_empty()).then_some(bmux_pane_runtime_state::ForegroundJob {
                    session_id,
                    pane_id,
                    process_group_id: group,
                    commands,
                })
            })
            .collect()
    }

    fn pane_signals(&self) -> Vec<bmux_pane_runtime_state::PaneSignals> {
        // Copy the handles out so no pane's state is read under the manager.
        let panes = self
            .with_lock_read_named("pane_signals", |manager| {
                manager
                    .runtimes
                    .iter()
                    .flat_map(|(session_id, runtime)| {
                        runtime.panes.iter().map(|(pane_id, pane)| {
                            (
                                *session_id,
                                *pane_id,
                                Arc::clone(&pane.signals),
                                Arc::clone(&pane.terminal_grid),
                            )
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        panes
            .into_iter()
            .filter_map(|(session_id, pane_id, signals, grid)| {
                let signals = signals.lock().ok()?.clone();
                // Content, not cursor moves or redraws that change nothing.
                let screen_revision = grid.lock().ok()?.grid().content_revision();
                Some(bmux_pane_runtime_state::PaneSignals {
                    session_id,
                    pane_id,
                    revision: signals.revision,
                    title: signals.title,
                    working_directory: signals.working_directory,
                    progress: signals.progress,
                    exit_status: signals.exit_status,
                    notifications: signals.notifications,
                    last_notification: signals.last_notification,
                    bells: signals.bells,
                    report: signals.report,
                    reports: signals.reports,
                    screen_revision,
                })
            })
            .collect()
    }

    fn pane_screen_text(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
    ) -> Option<bmux_pane_runtime_state::PaneScreenText> {
        let grid = self
            .with_lock_read(|manager| {
                manager
                    .runtimes
                    .get(&session_id)?
                    .panes
                    .get(&pane_id)
                    .map(|pane| Arc::clone(&pane.terminal_grid))
            })
            .flatten()?;
        let stream = grid.lock().ok()?;
        let grid = stream.grid();
        let mut lines = bmux_terminal_grid::visible_text_lines(grid, 0, grid.height())
            .into_iter()
            .map(|line| line.trim_end().to_string())
            .collect::<Vec<_>>();
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        Some(bmux_pane_runtime_state::PaneScreenText {
            revision: grid.revision(),
            text: lines.join("\n"),
        })
    }

    fn list_pane_processes(&self) -> Vec<bmux_pane_runtime_state::PaneProcessIdentity> {
        self.with_lock_read(|m| {
            let mut identities = Vec::new();
            for (session_id, runtime) in &m.runtimes {
                for (pane_id, pane) in &runtime.panes {
                    identities.push(bmux_pane_runtime_state::PaneProcessIdentity {
                        session_id: *session_id,
                        pane_id: *pane_id,
                        pid: pane.process_id.lock().ok().and_then(|pid| *pid),
                        process_group_id: pane.process_group_id.lock().ok().and_then(|pgid| *pgid),
                    });
                }
            }
            identities
        })
        .unwrap_or_default()
    }

    fn pane_process_identity(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
    ) -> Option<bmux_pane_runtime_state::PaneProcessIdentity> {
        self.with_lock_read(|m| {
            let runtime = m.runtimes.get(&session_id)?;
            let pane = runtime.panes.get(&pane_id)?;
            Some(bmux_pane_runtime_state::PaneProcessIdentity {
                session_id,
                pane_id,
                pid: pane.process_id.lock().ok().and_then(|pid| *pid),
                process_group_id: pane.process_group_id.lock().ok().and_then(|pgid| *pgid),
            })
        })
        .flatten()
    }

    fn write_input(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        data: Vec<u8>,
    ) -> Result<(usize, Uuid), SessionRuntimeError> {
        self.with_lock(|m| m.write_input(session_id, client_id, data))
            .unwrap_or(Err(SessionRuntimeError::Closed))
    }

    fn write_input_to_pane(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        pane_id: Uuid,
        data: Vec<u8>,
    ) -> Result<usize, SessionRuntimeError> {
        let host = &self.host;
        if crate::handlers::attach_commands::observer_identity(host, client_id) {
            return Err(SessionRuntimeError::NotAttached);
        }
        let lookup_started = switchy_time::Instant::now();
        let owner_session_id = self
            .with_lock_read(|m| {
                let scene = m
                    .build_attach_scene_for_client(session_id, client_id)
                    .ok()?;
                let visible_target = scene.surfaces.iter().any(|surface| {
                    surface.visible && surface.accepts_input && surface.pane_id == Some(pane_id)
                });
                visible_target.then(|| m.pane_session_for_attach(session_id, pane_id))?
            })
            .flatten()
            .ok_or(SessionRuntimeError::NotFound)?;
        let pane = self
            .pane_input_index
            .read()
            .ok()
            .and_then(|index| index.get(&pane_id).cloned())
            .filter(|handle| handle.session_id == owner_session_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        let lookup = lookup_started.elapsed();
        if lookup > Duration::from_millis(2) {
            warn!(attach_session_id = %session_id.0, owner_session_id = %owner_session_id.0, %pane_id, lookup_us = lookup.as_micros(), "pane direct input lookup exceeded hot-path budget");
        }

        // Register only after the authorized visible target has been resolved.
        // The admission barrier below remains authoritative for every write.
        if !crate::private_input::register_producer(host, pane_id, client_id.0) {
            return Err(SessionRuntimeError::Closed);
        }
        let bytes = data.len();
        let send_started = switchy_time::Instant::now();
        let recording_data =
            crate::private_input::with_producer_admission(host, pane_id, client_id.0, || {
                crate::private_input::with_capture_barrier(host, pane_id, |capture| {
                    let recording_data = capture
                        .then(|| self.host.pane_input_recording_data(&data))
                        .flatten();
                    pane.send_input(data)?;
                    if let Some((handle, data)) = recording_data {
                        record_pane_input_raw(&handle, data, owner_session_id, pane_id, client_id);
                    }
                    Ok::<(), SessionRuntimeError>(())
                })
            })
            .flatten();
        match recording_data {
            Some(result) => result?,
            None => return Err(SessionRuntimeError::Closed),
        }
        let send = send_started.elapsed();
        if send > Duration::from_millis(2) {
            warn!(attach_session_id = %session_id.0, owner_session_id = %owner_session_id.0, %pane_id, send_us = send.as_micros(), "pane direct input send exceeded hot-path budget");
        }
        Ok(bytes)
    }

    fn read_pane_output_at(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
        cursor: u64,
        max_bytes: usize,
    ) -> Result<bmux_pane_runtime_state::OutputRead, SessionRuntimeError> {
        let output = self
            .with_lock_read_named("read_pane_output_at", |manager| {
                let runtime = manager.runtimes.get(&session_id)?;
                runtime
                    .panes
                    .get(&pane_id)
                    .map(|pane| Arc::clone(&pane.output_buffer))
            })
            .flatten()
            .ok_or(SessionRuntimeError::NotFound)?;
        let read = output.lock().map_err(|_| SessionRuntimeError::Closed)?;
        let retained_start = read.start_offset;
        let source_end = read.end_offset();
        let read = read.read_at(cursor, max_bytes);
        Ok(bmux_pane_runtime_state::OutputRead {
            bytes: read.bytes,
            retained_start,
            stream_start: read.stream_start,
            stream_end: read.stream_end,
            source_end,
            stream_gap: read.stream_gap,
        })
    }

    fn set_client_write_permission(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        allowed: bool,
    ) {
        let host = &self.host;
        if allowed && crate::handlers::attach_commands::observer_identity(host, client_id) {
            return;
        }
        if let Ok(mut permissions) = self.client_write_permissions.write() {
            let writers = permissions.entry(session_id).or_default();
            if allowed {
                writers.insert(client_id);
            } else {
                writers.remove(&client_id);
            }
        }
    }

    fn client_can_write(&self, session_id: SessionId, client_id: ClientId) -> bool {
        let host = &self.host;
        if crate::handlers::attach_commands::observer_identity(host, client_id) {
            return false;
        }
        self.client_write_permissions
            .read()
            .ok()
            .and_then(|permissions| permissions.get(&session_id).cloned())
            .is_some_and(|writers| writers.contains(&client_id))
    }

    fn read_output(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        max_bytes: usize,
    ) -> Result<Vec<u8>, SessionRuntimeError> {
        self.with_lock(|m| m.read_output(session_id, client_id, max_bytes))
            .unwrap_or(Err(SessionRuntimeError::Closed))
    }

    fn read_pane_output_batch(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        pane_ids: &[Uuid],
        max_bytes: usize,
    ) -> Result<Vec<AttachPaneChunk>, SessionRuntimeError> {
        let panes = self
            .with_lock_read_named("read_pane_output_batch.resolve", |m| {
                m.attach_pane_data_refs_for_scene_panes(session_id, client_id, pane_ids)
            })
            .unwrap_or(Err(SessionRuntimeError::Closed))?;
        let num_panes = pane_ids.len().max(1);
        let per_pane_budget = (RESPONSE_OUTPUT_BUDGET / num_panes).min(max_bytes);
        let mut budget_remaining = RESPONSE_OUTPUT_BUDGET;
        let mut chunks = Vec::new();
        for pane in panes {
            let allowed = per_pane_budget.min(budget_remaining);
            let chunk = attach_pane_chunk_for_client(&pane, client_id, allowed)?;
            budget_remaining = budget_remaining.saturating_sub(chunk.data.len());
            chunks.push(chunk);
        }
        Ok(chunks)
    }

    fn attach_pane_output_batch_with_dirty_check(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        pane_ids: &[Uuid],
        max_bytes: usize,
    ) -> (Result<Vec<AttachPaneChunk>, SessionRuntimeError>, bool) {
        let panes = match self
            .with_lock_read_named("attach_pane_output_batch.resolve", |m| {
                m.attach_pane_data_refs_for_scene_panes(session_id, client_id, pane_ids)
            })
            .unwrap_or(Err(SessionRuntimeError::Closed))
        {
            Ok(panes) => panes,
            Err(err) => return (Err(err), false),
        };
        for pane in &panes {
            pane.output_dirty.store(false, Ordering::SeqCst);
        }
        let num_panes = pane_ids.len().max(1);
        let per_pane_budget = (RESPONSE_OUTPUT_BUDGET / num_panes).min(max_bytes);
        let mut budget_remaining = RESPONSE_OUTPUT_BUDGET;
        let mut chunks = Vec::new();
        // Output left unread because a read reached its budget is pending too, not only output
        // that arrived during the reads.
        let mut remainder = false;
        for pane in &panes {
            let allowed = per_pane_budget.min(budget_remaining);
            match attach_pane_chunk_and_remainder_for_client(pane, client_id, allowed) {
                Ok((chunk, more)) => {
                    budget_remaining = budget_remaining.saturating_sub(chunk.data.len());
                    remainder |= more;
                    chunks.push(chunk);
                }
                Err(err) => return (Err(err), false),
            }
        }
        let still_pending = remainder
            || panes
                .iter()
                .any(|pane| pane.output_dirty.load(Ordering::SeqCst));
        (Ok(chunks), still_pending)
    }

    fn attach_pane_image_deltas(
        &self,
        session_id: SessionId,
        pane_ids: &[Uuid],
        since_sequences: &[u64],
        payload_codec: Option<&dyn bmux_ipc::compression::CompressionCodec>,
    ) -> Vec<bmux_attach_image_protocol::AttachPaneImageDelta> {
        let panes = self
            .with_lock_read_named("attach_pane_image_deltas.resolve", |m| {
                m.pane_data_refs_for_images(session_id, pane_ids)
            })
            .unwrap_or_default();
        let pane_by_id = panes
            .into_iter()
            .map(|pane| (pane.pane_id, pane))
            .collect::<BTreeMap<_, _>>();
        let mut result = Vec::new();
        for (i, pane_id) in pane_ids.iter().enumerate() {
            let Some(pane) = pane_by_id.get(pane_id) else {
                continue;
            };
            #[cfg(feature = "image-registry")]
            pane.image_dirty.store(false, Ordering::SeqCst);
            let since = since_sequences.get(i).copied().unwrap_or(0);
            #[cfg(feature = "image-registry")]
            if let Ok(reg) = pane.image_registry.lock() {
                let delta = reg.delta_since(since);
                result.push(delta.to_ipc(*pane_id, payload_codec));
            }
            #[cfg(not(feature = "image-registry"))]
            {
                let _ = (pane, since, payload_codec);
                result.push(bmux_attach_image_protocol::AttachPaneImageDelta {
                    pane_id: *pane_id,
                    added: Vec::new(),
                    removed: Vec::new(),
                    sequence: 0,
                });
            }
        }
        result
    }

    fn begin_attach(
        &self,
        session_id: SessionId,
        client_id: ClientId,
    ) -> Result<(), SessionRuntimeError> {
        self.with_lock(|m| m.begin_attach(session_id, client_id))
            .unwrap_or(Err(SessionRuntimeError::Closed))
    }

    fn end_attach(&self, session_id: SessionId, client_id: ClientId) {
        let _ = self.with_lock(|m| m.end_attach(session_id, client_id));
    }

    fn set_attach_viewport(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        cols: u16,
        rows: u16,
        top_inset: u16,
        right_inset: u16,
        bottom_inset: u16,
        left_inset: u16,
        cell_pixel_width: u16,
        cell_pixel_height: u16,
    ) -> Result<(u16, u16, u16, u16, u16, u16), SessionRuntimeError> {
        self.with_lock(|m| {
            m.set_attach_viewport(
                session_id,
                client_id,
                cols,
                rows,
                top_inset,
                right_inset,
                bottom_inset,
                left_inset,
                cell_pixel_width,
                cell_pixel_height,
            )
        })
        .unwrap_or(Err(SessionRuntimeError::Closed))
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "implements the trait's atomic attach-retarget operation with viewport dimensions"
    )]
    fn retarget_attach_stream(
        &self,
        previous_session_id: Option<SessionId>,
        next_session_id: SessionId,
        client_id: ClientId,
        cols: u16,
        rows: u16,
        top_inset: u16,
        right_inset: u16,
        bottom_inset: u16,
        left_inset: u16,
        cell_pixel_width: u16,
        cell_pixel_height: u16,
    ) -> Result<(u16, u16, u16, u16, u16, u16), SessionRuntimeError> {
        self.with_lock(|m| {
            m.begin_attach(next_session_id, client_id)?;
            let viewport = match m.set_attach_viewport(
                next_session_id,
                client_id,
                cols,
                rows,
                top_inset,
                right_inset,
                bottom_inset,
                left_inset,
                cell_pixel_width,
                cell_pixel_height,
            ) {
                Ok(viewport) => viewport,
                Err(error) => {
                    if previous_session_id != Some(next_session_id) {
                        m.end_attach(next_session_id, client_id);
                    }
                    return Err(error);
                }
            };
            if let Some(previous_session_id) = previous_session_id
                && previous_session_id != next_session_id
            {
                m.end_attach(previous_session_id, client_id);
            }
            Ok(viewport)
        })
        .unwrap_or(Err(SessionRuntimeError::Closed))
    }

    fn apply_stored_attach_viewport(&self, session_id: SessionId) {
        let _ = self.with_lock(|m| m.apply_stored_attach_viewport(session_id));
    }

    fn attach_layout_state(
        &self,
        session_id: SessionId,
        client_id: ClientId,
    ) -> Result<bmux_pane_runtime_state::AttachLayoutState, SessionRuntimeError> {
        let snapshot = self
            .with_lock_read_named("attach_layout_state.snapshot", |m| {
                m.attach_scene_data_snapshot(session_id, client_id)
            })
            .unwrap_or(Err(SessionRuntimeError::Closed))?;
        let panes = snapshot
            .panes
            .iter()
            .enumerate()
            .map(|(index, pane)| pane_summary_for_data_ref(index, snapshot.focused_pane_id, pane))
            .collect();
        Ok(bmux_pane_runtime_state::AttachLayoutState {
            focused_pane_id: snapshot.focused_pane_id,
            panes,
            layout_root: snapshot.layout_root,
            scene: snapshot.scene,
            zoomed: snapshot.zoomed,
        })
    }

    fn attach_snapshot_state(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        max_bytes_per_pane: usize,
    ) -> Result<bmux_pane_runtime_state::AttachSnapshotState, SessionRuntimeError> {
        let snapshot = self
            .with_lock_read_named("attach_snapshot_state.snapshot", |m| {
                m.attach_scene_data_snapshot(session_id, client_id)
            })
            .unwrap_or(Err(SessionRuntimeError::Closed))?;
        let panes = snapshot
            .panes
            .iter()
            .enumerate()
            .map(|(index, pane)| pane_summary_for_data_ref(index, snapshot.focused_pane_id, pane))
            .collect::<Vec<_>>();
        let mut chunks = Vec::new();
        let mut pane_mouse_protocols = Vec::new();
        let mut pane_input_modes = Vec::new();
        let num_panes = snapshot.panes.len().max(1);
        let per_pane_budget = (RESPONSE_OUTPUT_BUDGET / num_panes).min(max_bytes_per_pane);
        let mut budget_remaining = RESPONSE_OUTPUT_BUDGET;
        for pane in &snapshot.panes {
            let protocol = pane
                .mouse_protocol_state
                .lock()
                .map(|state| *state)
                .unwrap_or_default();
            pane_mouse_protocols.push(AttachPaneMouseProtocol {
                pane_id: pane.pane_id,
                protocol,
            });
            let mode = pane
                .input_mode_state
                .lock()
                .map(|state| *state)
                .unwrap_or_default();
            pane_input_modes.push(AttachPaneInputMode {
                pane_id: pane.pane_id,
                mode,
            });
            let allowed = per_pane_budget.min(budget_remaining);
            let chunk = attach_pane_recent_chunk(pane, client_id, allowed)?;
            budget_remaining = budget_remaining.saturating_sub(chunk.data.len());
            chunks.push(chunk);
        }
        Ok(bmux_pane_runtime_state::AttachSnapshotState {
            focused_pane_id: snapshot.focused_pane_id,
            panes,
            layout_root: snapshot.layout_root,
            scene: snapshot.scene,
            zoomed: snapshot.zoomed,
            chunks,
            pane_mouse_protocols,
            pane_input_modes,
        })
    }

    fn attach_pane_snapshot_state(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        pane_ids: &[Uuid],
        max_bytes_per_pane: usize,
    ) -> Result<bmux_pane_runtime_state::AttachPaneSnapshotState, SessionRuntimeError> {
        let panes = self
            .with_lock_read_named("attach_pane_snapshot_state.resolve", |m| {
                m.attach_pane_data_refs_for_scene_panes(session_id, client_id, pane_ids)
            })
            .unwrap_or(Err(SessionRuntimeError::Closed))?;
        let num_panes = pane_ids.len().max(1);
        let per_pane_budget = (RESPONSE_OUTPUT_BUDGET / num_panes).min(max_bytes_per_pane);
        let mut budget_remaining = RESPONSE_OUTPUT_BUDGET;
        let mut chunks = Vec::new();
        let mut pane_mouse_protocols = Vec::new();
        let mut pane_input_modes = Vec::new();
        let mut seen = BTreeSet::new();
        for pane in panes {
            if !seen.insert(pane.pane_id) {
                continue;
            }
            let protocol = pane
                .mouse_protocol_state
                .lock()
                .map(|state| *state)
                .unwrap_or_default();
            pane_mouse_protocols.push(AttachPaneMouseProtocol {
                pane_id: pane.pane_id,
                protocol,
            });
            let mode = pane
                .input_mode_state
                .lock()
                .map(|state| *state)
                .unwrap_or_default();
            pane_input_modes.push(AttachPaneInputMode {
                pane_id: pane.pane_id,
                mode,
            });
            let allowed = per_pane_budget.min(budget_remaining);
            let chunk = attach_pane_recent_chunk(&pane, client_id, allowed)?;
            budget_remaining = budget_remaining.saturating_sub(chunk.data.len());
            chunks.push(chunk);
        }
        Ok(bmux_pane_runtime_state::AttachPaneSnapshotState {
            chunks,
            pane_mouse_protocols,
            pane_input_modes,
        })
    }

    fn attach_grid_snapshot_state(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        pane_ids: &[Uuid],
        max_rows_per_pane: usize,
    ) -> Result<bmux_pane_runtime_state::AttachGridSnapshotState, SessionRuntimeError> {
        // Reserve both the projected snapshot and its coexisting encoded output.
        // The guard is declared first so construction temporaries drop before it.
        // Returned response storage belongs to the caller, not this working set.
        let _working_set = self.reserve_grid_working_set()?;
        let panes = self
            .with_lock_read_named("attach_grid_snapshot_state.resolve", |m| {
                m.attach_pane_data_refs_for_session_panes(session_id, client_id, pane_ids)
            })
            .unwrap_or(Err(SessionRuntimeError::Closed))?;
        let mut snapshots = Vec::new();
        let mut remaining = RESPONSE_OUTPUT_BUDGET;
        let mut cursor_updates = Vec::new();
        let mut wire = REPLY_WIRE_BUDGET;
        let mut seen = BTreeSet::new();
        let mut omitted = false;
        for pane in panes {
            if !seen.insert(pane.pane_id) {
                continue;
            }
            let grid = pane
                .terminal_grid
                .lock()
                .map_err(|_| SessionRuntimeError::Closed)?;
            // Panes that no longer fit the shared reply budget are omitted, as
            // missing panes are: callers hydrate what is returned and leave the
            // rest to live state, instead of losing every pane to one large one.
            // Admit selected aggregate source text before projection clones it.
            if !grid
                .grid()
                .snapshot_text_fits(0, max_rows_per_pane, remaining)
            {
                omitted = true;
                warn_grid_snapshot_omitted(pane.pane_id, remaining);
                continue;
            }
            let Some(snapshot) = grid.try_snapshot(0, max_rows_per_pane, remaining) else {
                omitted = true;
                warn_grid_snapshot_omitted(pane.pane_id, remaining);
                continue;
            };
            let stream_end = pane
                .output_buffer
                .lock()
                .map_err(|_| SessionRuntimeError::Closed)?
                .end_offset();
            drop(grid);
            let encoded = match encode_grid_window(&snapshot, &mut wire) {
                Ok(encoded) => encoded,
                Err(SessionRuntimeError::ResponseBudgetExceeded) => {
                    omitted = true;
                    warn_grid_snapshot_omitted(pane.pane_id, wire);
                    continue;
                }
                Err(error) => return Err(error),
            };
            // Encoded output coexists with later projections.
            remaining = remaining.saturating_sub(encoded.len());
            cursor_updates.push((pane.pane_id, Arc::clone(&pane.output_buffer), stream_end));
            snapshots.push(bmux_pane_runtime_state::AttachPaneGridSnapshot {
                pane_id: pane.pane_id,
                stream_end,
                encoded,
            });
        }
        // Nothing fit: report the budget instead of an empty success that is
        // indistinguishable from every requested pane being absent.
        if snapshots.is_empty() && omitted {
            return Err(SessionRuntimeError::ResponseBudgetExceeded);
        }
        acknowledge_grid_snapshots(client_id, &mut cursor_updates)?;
        Ok(bmux_pane_runtime_state::AttachGridSnapshotState { snapshots })
    }

    fn attach_grid_window_state(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        windows: &[bmux_pane_runtime_state::AttachPaneGridWindowRequest],
    ) -> Result<bmux_pane_runtime_state::AttachGridWindowState, SessionRuntimeError> {
        // Share construction admission with snapshots and retained captures.
        let _working_set = self.reserve_grid_working_set()?;
        let pane_ids = windows
            .iter()
            .map(|window| window.pane_id)
            .collect::<Vec<_>>();
        let panes = self
            .with_lock_read_named("attach_grid_window_state.resolve", |m| {
                m.attach_pane_data_refs_for_scene_panes(session_id, client_id, &pane_ids)
            })
            .unwrap_or(Err(SessionRuntimeError::Closed))?;
        let pane_by_id = panes
            .into_iter()
            .map(|pane| (pane.pane_id, pane))
            .collect::<BTreeMap<_, _>>();
        let mut snapshots = Vec::new();
        let mut remaining = RESPONSE_OUTPUT_BUDGET;
        for window in windows {
            let Some(pane) = pane_by_id.get(&window.pane_id) else {
                continue;
            };
            let pin = window.pin_id.map_or(Ok(None), |pin_id| {
                self.with_lock_read_named("attach_grid_window_state.pin", |manager| {
                    let runtime = manager
                        .runtimes
                        .get(&session_id)
                        .ok_or(SessionRuntimeError::NotFound)?;
                    let pin = runtime
                        .scrollback_pins
                        .get(&(client_id, window.pane_id))
                        .filter(|pin| pin.id == pin_id)
                        .cloned()
                        .ok_or(SessionRuntimeError::NotFound)?;
                    Ok(Some(pin))
                })
                .unwrap_or(Err(SessionRuntimeError::Closed))
            })?;
            let (
                snapshot,
                max_scrollback_offset,
                total_scrolled_rows,
                adjusted_offset,
                desired_offset,
                stream_end,
            ) = if let Some(pin) = pin {
                // A frozen pin is immutable: use the requested offset directly
                // (bounded only by the captured history), never apply live
                // anchor growth or live-history clamping.
                let max_offset = pin.grid.grid.max_scrollback_offset();
                let offset = window.scrollback_offset.min(max_offset);
                (
                    bmux_terminal_grid::GridSnapshot::try_from_grid(
                        &pin.grid.grid,
                        offset,
                        window.rows,
                        remaining,
                    )
                    .ok_or(SessionRuntimeError::ResponseBudgetExceeded)?,
                    max_offset,
                    pin.total_scrolled_rows,
                    offset,
                    window.scrollback_offset,
                    pin.stream_end,
                )
            } else {
                let grid = pane
                    .terminal_grid
                    .lock()
                    .map_err(|_| SessionRuntimeError::Closed)?;
                let total_scrolled_rows = grid.grid().total_scrolled_rows();
                let max_scrollback_offset = grid.grid().max_scrollback_offset();
                let anchor_growth = window
                    .anchor_total_scrolled_rows
                    .map_or(0, |anchor| total_scrolled_rows.saturating_sub(anchor));
                let desired_offset = window
                    .scrollback_offset
                    .saturating_add(usize::try_from(anchor_growth).unwrap_or(usize::MAX));
                let adjusted_offset = desired_offset.min(max_scrollback_offset);
                (
                    grid.try_snapshot(adjusted_offset, window.rows, remaining)
                        .ok_or(SessionRuntimeError::ResponseBudgetExceeded)?,
                    max_scrollback_offset,
                    total_scrolled_rows,
                    adjusted_offset,
                    desired_offset,
                    pane.output_buffer
                        .lock()
                        .map_err(|_| SessionRuntimeError::Closed)?
                        .end_offset(),
                )
            };
            let encoded = encode_grid_window(&snapshot, &mut remaining)?;
            snapshots.push(bmux_pane_runtime_state::AttachPaneGridWindow {
                pane_id: window.pane_id,
                scrollback_offset: adjusted_offset,
                max_scrollback_offset,
                total_scrolled_rows,
                anchor_delta_rows: adjusted_offset.saturating_sub(window.scrollback_offset),
                anchor_clamped: adjusted_offset < desired_offset,
                stream_end,
                encoded,
            });
        }
        Ok(bmux_pane_runtime_state::AttachGridWindowState { windows: snapshots })
    }

    fn attach_history_slice(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        pane_id: Uuid,
        pin_id: (u64, Uuid),
        position: (usize, usize),
        limits: (usize, usize),
    ) -> Result<
        (Vec<u8>, usize, bmux_pane_runtime_state::HistoryEnd),
        bmux_pane_runtime_state::HistoryFetchError,
    > {
        use bmux_pane_runtime_state::{HistoryEnd, HistoryFetchError};
        let _working = self.reserve_grid_working_set()?;
        let pin = self
            .with_lock_read_named("attach_history_slice", |manager| {
                let runtime = manager
                    .runtimes
                    .get(&session_id)
                    .ok_or(SessionRuntimeError::NotFound)?;
                if !runtime.attached_clients.contains(&client_id) {
                    return Err(HistoryFetchError::Runtime(SessionRuntimeError::NotAttached));
                }
                let pin = runtime
                    .scrollback_pins
                    .get(&(client_id, pane_id))
                    .ok_or(HistoryFetchError::Unavailable)?;
                if pin.id != pin_id.0 || pin.capture_id != pin_id.1 {
                    return Err(HistoryFetchError::StaleCapture);
                }
                Ok(pin.clone())
            })
            .unwrap_or(Err(HistoryFetchError::Runtime(SessionRuntimeError::Closed)))?;
        let grid = &pin.grid.grid;
        let slice = grid
            .history_slice(
                grid.revision(),
                position.0,
                position.1,
                limits.0.min(4096),
                limits.1.min(64 * 1024),
            )
            .map_err(|error| match error {
                bmux_terminal_grid::HistorySliceError::BudgetExhausted => {
                    HistoryFetchError::BudgetExhausted
                }
                bmux_terminal_grid::HistorySliceError::InvalidOffset => {
                    HistoryFetchError::InvalidOffset
                }
                bmux_terminal_grid::HistorySliceError::Unavailable => {
                    HistoryFetchError::Unavailable
                }
                bmux_terminal_grid::HistorySliceError::StaleRevision => {
                    HistoryFetchError::StaleCapture
                }
            })?;
        let cells = slice
            .cells
            .iter()
            .map(|cell| (cell.text(), cell.width(), grid.palette().get(cell.style())))
            .collect::<Vec<_>>();
        let encoded = serde_json::to_vec(&cells).map_err(|_| SessionRuntimeError::Closed)?;
        if encoded.len() > 128 * 1024 {
            return Err(HistoryFetchError::BudgetExhausted);
        }
        let end = match slice.end {
            bmux_terminal_grid::HistorySliceEnd::Continue => HistoryEnd::Continue,
            bmux_terminal_grid::HistorySliceEnd::HardBreak => HistoryEnd::HardBreak,
            bmux_terminal_grid::HistorySliceEnd::Open => HistoryEnd::Open,
        };
        Ok((encoded, slice.next_cell_offset, end))
    }

    fn attach_main_row_slice(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        pane_id: Uuid,
        pin_id: (u64, Uuid),
        position: (usize, usize),
        limits: (usize, usize),
    ) -> Result<
        (Vec<u8>, usize, bmux_pane_runtime_state::HistoryEnd),
        bmux_pane_runtime_state::HistoryFetchError,
    > {
        use bmux_pane_runtime_state::{HistoryEnd, HistoryFetchError};
        let _working = self.reserve_grid_working_set()?;
        let pin = self
            .with_lock_read_named("attach_main_row_slice", |manager| {
                let runtime = manager
                    .runtimes
                    .get(&session_id)
                    .ok_or(SessionRuntimeError::NotFound)?;
                if !runtime.attached_clients.contains(&client_id) {
                    return Err(HistoryFetchError::Runtime(SessionRuntimeError::NotAttached));
                }
                let pin = runtime
                    .scrollback_pins
                    .get(&(client_id, pane_id))
                    .ok_or(HistoryFetchError::Unavailable)?;
                if pin.id != pin_id.0 || pin.capture_id != pin_id.1 {
                    return Err(HistoryFetchError::StaleCapture);
                }
                Ok(pin.clone())
            })
            .unwrap_or(Err(HistoryFetchError::Runtime(SessionRuntimeError::Closed)))?;
        let grid = &pin.grid.grid;
        let slice = grid
            .main_row_slice(
                grid.revision(),
                position.0,
                position.1,
                limits.0.min(4096),
                limits.1.min(64 * 1024),
            )
            .map_err(|error| match error {
                bmux_terminal_grid::HistorySliceError::BudgetExhausted => {
                    HistoryFetchError::BudgetExhausted
                }
                bmux_terminal_grid::HistorySliceError::InvalidOffset => {
                    HistoryFetchError::InvalidOffset
                }
                bmux_terminal_grid::HistorySliceError::Unavailable => {
                    HistoryFetchError::Unavailable
                }
                bmux_terminal_grid::HistorySliceError::StaleRevision => {
                    HistoryFetchError::StaleCapture
                }
            })?;
        let cells = slice
            .cells
            .iter()
            .enumerate()
            .filter(|(_, cell)| !cell.is_wide_continuation())
            .map(|(index, cell)| {
                // A width-two glyph clipped at the physical row edge occupies
                // one column. Match the slice's physical continuation offset.
                let width = if cell.width() == 2
                    && !slice
                        .cells
                        .get(index + 1)
                        .is_some_and(bmux_terminal_grid::Cell::is_wide_continuation)
                {
                    1
                } else {
                    cell.width()
                };
                (cell.text(), width, grid.palette().get(cell.style()))
            })
            .collect::<Vec<_>>();
        let encoded = serde_json::to_vec(&cells).map_err(|_| SessionRuntimeError::Closed)?;
        if encoded.len() > 128 * 1024 {
            return Err(HistoryFetchError::BudgetExhausted);
        }
        let end = match slice.end {
            bmux_terminal_grid::HistorySliceEnd::Continue => HistoryEnd::Continue,
            bmux_terminal_grid::HistorySliceEnd::HardBreak => HistoryEnd::HardBreak,
            bmux_terminal_grid::HistorySliceEnd::Open => HistoryEnd::Open,
        };
        Ok((encoded, slice.next_cell_offset, end))
    }

    #[allow(
        clippy::too_many_lines,
        reason = "capture admission and publication stay together to make lock and reservation lifetimes reviewable"
    )]
    fn attach_scrollback_pin(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        pane_id: Uuid,
    ) -> Result<bmux_pane_runtime_state::AttachPaneScrollbackPin, SessionRuntimeError> {
        let pane = self
            .with_lock_read_named("attach_scrollback_pin.resolve", |manager| {
                manager
                    .attach_pane_data_refs_for_session_panes(session_id, client_id, &[pane_id])
                    .and_then(|mut panes| panes.pop().ok_or(SessionRuntimeError::NotFound))
            })
            .unwrap_or(Err(SessionRuntimeError::Closed))?;
        let reservation =
            PinReservation::acquire(&self.retained_pin_bytes, RESPONSE_OUTPUT_BUDGET)?;
        let mut capture_budget = RESPONSE_OUTPUT_BUDGET;
        // Same order as the PTY writer: image state, grid, then output.
        #[cfg(feature = "image-registry")]
        let image_guard = pane
            .image_registry
            .lock()
            .map_err(|_| SessionRuntimeError::Closed)?;
        #[cfg(feature = "image-registry")]
        let image_budget_before = capture_budget;
        #[cfg(feature = "image-registry")]
        let captured_images = image_guard
            .capture_history(&mut capture_budget)
            .map_err(|_| SessionRuntimeError::ResponseBudgetExceeded)?;
        #[cfg(feature = "image-registry")]
        let image_bytes = image_budget_before - capture_budget;
        let (captured_grid, stream_end) = {
            let grid = pane
                .terminal_grid
                .lock()
                .map_err(|_| SessionRuntimeError::Closed)?;
            let captured_grid = grid
                .grid()
                .try_clone_charged(&mut capture_budget)
                .ok_or(SessionRuntimeError::ResponseBudgetExceeded)?;
            let stream_end = pane
                .output_buffer
                .lock()
                .map_err(|_| SessionRuntimeError::Closed)?
                .end_offset();
            (captured_grid, stream_end)
        };
        #[cfg(feature = "image-registry")]
        drop(image_guard);
        let width =
            u16::try_from(captured_grid.width()).map_err(|_| SessionRuntimeError::Closed)?;
        let height =
            u16::try_from(captured_grid.height()).map_err(|_| SessionRuntimeError::Closed)?;
        let history_truncated = captured_grid.history_truncated();
        let history_line_count = captured_grid.history_line_count();
        let total_scrolled_rows = captured_grid.total_scrolled_rows();
        let max_scrollback_offset = captured_grid.max_scrollback_offset();
        let full_snapshot = bmux_terminal_grid::GridSnapshot::try_from_grid(
            &captured_grid,
            0,
            captured_grid.main_row_count(),
            capture_budget,
        )
        .ok_or(SessionRuntimeError::ResponseBudgetExceeded)?;
        let mut encoded_count = EncodedByteCount(0, RESPONSE_OUTPUT_BUDGET);
        serde_json::to_writer(&mut encoded_count, &full_snapshot)
            .map_err(|_| SessionRuntimeError::ResponseBudgetExceeded)?;
        let encoded_bytes = encoded_count.0;
        drop(full_snapshot);
        // Couple storage and charge before entering the publication closure: its
        // early returns and lock failures must free storage before admission.
        #[cfg(not(feature = "image-registry"))]
        let captured = Arc::new(CapturedPinGrid::new(captured_grid, reservation)?);
        #[cfg(feature = "image-registry")]
        let captured = {
            let bytes = captured_grid
                .retained_capacity_bytes()
                .and_then(|bytes| bytes.checked_add(image_bytes))
                .ok_or(SessionRuntimeError::ResponseBudgetExceeded)?;
            let mut reservation = reservation;
            reservation.shrink(bytes)?;
            Arc::new(CapturedPinGrid {
                grid: captured_grid,
                images: Some(captured_images),
                reservation,
            })
        };
        self.with_lock_named("attach_scrollback_pin.store", |manager| {
            admit_retained_pin(manager, session_id, client_id, pane_id, &captured.grid)?;
            let runtime = manager
                .runtimes
                .get_mut(&session_id)
                .ok_or(SessionRuntimeError::NotFound)?;
            if !runtime.attached_clients.contains(&client_id) {
                return Err(SessionRuntimeError::NotAttached);
            }
            // Capture runs outside the manager lock. Removal, transfer, or
            // replacement must not let an old capture recreate a retired pin.
            let current_pane = runtime
                .panes
                .get(&pane_id)
                .ok_or(SessionRuntimeError::NotFound)?;
            if !Arc::ptr_eq(&current_pane.terminal_grid, &pane.terminal_grid) {
                return Err(SessionRuntimeError::NotFound);
            }
            let capture_id = crate::ids::new_uuid();
            let pin_id = runtime.next_scrollback_pin_id;
            // Never reuse a token: a delayed unpin must not release a newer capture.
            runtime.next_scrollback_pin_id =
                pin_id.checked_add(1).ok_or(SessionRuntimeError::Closed)?;
            runtime.scrollback_pins.insert(
                (client_id, pane_id),
                ScrollbackPin {
                    capture_id,
                    id: pin_id,
                    grid: captured,
                    total_scrolled_rows,
                    stream_end,
                    encoded_bytes,
                },
            );
            let client_pinned_bytes = runtime
                .scrollback_pins
                .iter()
                .filter(|((pin_client_id, _), _)| *pin_client_id == client_id)
                .map(|(_, pin)| pin.encoded_bytes)
                .sum::<usize>();
            trace!(
                %pane_id,
                client_id = %client_id.0,
                pin_id,
                encoded_bytes,
                client_pinned_bytes,
                "captured frozen scrollback pin"
            );
            Ok(bmux_pane_runtime_state::AttachPaneScrollbackPin {
                width,
                height,
                history_truncated,
                history_line_count,
                capture_id,
                pane_id,
                pin_id,
                total_scrolled_rows,
                max_scrollback_offset,
                stream_end,
            })
        })
        .unwrap_or(Err(SessionRuntimeError::Closed))
    }

    fn attach_scrollback_unpin(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        pane_id: Uuid,
        pin_id: u64,
    ) -> Result<bmux_pane_runtime_state::AttachPaneScrollbackUnpinAck, SessionRuntimeError> {
        self.with_lock_named("attach_scrollback_unpin", |manager| {
            let runtime = manager
                .runtimes
                .get_mut(&session_id)
                .ok_or(SessionRuntimeError::NotFound)?;
            if !runtime.attached_clients.contains(&client_id) {
                return Err(SessionRuntimeError::NotAttached);
            }
            let removed_pin = runtime
                .scrollback_pins
                .get(&(client_id, pane_id))
                .filter(|pin| pin.id == pin_id)
                .map(|pin| pin.encoded_bytes);
            let released = removed_pin.is_some()
                && runtime
                    .scrollback_pins
                    .remove(&(client_id, pane_id))
                    .is_some();
            trace!(
                %pane_id,
                client_id = %client_id.0,
                pin_id,
                encoded_bytes = removed_pin,
                released,
                "released frozen scrollback pin"
            );
            Ok(bmux_pane_runtime_state::AttachPaneScrollbackUnpinAck {
                pane_id,
                pin_id,
                released,
            })
        })
        .unwrap_or(Err(SessionRuntimeError::Closed))
    }

    fn attach_pane_frames(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        pane_ids: &[Uuid],
        max_history_lines: usize,
    ) -> Result<(Vec<bmux_pane_runtime_state::AttachPaneFrame>, bool), SessionRuntimeError> {
        let panes = self
            .with_lock_read_named("attach_pane_frames.resolve", |m| {
                m.attach_pane_data_refs_for_session_panes(session_id, client_id, pane_ids)
            })
            .unwrap_or(Err(SessionRuntimeError::Closed))?;
        frames_for_panes(&panes, client_id, max_history_lines)
    }

    fn observe_pane_frames(
        &self,
        client_id: ClientId,
        targets: &[(SessionId, Uuid)],
        max_history_lines: usize,
    ) -> Result<(Vec<bmux_pane_runtime_state::AttachPaneFrame>, bool), SessionRuntimeError> {
        let panes = self
            .with_lock_read_named("observe_pane_frames.resolve", |m| {
                m.pane_data_refs_for_targets(targets)
            })
            .ok_or(SessionRuntimeError::Closed)?;
        frames_for_panes(&panes, client_id, max_history_lines)
    }

    fn reset_observed_pane_frames(
        &self,
        client_id: ClientId,
        targets: &[(SessionId, Uuid)],
    ) -> Result<(), SessionRuntimeError> {
        let panes = self
            .with_lock_read_named("reset_observed_pane_frames.resolve", |m| {
                m.pane_data_refs_for_targets(targets)
            })
            .ok_or(SessionRuntimeError::Closed)?;
        for pane in panes {
            pane.frame_replicas
                .lock()
                .map_err(|_| SessionRuntimeError::Closed)?
                .remove(&client_id);
        }
        Ok(())
    }

    fn reset_attach_pane_frames(
        &self,
        session_id: SessionId,
        client_id: ClientId,
        pane_ids: &[Uuid],
    ) -> Result<(), SessionRuntimeError> {
        let panes = self
            .with_lock_read_named("reset_attach_pane_frames.resolve", |m| {
                m.attach_pane_data_refs_for_session_panes(session_id, client_id, pane_ids)
            })
            .unwrap_or(Err(SessionRuntimeError::Closed))?;
        for pane in panes {
            pane.frame_replicas
                .lock()
                .map_err(|_| SessionRuntimeError::Closed)?
                .remove(&client_id);
        }
        Ok(())
    }

    fn pane_state(&self, session_id: SessionId, pane_id: Uuid) -> Option<PaneState> {
        self.with_lock_read(|m| {
            m.runtimes
                .get(&session_id)
                .and_then(|r| r.panes.get(&pane_id))
                .map(pane_state_for_handle)
        })
        .flatten()
    }

    fn pane_state_reason(&self, session_id: SessionId, pane_id: Uuid) -> Option<String> {
        self.with_lock_read(|m| {
            m.runtimes
                .get(&session_id)
                .and_then(|r| r.panes.get(&pane_id))
                .and_then(pane_state_reason_for_handle)
        })
        .flatten()
    }

    fn clear_output_dirty(&self, session_id: SessionId, pane_id: Uuid) {
        if let Some(pane) = self
            .with_lock_read_named("clear_output_dirty.resolve", |m| {
                m.runtimes
                    .get(&session_id)
                    .and_then(|r| r.panes.get(&pane_id))
                    .map(|pane| pane_attach_data_ref(session_id, pane_id, pane))
            })
            .flatten()
        {
            pane.output_dirty.store(false, Ordering::SeqCst);
        }
    }

    fn clear_image_dirty(&self, session_id: SessionId, pane_id: Uuid) {
        #[cfg(feature = "image-registry")]
        if let Some(pane) = self
            .with_lock_read_named("clear_image_dirty.resolve", |m| {
                m.runtimes
                    .get(&session_id)
                    .and_then(|r| r.panes.get(&pane_id))
                    .map(|pane| pane_attach_data_ref(session_id, pane_id, pane))
            })
            .flatten()
        {
            pane.image_dirty.store(false, Ordering::SeqCst);
        }
        #[cfg(not(feature = "image-registry"))]
        {
            let _ = (session_id, pane_id);
        }
    }

    fn client_is_attached(&self, session_id: SessionId, client_id: ClientId) -> bool {
        self.with_lock_read(|m| {
            m.runtimes
                .get(&session_id)
                .is_some_and(|r| r.attached_clients.contains(&client_id))
        })
        .unwrap_or(false)
    }

    fn pane_output_has_pending(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
        _client_id: ClientId,
    ) -> bool {
        self.with_lock_read_named("pane_output_has_pending.resolve", |m| {
            m.runtimes
                .get(&session_id)
                .and_then(|r| r.panes.get(&pane_id))
                .map(|pane| Arc::clone(&pane.output_dirty))
        })
        .flatten()
        .is_some_and(|dirty| dirty.load(Ordering::SeqCst))
    }

    fn session_has_stored_viewport(&self, session_id: SessionId) -> bool {
        self.with_lock_read(|m| {
            m.runtimes
                .get(&session_id)
                .is_some_and(|r| r.attach_viewport.is_some())
        })
        .unwrap_or(false)
    }

    fn snapshot_session_runtime(
        &self,
        session_id: SessionId,
    ) -> Option<bmux_pane_runtime_state::SessionRuntimeSnapshot> {
        self.with_lock_read(|m| {
            let runtime = m.runtimes.get(&session_id)?;
            let panes = runtime
                .panes
                .values()
                .map(|p| p.meta.clone())
                .collect::<Vec<_>>();
            Some(bmux_pane_runtime_state::SessionRuntimeSnapshot {
                session_id,
                panes,
                focused_pane_id: runtime.focused_pane_id,
                zoomed_pane_id: runtime.zoomed_pane_id,
                layout_root: runtime.layout_root.clone(),
                floating_surfaces: runtime.floating_surfaces.clone(),
                attached_clients: runtime.attached_clients.clone(),
                attach_viewport: runtime.attach_viewport,
            })
        })
        .flatten()
    }

    fn snapshot_session_runtime_for_persistence(
        &self,
        session_id: SessionId,
    ) -> anyhow::Result<Option<bmux_pane_runtime_state::SessionRuntimeSnapshot>> {
        let Some(parts) = self
            .with_lock_read_named("snapshot_session_runtime_for_persistence.resolve", |m| {
                m.persistence_snapshot_parts(session_id)
            })
            .unwrap_or_else(|| Err(anyhow::anyhow!("session runtime manager lock poisoned")))?
        else {
            return Ok(None);
        };

        // One observation budget for the whole session, not one unbounded
        // process-table scan per pane. Missing observations preserve metadata.
        let inspection_deadline = Instant::now() + Duration::from_millis(250);
        let mut panes = Vec::with_capacity(parts.panes.len());
        for pane in parts.panes {
            let process_id = pane.process_id.lock().ok().and_then(|v| *v);
            let process_group_id = pane.process_group_id.lock().ok().and_then(|v| *v);
            let version = {
                let state = pane
                    .resurrection_state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("resurrection state lock poisoned"))?;
                (!state.frozen
                    && !pane.exited.load(Ordering::SeqCst)
                    && state.active_command_source != Some(PaneCommandSource::Verbatim)
                    && pane.meta.launch.is_none())
                .then(|| state.inspection_version())
            };
            let inspection = version.as_ref().and_then(|_| {
                let budget = inspection_deadline.saturating_duration_since(Instant::now());
                if budget.is_zero() {
                    return None;
                }
                let shell = pane.meta.shell.clone();
                let expected_identity = pane.process_start_identity.lock().ok().and_then(|v| *v);
                bounded_directory_probe(directory_probes(), budget, move || {
                    let pid = process_id?;
                    let identity = process_start_identity_for_pid(pid)?;
                    if expected_identity
                        .as_ref()
                        .is_some_and(|expected| *expected != identity)
                    {
                        return None;
                    }
                    let observed =
                        inspect_process_group_command_and_cwd(process_group_id, process_id, &shell);
                    (process_start_identity_for_pid(pid).as_ref() == Some(&identity))
                        .then_some(observed)
                        .flatten()
                })
                .ok()
                .flatten()
            });
            let same_process = pane.process_id.lock().ok().and_then(|value| *value) == process_id
                && pane.process_group_id.lock().ok().and_then(|value| *value) == process_group_id;
            let mut state = pane
                .resurrection_state
                .lock()
                .map_err(|_| anyhow::anyhow!("resurrection state lock poisoned"))?;
            if let Some(version) = version
                && !pane.exited.load(Ordering::SeqCst)
                && same_process
            {
                state.apply_inspection(&version, inspection);
            }
            let mut meta = pane.meta;
            meta.resurrection = state.to_snapshot();
            panes.push(meta);
        }

        Ok(Some(bmux_pane_runtime_state::SessionRuntimeSnapshot {
            session_id,
            panes,
            focused_pane_id: parts.focused_pane_id,
            zoomed_pane_id: parts.zoomed_pane_id,
            layout_root: parts.layout_root,
            floating_surfaces: parts.floating_surfaces,
            attached_clients: parts.attached_clients,
            attach_viewport: parts.attach_viewport,
        }))
    }

    fn list_session_ids(&self) -> Vec<SessionId> {
        self.with_lock_read(|m| m.runtimes.keys().copied().collect())
            .unwrap_or_default()
    }

    fn shutdown_removed_runtime(&self, info: bmux_pane_runtime_state::RemovedRuntimeInfo) {
        let host = &self.host;
        let Some(handle) = Self::take_shutdown_handle(&info) else {
            return;
        };
        if let Ok(rt) = switchy_async::runtime::Handle::try_current() {
            switchy_async::task::block_in_place(|| {
                rt.block_on(shutdown_runtime_handle(
                    host,
                    RemovedRuntime {
                        session_id: info.session_id,
                        handle,
                    },
                ));
            });
        } else {
            drop(handle);
        }
    }

    fn read_pane_output_for_push(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
        client_id: ClientId,
        budget: usize,
    ) -> Option<(bmux_pane_runtime_state::OutputRead, bool)> {
        let pane = self
            .with_lock_read_named("read_pane_output_for_push.resolve", |m| {
                m.attach_pane_data_ref_for_push(session_id, pane_id, client_id)
            })
            .flatten()?;
        pane.output_dirty.store(false, Ordering::SeqCst);
        let mut output = pane.output_buffer.lock().ok()?;
        let retained_start = output.start_offset;
        let source_end = output.end_offset();
        let inner_read = output.read_for_client(client_id, budget);
        let sync_at_read_end = output.sync_update_open_at(inner_read.stream_end);
        drop(output);
        let sync_update_active = sync_at_read_end;
        Some((
            bmux_pane_runtime_state::OutputRead {
                bytes: inner_read.bytes,
                retained_start,
                stream_start: inner_read.stream_start,
                stream_end: inner_read.stream_end,
                source_end,
                stream_gap: inner_read.stream_gap,
            },
            sync_update_active,
        ))
    }

    fn lag_recovery_bump_attach_view_for_client(
        &self,
        client_id: ClientId,
    ) -> Vec<(SessionId, u64)> {
        self.with_lock(|m| {
            m.runtimes
                .iter_mut()
                .filter_map(|(session_id, runtime)| {
                    if !runtime.attached_clients.contains(&client_id) {
                        return None;
                    }
                    runtime.attach_view_revision = runtime.attach_view_revision.saturating_add(1);
                    Some((*session_id, runtime.attach_view_revision))
                })
                .collect()
        })
        .unwrap_or_default()
    }

    fn bump_attach_view_revision(&self, session_id: SessionId) -> Option<u64> {
        self.with_lock(|m| m.bump_attach_view_revision(session_id))
            .flatten()
    }

    fn shell_integration_root(&self) -> Option<std::path::PathBuf> {
        self.with_lock_read(|m| m.shell_integration_root.clone())
            .flatten()
    }

    fn test_mark_pane_exited(&self, session_id: SessionId, pane_id: Uuid, reason: String) -> bool {
        self.with_lock(|m| {
            let Some(runtime) = m.runtimes.get_mut(&session_id) else {
                return false;
            };
            let Some(pane) = runtime.panes.get_mut(&pane_id) else {
                return false;
            };
            pane.exited.store(true, Ordering::SeqCst);
            if let Ok(mut slot) = pane.exit_reason.lock() {
                *slot = Some(reason);
            }
            true
        })
        .unwrap_or(false)
    }
}

/// A pane's program ended (or never started). `exited` is that program's own flag: a restart
/// keeps the pane's id but gives its new program a new flag, so a late notice from the program
/// it replaced is recognisably stale ([`PaneExitEvent::is_current`]).
#[derive(Debug, Clone)]
struct PaneExitEvent {
    session_id: SessionId,
    pane_id: Uuid,
    exited: Arc<AtomicBool>,
}

impl PaneExitEvent {
    /// Whether the notice is about the program the pane runs now, or about a pane that is
    /// gone (closed): not about one a restart replaced.
    fn is_current(&self, manager: &SessionRuntimeManager) -> bool {
        manager
            .runtimes
            .get(&self.session_id)
            .and_then(|runtime| runtime.panes.get(&self.pane_id))
            .is_none_or(|pane| Arc::ptr_eq(&pane.exited, &self.exited))
    }
}

pub(crate) fn captured_image_window_anchored(
    host: &PaneRuntimeHost,
    request: &crate::handlers::attach_state::AnchoredHistoryImagesArgs,
    client: ClientId,
) -> Result<Vec<u8>, SessionRuntimeError> {
    if request.width == 0 || request.width > 4096 || request.rows == 0 || request.rows > 256 {
        return Err(SessionRuntimeError::ResponseBudgetExceeded);
    }
    let handle = host
        .pane_padding_handle()
        .ok_or(SessionRuntimeError::Closed)?;
    let pin = {
        let manager = handle.0.lock().map_err(|_| SessionRuntimeError::Closed)?;
        let runtime = manager
            .runtimes
            .get(&SessionId(request.session_id))
            .ok_or(SessionRuntimeError::NotFound)?;
        if !runtime.attached_clients.contains(&client) {
            return Err(SessionRuntimeError::NotAttached);
        }
        let pin = runtime
            .scrollback_pins
            .get(&(client, request.pane_id))
            .filter(|pin| pin.id == request.pin_id && pin.capture_id == request.capture_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        Arc::clone(&pin.grid)
    };
    let mut budget = RESPONSE_OUTPUT_BUDGET;
    let (top, column) = pin
        .grid
        .captured_anchor_at_width(
            request.line_index as usize,
            request.column as usize,
            usize::from(request.width),
            &mut budget,
        )
        .map_err(|_| SessionRuntimeError::ResponseBudgetExceeded)?;
    if column != 0 {
        return Err(SessionRuntimeError::ResponseBudgetExceeded);
    }
    let top = i64::try_from(top).map_err(|_| SessionRuntimeError::ResponseBudgetExceeded)?;
    #[cfg(feature = "image-registry")]
    let images = {
        let snapshot = pin.images.as_ref().ok_or(SessionRuntimeError::NotFound)?;
        let mut image_budget = RESPONSE_OUTPUT_BUDGET;
        snapshot
            .project_mapped(
                request.width,
                request.rows,
                &mut image_budget,
                |row, column| {
                    let (row, column) = pin
                        .grid
                        .captured_position_at_width(
                            row,
                            usize::from(column),
                            usize::from(request.width),
                            &mut budget,
                        )
                        .map_err(|error| {
                            std::io::Error::other(format!("captured image anchor: {error:?}"))
                        })?;
                    Ok((
                        i64::try_from(row).map_err(std::io::Error::other)? - top,
                        u16::try_from(column).map_err(std::io::Error::other)?,
                    ))
                },
            )
            .map_err(|_| SessionRuntimeError::ResponseBudgetExceeded)?
            .iter()
            .map(bmux_attach_image_protocol::AttachPaneImage::from)
            .collect::<Vec<_>>()
    };
    #[cfg(not(feature = "image-registry"))]
    let images: Vec<bmux_attach_image_protocol::AttachPaneImage> = {
        let _ = top;
        Vec::new()
    };
    let mut count = EncodedByteCount(0, RESPONSE_OUTPUT_BUDGET);
    serde_json::to_writer(&mut count, &images)
        .map_err(|_| SessionRuntimeError::ResponseBudgetExceeded)?;
    serde_json::to_vec(&images).map_err(|_| SessionRuntimeError::ResponseBudgetExceeded)
}

pub(crate) fn captured_image_window_reflowed(
    host: &PaneRuntimeHost,
    request: &crate::handlers::attach_state::HistoryImagesArgs,
    client: ClientId,
) -> Result<Vec<u8>, SessionRuntimeError> {
    captured_image_window_impl(host, request, client, true)
}

pub(crate) fn captured_image_window(
    host: &PaneRuntimeHost,
    request: &crate::handlers::attach_state::HistoryImagesArgs,
    client: ClientId,
) -> Result<Vec<u8>, SessionRuntimeError> {
    captured_image_window_impl(host, request, client, false)
}

fn captured_image_window_impl(
    host: &PaneRuntimeHost,
    request: &crate::handlers::attach_state::HistoryImagesArgs,
    client: ClientId,
    reflow: bool,
) -> Result<Vec<u8>, SessionRuntimeError> {
    if request.rows == 0 || request.rows > 256 {
        return Err(SessionRuntimeError::ResponseBudgetExceeded);
    }
    let handle = host
        .pane_padding_handle()
        .ok_or(SessionRuntimeError::Closed)?;
    let pin = {
        let manager = handle.0.lock().map_err(|_| SessionRuntimeError::Closed)?;
        let runtime = manager
            .runtimes
            .get(&SessionId(request.session_id))
            .ok_or(SessionRuntimeError::NotFound)?;
        if !runtime.attached_clients.contains(&client) {
            return Err(SessionRuntimeError::NotAttached);
        }
        let pin = runtime
            .scrollback_pins
            .get(&(client, request.pane_id))
            .filter(|pin| pin.id == request.pin_id && pin.capture_id == request.capture_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        Arc::clone(&pin.grid)
    };
    if reflow {
        encode_reflowed_image_window(&pin, request)
    } else {
        encode_captured_image_window(&pin, request)
    }
}

fn encode_reflowed_image_window(
    pin: &CapturedPinGrid,
    request: &crate::handlers::attach_state::HistoryImagesArgs,
) -> Result<Vec<u8>, SessionRuntimeError> {
    if request.width == 0 || request.width > 4096 || request.rows == 0 || request.rows > 256 {
        return Err(SessionRuntimeError::ResponseBudgetExceeded);
    }
    let mut budget = RESPONSE_OUTPUT_BUDGET;
    #[cfg(feature = "image-registry")]
    let count = pin.grid.main_row_count();
    let projected_count = pin
        .grid
        .main_row_count_at_width(usize::from(request.width), &mut budget)
        .map_err(|_| SessionRuntimeError::ResponseBudgetExceeded)?;
    let offset = request.scrollback_offset as usize;
    let visible_rows = usize::from(request.rows);
    if offset > projected_count.saturating_sub(visible_rows) {
        return Err(SessionRuntimeError::ResponseBudgetExceeded);
    }
    let top = i64::try_from(
        projected_count
            .saturating_sub(visible_rows)
            .saturating_sub(offset),
    )
    .map_err(|_| SessionRuntimeError::ResponseBudgetExceeded)?;
    #[cfg(feature = "image-registry")]
    let images = {
        let snapshot = pin.images.as_ref().ok_or(SessionRuntimeError::NotFound)?;
        let base = i64::try_from(count.saturating_sub(pin.grid.height()))
            .map_err(|_| SessionRuntimeError::ResponseBudgetExceeded)?;
        let mut image_budget = RESPONSE_OUTPUT_BUDGET;
        snapshot
            .project_mapped(
                request.width,
                request.rows,
                &mut image_budget,
                |row, column| {
                    let absolute =
                        usize::try_from(base.saturating_add(row)).map_err(std::io::Error::other)?;
                    let (row, column) = pin
                        .grid
                        .main_position_at_width(
                            absolute,
                            usize::from(column),
                            usize::from(request.width),
                            &mut budget,
                        )
                        .map_err(|error| {
                            std::io::Error::other(format!("image anchor mapping: {error:?}"))
                        })?;
                    Ok((
                        i64::try_from(row).map_err(std::io::Error::other)? - top,
                        u16::try_from(column).map_err(std::io::Error::other)?,
                    ))
                },
            )
            .map_err(|_| SessionRuntimeError::ResponseBudgetExceeded)?
            .iter()
            .map(bmux_attach_image_protocol::AttachPaneImage::from)
            .collect::<Vec<_>>()
    };
    #[cfg(not(feature = "image-registry"))]
    let images: Vec<bmux_attach_image_protocol::AttachPaneImage> = {
        let _ = top;
        Vec::new()
    };
    let mut count = EncodedByteCount(0, RESPONSE_OUTPUT_BUDGET);
    serde_json::to_writer(&mut count, &images)
        .map_err(|_| SessionRuntimeError::ResponseBudgetExceeded)?;
    serde_json::to_vec(&images).map_err(|_| SessionRuntimeError::ResponseBudgetExceeded)
}

fn encode_captured_image_window(
    pin: &CapturedPinGrid,
    request: &crate::handlers::attach_state::HistoryImagesArgs,
) -> Result<Vec<u8>, SessionRuntimeError> {
    if request.rows == 0
        || request.rows > 256
        || usize::from(request.width) != pin.grid.width()
        || request.scrollback_offset as usize > pin.grid.max_scrollback_offset()
    {
        return Err(SessionRuntimeError::ResponseBudgetExceeded);
    }
    #[cfg(feature = "image-registry")]
    let images = pin
        .images
        .as_ref()
        .ok_or(SessionRuntimeError::NotFound)?
        .project_viewport(request.scrollback_offset as usize, request.rows)
        .map_err(|_| SessionRuntimeError::ResponseBudgetExceeded)?
        .iter()
        .map(bmux_attach_image_protocol::AttachPaneImage::from)
        .collect::<Vec<_>>();
    #[cfg(not(feature = "image-registry"))]
    let images: Vec<bmux_attach_image_protocol::AttachPaneImage> = Vec::new();
    let mut count = EncodedByteCount(0, RESPONSE_OUTPUT_BUDGET);
    serde_json::to_writer(&mut count, &images)
        .map_err(|_| SessionRuntimeError::ResponseBudgetExceeded)?;
    serde_json::to_vec(&images).map_err(|_| SessionRuntimeError::ResponseBudgetExceeded)
}

#[derive(Clone)]
pub(crate) struct SpawnRuntimeHandle(
    Arc<Mutex<SessionRuntimeManager>>,
    switchy_async::runtime::Handle,
    Arc<RwLock<BTreeMap<Uuid, PaneInputHandle>>>,
);

#[derive(Debug)]
struct SpawnFailure(bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SpawnError);

impl std::fmt::Display for SpawnFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:?}", self.0)
    }
}

impl std::error::Error for SpawnFailure {}

fn spawn_error_after_cleanup(error: anyhow::Error, cleanup_errors: Vec<String>) -> anyhow::Error {
    if cleanup_errors.is_empty() {
        error
    } else {
        let cleanup = cleanup_errors
            .into_iter()
            .fold(String::new(), |mut message, failure| {
                if !message.is_empty() {
                    message.push_str("; ");
                }
                message.push_str(&failure);
                message
            });
        spawn_failure(
            bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SpawnError::CleanupFailed {
                reason: format!("{error}; cleanup errors: {cleanup}"),
            },
        )
    }
}

pub(crate) fn spawn_failure(
    error: bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SpawnError,
) -> anyhow::Error {
    anyhow::Error::new(SpawnFailure(error))
}

pub(crate) fn spawn_wire_error(
    error: anyhow::Error,
) -> bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SpawnError {
    use bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SpawnError;
    match error.downcast::<SpawnFailure>() {
        Ok(error) => error.0,
        Err(error) => SpawnError::LaunchFailed {
            reason: error.to_string(),
        },
    }
}

pub(crate) fn new_session_v1(
    host: &PaneRuntimeHost,
    request: bmux_pane_runtime_plugin_api::pane_runtime_spawn_commands_v1::client::NewSessionRequest,
    authorize: impl Fn(Uuid) -> Result<()>,
) -> Result<Uuid> {
    use bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SpawnError;
    let session = bmux_session_models::Session::with_id(
        bmux_session_models::SessionId(crate::ids::new_uuid()),
        request.name,
    );
    let id = session.id;
    authorize(id.0)?;
    validate_initial_shell_options(Some(&request.options)).map_err(|error| {
        spawn_failure(SpawnError::InvalidDirectory {
            path: request.options.cwd.clone().unwrap_or_default(),
            reason: error.to_string(),
        })
    })?;
    let handle = host
        .spawn_runtime_handle()
        .ok_or_else(|| spawn_failure(SpawnError::SourceUnavailable))?;
    let sessions = host
        .handle::<bmux_session_state::SessionManagerHandle>()
        .ok_or_else(|| spawn_failure(SpawnError::SourceUnavailable))?;
    let runtime = switchy_async::runtime::Handle::try_current()?;
    anyhow::ensure!(
        runtime.runtime_flavor() == switchy_async::RuntimeFlavor::MultiThread,
        "new-session requires multi-thread runtime"
    );
    let inserted = std::cell::Cell::new(false);
    let adapter = ServerSessionRuntimeAdapter::new(handle.0);
    let result = switchy_async::task::block_in_place(|| {
        runtime.block_on(adapter.start_fresh_runtime_transaction(
            id,
            Some(request.options),
            || authorize(id.0),
            || {
                sessions.0.insert_session(session)?;
                inserted.set(true);
                Ok(())
            },
        ))
    });
    if let Err(error) = result {
        if inserted.get() {
            sessions.0.remove_session(id).map_err(|cleanup| {
                spawn_failure(SpawnError::CleanupFailed {
                    reason: format!("{error}; session rollback failed: {cleanup}"),
                })
            })?;
        }
        return Err(error);
    }
    Ok(id.0)
}

pub(crate) fn split_shell_v1(
    host: &PaneRuntimeHost,
    request: bmux_pane_runtime_plugin_api::pane_runtime_spawn_commands_v1::client::SplitShellRequest,
    authorize: impl Fn() -> Result<()>,
) -> Result<Uuid> {
    let direction = match request.direction.as_str() {
        "horizontal" => PaneSplitDirection::Horizontal,
        "vertical" => PaneSplitDirection::Vertical,
        _ => anyhow::bail!("unsupported split direction"),
    };
    anyhow::ensure!(
        (10..=90).contains(&request.ratio_percent),
        "split ratio must be 10..90"
    );
    // Path validation observes execution-host state. Denied callers must not
    // learn path existence or runtime availability through validation errors.
    authorize()?;
    validate_initial_shell_options(Some(&request.options)).map_err(|error| {
        spawn_failure(
            bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SpawnError::InvalidDirectory {
                path: request.options.cwd.clone().unwrap_or_default(),
                reason: error.to_string(),
            },
        )
    })?;
    let handle = host.spawn_runtime_handle().ok_or_else(|| {
        spawn_failure(
            bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SpawnError::SourceUnavailable,
        )
    })?;
    ServerSessionRuntimeAdapter::new(handle.0).split_fresh_shell(
        SessionId(request.session_id),
        Some(PaneSelector::ById(request.target)),
        direction,
        Some(request.options),
        f32::from(request.ratio_percent) / 100.0,
        authorize,
    )
}

pub(crate) fn capture_spawn_source(
    host: &PaneRuntimeHost,
    session_id: Uuid,
    pane_id: Uuid,
) -> std::result::Result<
    bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SourceExecution,
    bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SpawnError,
> {
    use bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SpawnError;
    let handle = host
        .spawn_runtime_handle()
        .ok_or(SpawnError::SourceUnavailable)?;
    indexed_spawn_source(&handle.2, SessionId(session_id), pane_id)
}

// The snapshot bracket needs only the published incarnation, not mutable shell
// metadata. Never take the runtime manager or shell observer lock on this path.
fn indexed_spawn_source(
    index: &RwLock<BTreeMap<Uuid, PaneInputHandle>>,
    session_id: SessionId,
    pane_id: Uuid,
) -> std::result::Result<
    bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SourceExecution,
    bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SpawnError,
> {
    use bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::{SourceExecution, SpawnError};
    let index = index.read().map_err(|_| SpawnError::SourceUnavailable)?;
    let entry = index
        .get(&pane_id)
        .filter(|entry| entry.session_id == session_id && !entry.exited.load(Ordering::SeqCst))
        .ok_or(SpawnError::SourceUnavailable)?;
    Ok(SourceExecution {
        session_id: session_id.0,
        pane_id,
        execution_token: entry.execution_token.ok_or(SpawnError::SourceUnavailable)?,
    })
}

pub(crate) fn observe_spawn_directory(
    host: &PaneRuntimeHost,
    source: &bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SourceExecution,
) -> std::result::Result<
    bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::DirectoryObservation,
    bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SpawnError,
> {
    let handle = host.spawn_runtime_handle().ok_or(
        bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SpawnError::SourceUnavailable,
    )?;
    observe_spawn_directory_with(&handle.0, source, |pid, identity| {
        bounded_directory_probe(directory_probes(), Duration::from_millis(250), move || {
            observe_process_directory_for_identity(pid, identity)
        })
    })
}

// Timed-out OS calls retain their admission slot until they actually finish.
// There is no queue: a stalled filesystem cannot accumulate detached workers.
// Process-wide because the stalled OS threads it bounds are process-wide.
/// Directory probes running (see [`crate::run_shared`]).
enum DirectoryProbes {}

fn directory_probes() -> Arc<AtomicUsize> {
    crate::run_shared::run_shared::<DirectoryProbes, _>(|| AtomicUsize::new(0))
}
const MAX_DIRECTORY_PROBES: usize = 4;

fn bounded_directory_probe<
    T: Send + 'static,
    A: std::ops::Deref<Target = AtomicUsize> + Send + 'static,
>(
    active: A,
    timeout: Duration,
    probe: impl FnOnce() -> Option<T> + Send + 'static,
) -> std::result::Result<Option<T>, bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SpawnError>
{
    use bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SpawnError;
    struct Permit<A: std::ops::Deref<Target = AtomicUsize>>(A);
    impl<A: std::ops::Deref<Target = AtomicUsize>> Drop for Permit<A> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    active
        .try_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
            (count < MAX_DIRECTORY_PROBES).then_some(count + 1)
        })
        .map_err(|_| SpawnError::Busy)?;
    let permit = Permit(active);
    let (sender, receiver) = switchy_thread::sync::mpsc::sync_channel(1);
    switchy_thread::thread::Builder::new()
        .name("bmux-cwd-probe".into())
        .spawn(move || {
            let _permit = permit;
            let result = probe();
            let _ = sender.send(result);
        })
        .map_err(|_| SpawnError::ObservationUnavailable)?;
    receiver.recv_timeout(timeout).map_err(|error| match error {
        switchy_thread::sync::mpsc::RecvTimeoutError::Timeout => SpawnError::ObservationTimeout,
        switchy_thread::sync::mpsc::RecvTimeoutError::Disconnected => {
            SpawnError::ObservationUnavailable
        }
    })
}

fn observe_spawn_directory_with(
    manager: &switchy_thread::sync::Mutex<SessionRuntimeManager>,
    source: &bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SourceExecution,
    probe: impl FnOnce(
        u32,
        ProcessStartIdentity,
    ) -> std::result::Result<
        Option<String>,
        bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SpawnError,
    >,
) -> std::result::Result<
    bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::DirectoryObservation,
    bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SpawnError,
> {
    use bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::{
        DirectoryObservation, ObservationMethod, SpawnError,
    };
    let prepare = || {
        manager
            .try_lock()
            .map_err(|error| match error {
                switchy_thread::sync::TryLockError::WouldBlock => SpawnError::Busy,
                switchy_thread::sync::TryLockError::Poisoned(_) => SpawnError::SourceUnavailable,
            })?
            .prepare_spawn_directory_probe(source)
    };
    let (path, method) = match prepare()? {
        SpawnDirectoryProbe::Shell(path) => (path, ObservationMethod::Shell),
        SpawnDirectoryProbe::Process(pid, identity) => {
            // No manager, metadata or PID lock may span native I/O. Revalidate
            // the captured execution after the query; replacement can race it.
            let path = probe(pid, identity);
            match prepare()? {
                SpawnDirectoryProbe::Shell(path) => (path, ObservationMethod::Shell),
                SpawnDirectoryProbe::Process(current_pid, current_identity)
                    if current_pid == pid && current_identity == identity =>
                {
                    (
                        path?.ok_or(SpawnError::ObservationUnavailable)?,
                        ObservationMethod::Process,
                    )
                }
                SpawnDirectoryProbe::Process(_, _) => return Err(SpawnError::StaleExecution),
            }
        }
    };
    Ok(DirectoryObservation {
        source: source.clone(),
        path,
        method,
    })
}

fn pane_padding_state(
    host: &PaneRuntimeHost,
    session_id: SessionId,
    pane_id: Uuid,
) -> Option<crate::padding_api::RuntimePanePaddingState> {
    host.pane_padding_handle()?
        .state(session_id, Some(pane_id))
        .ok()
}

fn notify_pane_metadata_changed(
    host: &PaneRuntimeHost,
    session_id: SessionId,
    pane_id: Uuid,
    before: crate::padding_api::RuntimePanePaddingState,
) {
    let Some(handle) = host.pane_padding_handle() else {
        return;
    };
    with_padding_manager(&handle.0, |manager| {
        manager.reconcile_padding_after_metadata_change(session_id, pane_id, &before)
    })
    .ok();
}

fn reap_exited_pane(
    manager: &Arc<Mutex<SessionRuntimeManager>>,
    session_id: SessionId,
    pane_id: Uuid,
) {
    let Ok(host) = manager.lock().map(|manager| manager.host.clone()) else {
        return;
    };
    let host = &host;
    let (state_reason, update_sessions) = manager.lock().map_or_else(
        |_| (None, vec![session_id]),
        |mut manager| {
            let state_reason = manager
                .runtimes
                .get(&session_id)
                .and_then(|runtime| runtime.panes.get(&pane_id))
                .and_then(pane_state_reason_for_handle);
            let publish_all = manager.reconcile_exited_pane_focus(session_id, pane_id);
            let update_sessions = if publish_all {
                manager.active_session_ids()
            } else {
                vec![session_id]
            };
            (state_reason, update_sessions)
        },
    );
    host.publish_pane_event(PaneEvent::Exited {
        session_id: session_id.0,
        pane_id,
        reason: state_reason,
    });
    for session_id in update_sessions {
        host.emit_attach_view_changed_for_layout(session_id);
    }
    crate::handlers::publish_focus_state_snapshot(host);
    crate::handlers::pane_commands::publish_pane_lifecycle_facts(host);
    host.mark_snapshot_dirty();
}

async fn process_pane_exit_events(
    manager: Arc<Mutex<SessionRuntimeManager>>,
    mut pane_exit_rx: mpsc::UnboundedReceiver<PaneExitEvent>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    loop {
        switchy_async::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_ok() && *shutdown_rx.borrow() {
                    break;
                }
                if changed.is_err() {
                    break;
                }
            }
            maybe_event = pane_exit_rx.recv() => {
                let Some(event) = maybe_event else {
                    break;
                };
                // A restart replaced the program that exited: the pane's id now names its
                // successor, which is running.
                if !manager.lock().is_ok_and(|manager| event.is_current(&manager)) {
                    continue;
                }
                reap_exited_pane(&manager, event.session_id, event.pane_id);
            }
        }
    }
}

#[derive(Debug, Clone)]
struct ProcessInspectionResult {
    command: Option<String>,
    cwd: Option<String>,
}

fn inspect_process_group_entries_with_resolver<F>(
    entries: &[PsProcessEntry],
    process_group_id: i32,
    shell_pid: Option<u32>,
    shell_path: &str,
    mut resolve_cwd: F,
) -> Option<ProcessInspectionResult>
where
    F: FnMut(u32) -> Option<String>,
{
    if process_group_id <= 0 {
        return None;
    }

    let mut selected_command: Option<(u32, String)> = None;
    let mut shell_candidate_pid: Option<u32> = None;

    for entry in entries {
        if entry.pgid != process_group_id || entry.command.is_empty() || entry.state.contains('Z') {
            continue;
        }

        if shell_pid == Some(entry.pid)
            || process_command_looks_like_shell(&entry.command, shell_path)
        {
            if shell_candidate_pid.is_none_or(|best_pid| entry.pid > best_pid) {
                shell_candidate_pid = Some(entry.pid);
            }
            continue;
        }

        if selected_command
            .as_ref()
            .is_none_or(|(best_pid, _)| entry.pid > *best_pid)
        {
            selected_command = Some((entry.pid, entry.command.clone()));
        }
    }

    let cwd = selected_command
        .as_ref()
        .and_then(|(pid, _)| resolve_cwd(*pid))
        .or_else(|| shell_pid.and_then(&mut resolve_cwd))
        .or_else(|| shell_candidate_pid.and_then(&mut resolve_cwd));
    let command = selected_command.map(|(_, command)| command);

    if command.is_none() && cwd.is_none() {
        return None;
    }

    Some(ProcessInspectionResult { command, cwd })
}

/// How many of a foreground group's processes a job lists.
const MAX_FOREGROUND_COMMANDS: usize = 8;

/// The command lines of process group `group`'s live processes, its leader
/// first, then by pid.
fn foreground_group_commands(entries: &[PsProcessEntry], group: i32) -> Vec<String> {
    let mut members = entries
        .iter()
        .filter(|entry| {
            entry.pgid == group && !entry.command.is_empty() && !entry.state.contains('Z')
        })
        .collect::<Vec<_>>();
    members.sort_by_key(|entry| (i64::from(entry.pid) != i64::from(group), entry.pid));
    members
        .into_iter()
        .take(MAX_FOREGROUND_COMMANDS)
        .map(|entry| entry.command.clone())
        .collect()
}

/// The command and working directory of what runs in process group `process_group_id`.
fn inspect_process_group_command_and_cwd(
    process_group_id: Option<i32>,
    shell_pid: Option<u32>,
    shell_path: &str,
) -> Option<ProcessInspectionResult> {
    let process_group_id = process_group_id?;
    let entries = crate::platform::process_table()?;
    inspect_process_group_entries_with_resolver(
        &entries,
        process_group_id,
        shell_pid,
        shell_path,
        resolve_process_working_directory,
    )
}

fn process_command_looks_like_shell(command: &str, shell_path: &str) -> bool {
    let Some(first) = command.split_whitespace().next() else {
        return false;
    };
    let name = std::path::Path::new(first)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(first)
        .trim_start_matches('-')
        .to_ascii_lowercase();
    let shell_name = std::path::Path::new(shell_path)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(shell_path)
        .trim_start_matches('-')
        .to_ascii_lowercase();
    name == shell_name
        || matches!(
            name.as_str(),
            "sh" | "bash"
                | "zsh"
                | "fish"
                | "nu"
                | "nushell"
                | "dash"
                | "ksh"
                | "mksh"
                | "csh"
                | "tcsh"
        )
}

fn with_padding_manager<R>(
    manager: &Mutex<SessionRuntimeManager>,
    operation: impl FnOnce(&mut SessionRuntimeManager) -> Result<R, SessionRuntimeError>,
) -> Result<R, SessionRuntimeError> {
    let (result, revisions, host) = {
        let mut manager = manager.lock().map_err(|_| SessionRuntimeError::Closed)?;
        let result = operation(&mut manager);
        (
            result,
            std::mem::take(&mut manager.pending_layout_revisions),
            manager.host.clone(),
        )
    };
    // Publish even after a partial failure: completed mutations still invalidate
    // their projections. Never re-enter the manager from a locked mutation.
    publish_layout_revisions(&host, revisions);
    result
}

fn publish_layout_revisions(host: &PaneRuntimeHost, revisions: BTreeMap<SessionId, u64>) {
    for (session_id, revision) in revisions {
        host.publish_pane_event(PaneEvent::AttachViewChanged {
            context_id: host.current_context_id_for_session(session_id),
            session_id: session_id.0,
            revision,
            components: vec![
                AttachViewComponent::Scene,
                AttachViewComponent::SurfaceContent,
                AttachViewComponent::Layout,
            ],
        });
    }
}

#[derive(Clone)]
pub(crate) struct PanePaddingRuntimeHandle(Arc<Mutex<SessionRuntimeManager>>);

impl PanePaddingRuntimeHandle {
    #[cfg(test)]
    fn from_manager_for_test(manager: SessionRuntimeManager) -> Self {
        Self(Arc::new(Mutex::new(manager)))
    }

    pub(crate) fn session_for_client(
        &self,
        client_id: ClientId,
    ) -> Result<SessionId, SessionRuntimeError> {
        self.0
            .lock()
            .map_err(|_| SessionRuntimeError::Closed)?
            .runtimes
            .iter()
            .find_map(|(session_id, runtime)| {
                runtime
                    .attached_clients
                    .contains(&client_id)
                    .then_some(*session_id)
            })
            .ok_or(SessionRuntimeError::NotFound)
    }

    pub(crate) fn has_session(&self, session_id: SessionId) -> bool {
        self.0
            .lock()
            .is_ok_and(|manager| manager.runtimes.contains_key(&session_id))
    }

    pub(crate) fn begin_preview(
        &self,
        owner_client_id: ClientId,
        pane_ids: Vec<Uuid>,
        spec: PanePaddingSpec,
    ) -> Result<(Uuid, Vec<crate::padding_api::RuntimePanePaddingState>), SessionRuntimeError> {
        with_padding_manager(&self.0, |manager| {
            manager.begin_padding_preview(owner_client_id, pane_ids, spec)
        })
    }

    pub(crate) fn update_preview(
        &self,
        owner_client_id: ClientId,
        token: Uuid,
        pane_ids: Vec<Uuid>,
        spec: PanePaddingSpec,
    ) -> Result<Vec<crate::padding_api::RuntimePanePaddingState>, SessionRuntimeError> {
        with_padding_manager(&self.0, |manager| {
            manager.update_padding_preview(owner_client_id, token, pane_ids, spec)
        })
    }

    pub(crate) fn cancel_preview(
        &self,
        owner_client_id: ClientId,
        token: Uuid,
    ) -> Result<(), SessionRuntimeError> {
        with_padding_manager(&self.0, |manager| {
            manager.cancel_padding_preview(owner_client_id, token)
        })
    }

    pub(crate) fn commit_preview(
        &self,
        owner_client_id: ClientId,
        token: Uuid,
        persistence: PanePaddingOverridePersistence,
    ) -> Result<Vec<crate::padding_api::RuntimePanePaddingState>, SessionRuntimeError> {
        with_padding_manager(&self.0, |manager| {
            manager.commit_padding_preview(owner_client_id, token, persistence)
        })
    }

    pub(crate) fn state(
        &self,
        session_id: SessionId,
        pane_id: Option<Uuid>,
    ) -> Result<crate::padding_api::RuntimePanePaddingState, SessionRuntimeError> {
        self.0
            .lock()
            .map_err(|_| SessionRuntimeError::Closed)?
            .padding_state(session_id, pane_id)
    }

    pub(crate) fn clear_overrides(
        &self,
        pane_ids: &[Uuid],
    ) -> Result<Vec<crate::padding_api::RuntimePanePaddingState>, SessionRuntimeError> {
        with_padding_manager(&self.0, |manager| manager.clear_padding_overrides(pane_ids))
    }

    pub(crate) fn overrides_for_snapshot(
        &self,
    ) -> Result<BTreeMap<(SessionId, Uuid), PanePaddingSpec>, SessionRuntimeError> {
        Ok(self
            .0
            .lock()
            .map_err(|_| SessionRuntimeError::Closed)?
            .padding_overrides_for_snapshot())
    }

    pub(crate) fn install_restored_override(
        &self,
        session_id: SessionId,
        pane_id: Uuid,
        spec: PanePaddingSpec,
    ) -> Result<(), SessionRuntimeError> {
        self.0
            .lock()
            .map_err(|_| SessionRuntimeError::Closed)?
            .install_restored_padding_override(session_id, pane_id, spec);
        Ok(())
    }

    pub(crate) fn cancel_all_previews(&self) -> Result<(), SessionRuntimeError> {
        with_padding_manager(&self.0, |manager| {
            manager.cancel_all_padding_previews();
            Ok(())
        })
    }

    pub(crate) fn replace_padding_config(
        &self,
        padding_config: PanePaddingConfig,
    ) -> Result<(), SessionRuntimeError> {
        self.replace_padding_config_with_hook(padding_config, || Ok(()))
    }

    /// Install a new global padding default live, keeping configured rules.
    pub(crate) fn replace_padding_defaults(&self, defaults: PanePaddingSpec) -> Result<(), String> {
        let config = {
            let manager = self
                .0
                .lock()
                .map_err(|_| SessionRuntimeError::Closed.to_string())?;
            manager.padding_config.with_defaults(defaults)?
        };
        self.replace_padding_config(config)
            .map_err(|error| error.to_string())
    }

    fn replace_padding_config_with_hook(
        &self,
        padding_config: PanePaddingConfig,
        before_install: impl FnOnce() -> Result<(), SessionRuntimeError>,
    ) -> Result<(), SessionRuntimeError> {
        before_install()?;
        with_padding_manager(&self.0, |manager| {
            let targets = manager
                .runtimes
                .iter()
                .flat_map(|(session_id, runtime)| {
                    runtime
                        .panes
                        .keys()
                        .map(|pane_id| (*session_id, *pane_id))
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            let before = targets
                .iter()
                .map(|(session_id, pane_id)| {
                    manager
                        .padding_state(*session_id, Some(*pane_id))
                        .map(|state| (*session_id, *pane_id, state))
                })
                .collect::<Result<Vec<_>, _>>()?;
            manager.padding_config = padding_config;
            for (session_id, pane_id, previous) in before {
                let after = manager.padding_state(session_id, Some(pane_id))?;
                manager.apply_padding_geometry_change(session_id, pane_id, &previous, &after)?;
            }
            Ok(())
        })
    }

    pub(crate) fn pane_ids(&self, session_id: SessionId) -> Result<Vec<Uuid>, SessionRuntimeError> {
        let manager = self.0.lock().map_err(|_| SessionRuntimeError::Closed)?;
        let runtime = manager
            .runtimes
            .get(&session_id)
            .ok_or(SessionRuntimeError::NotFound)?;
        Ok(runtime.panes.keys().copied().collect())
    }

    pub(crate) fn all_pane_ids(&self) -> Result<Vec<Uuid>, SessionRuntimeError> {
        let manager = self.0.lock().map_err(|_| SessionRuntimeError::Closed)?;
        Ok(manager
            .runtimes
            .values()
            .flat_map(|runtime| runtime.panes.keys().copied())
            .collect())
    }

    pub(crate) fn set_override(
        &self,
        session_id: SessionId,
        pane_id: Option<Uuid>,
        spec: Option<PanePaddingSpec>,
    ) -> Result<crate::padding_api::RuntimePanePaddingState, SessionRuntimeError> {
        with_padding_manager(&self.0, |manager| {
            manager.set_padding_override(session_id, pane_id, spec)
        })
    }
}

struct PaneRuntimeShutdown {
    manager: Arc<Mutex<SessionRuntimeManager>>,
    host: PaneRuntimeHost,
}

impl bmux_plugin_runtime::shutdown::ShutdownParticipant for PaneRuntimeShutdown {
    fn id(&self) -> &'static str {
        "bmux.pane_runtime.release"
    }

    fn phase(&self) -> bmux_plugin_runtime::shutdown::ShutdownPhase {
        bmux_plugin_runtime::shutdown::ShutdownPhase::Release
    }

    fn shutdown(
        &self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = std::result::Result<(), String>> + Send + 'static>,
    > {
        let host = self.host.clone();
        let manager = Arc::clone(&self.manager);
        Box::pin(async move {
            let host = &host;
            let removed = manager
                .lock()
                .map_err(|_| "runtime lock poisoned".to_string())?
                .remove_all_runtimes();
            let result = shutdown_removed_runtimes(host, removed).await;
            // Earlier close/remove callers may already have received a failure
            // receipt while their tasks still own OS reapers. They also fence
            // unloading, even though their panes are no longer in the manager.
            let owners = {
                let mut owners = host
                    .pane_shutdown_owners()
                    .lock()
                    .map_err(|_| "pane shutdown owners lock poisoned".to_string())?;
                std::mem::take(&mut *owners)
            };
            let mut errors = result.err().into_iter().collect::<Vec<_>>();
            for (id, mut owner) in owners {
                loop {
                    match switchy_async::time::timeout(Duration::from_secs(5), &mut owner).await {
                        Ok(Ok(Ok(()))) => break,
                        Ok(Ok(Err(error))) => {
                            errors.push(error);
                            break;
                        }
                        Ok(Err(error)) => {
                            errors.push(format!("pane cleanup owner failed: {error}"));
                            break;
                        }
                        Err(_) => warn!(phase = "Release", cleanup_id = %id,
                            "prior pane cleanup still waiting; plugin unload fenced"),
                    }
                }
            }
            if errors.is_empty() {
                Ok(())
            } else {
                Err(errors.join("; "))
            }
        })
    }
}

async fn shutdown_removed_runtimes(
    host: &crate::runtime::PaneRuntimeHost,
    removed: Vec<RemovedRuntime>,
) -> std::result::Result<(), String> {
    // One cleanup owner across sessions: no runtime can wait for another
    // runtime whose stop has not yet been admitted.
    let panes = removed
        .into_iter()
        .flat_map(|runtime| runtime.handle.panes.into_values())
        .collect();
    shutdown_panes_owned(host, panes, true).await
}

pub(crate) fn activate_pane_runtime(
    host: &PaneRuntimeHost,
    config: PaneRuntimePluginConfig,
    padding_config: PanePaddingConfig,
) -> std::result::Result<(), String> {
    tracing::debug!(
        persist_runtime_overrides = padding_config.persist_runtime_overrides,
        "activating pane padding configuration"
    );
    // One runtime per process, released by its shutdown participant. A later
    // activation in the same process (each caller-process command loads a
    // fresh command-local instance) adopts it instead of registering a
    // duplicate participant and a second manager.
    if let Some(existing) = host.pane_padding_handle() {
        return existing
            .replace_padding_config(padding_config)
            .map_err(|error| error.to_string());
    }
    let (pane_exit_tx, pane_exit_rx) = mpsc::unbounded_channel();
    let manager = Arc::new(Mutex::new(SessionRuntimeManager::new(
        host.clone(),
        config.shell,
        config.pane_term.clone(),
        protocol_profile_for_term(&config.pane_term),
        config.bracketed_paste,
        config.shell_integration_root,
        padding_config,
        pane_exit_tx,
    )));
    let lifecycle = host
        .states()
        .get_or_insert_with(bmux_plugin_runtime::shutdown::ShutdownRegistry::default);
    let lifecycle = lifecycle
        .read()
        .map_err(|_| "shutdown registry lock poisoned".to_string())?
        .clone();
    lifecycle.register(Arc::new(PaneRuntimeShutdown {
        manager: Arc::clone(&manager),
        host: host.clone(),
    }))?;
    let adapter = ServerSessionRuntimeAdapter::new(Arc::clone(&manager));
    let pane_index = Arc::clone(&adapter.pane_input_index);
    let runtime_handle = bmux_pane_runtime_state::SessionRuntimeManagerHandle::new(adapter);
    // Core's state registry holds its entries behind std's locks.
    host.states()
        .register::<bmux_pane_runtime_state::SessionRuntimeManagerHandle>(&Arc::new(
            // simulation-safe: core's state registry type, written only at registration
            std::sync::RwLock::new(runtime_handle),
        ));
    host.states()
        // simulation-safe: core's state registry type, written only at registration
        .register::<PanePaddingRuntimeHandle>(&Arc::new(std::sync::RwLock::new(
            PanePaddingRuntimeHandle(Arc::clone(&manager)),
        )));

    host.states()
        // simulation-safe: core's state registry type, written only at registration
        .register::<SpawnRuntimeHandle>(&Arc::new(std::sync::RwLock::new(SpawnRuntimeHandle(
            Arc::clone(&manager),
            switchy_async::runtime::Handle::current(),
            pane_index,
        ))));
    let expiry_manager = Arc::clone(&manager);
    switchy_async::task::spawn(async move {
        let mut interval = switchy_async::time::interval(PANE_PADDING_PREVIEW_SWEEP_INTERVAL);
        interval.set_missed_tick_behavior(switchy_async::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if with_padding_manager(&expiry_manager, |manager| {
                manager.cancel_expired_padding_previews(Instant::now());
                Ok(())
            })
            .is_err()
            {
                break;
            }
        }
    });

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let exit_manager = Arc::clone(&manager);
    switchy_async::task::spawn(async move {
        // No external shutdown owner is wired here. Keep the sender alive for
        // this task's scope: exit_rx closure or runtime abort owns its lifetime.
        // A named binding (not `let _ = ...`) drops only after the await ends.
        let _shutdown_guard = shutdown_tx;
        process_pane_exit_events(exit_manager, pane_exit_rx, shutdown_rx).await;
    });

    crate::snapshot::PaneRuntimeStateful::register(host, move || {
        let states = {
            let manager = manager
                .lock()
                .map_err(|_| anyhow::anyhow!("runtime lock poisoned"))?;
            manager
                .runtimes
                .values()
                .flat_map(|runtime| {
                    runtime
                        .panes
                        .values()
                        .map(|pane| Arc::clone(&pane.resurrection_state))
                })
                .collect::<Vec<_>>()
        };
        for state in states {
            let mut state = state
                .lock()
                .map_err(|_| anyhow::anyhow!("resurrection lock poisoned"))?;
            // The metadata reader holds this same lock across boundary commit and
            // identity publication. Closing drains descriptor writers as well.
            state.freeze()?;
        }
        Ok(())
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    fn two_instance_config() -> PaneRuntimePluginConfig {
        PaneRuntimePluginConfig {
            shell: "/bin/sh".into(),
            pane_term: "xterm-256color".into(),
            bracketed_paste: true,
            shell_integration_root: None,
        }
    }

    /// Two pane-runtime instances never share runtime state: each activation
    /// builds its own manager in the registry it was handed, and per-activation
    /// state (capture barriers, observers, revisions) lives in that
    /// activation's host handles, even when two activations share one host
    /// registry root.
    #[switchy_async::test]
    async fn two_instances_keep_separate_runtime_and_activation_state() {
        let first = PaneRuntimeHost::detached();
        let second = PaneRuntimeHost::detached();
        activate_pane_runtime(&first, two_instance_config(), PanePaddingConfig::default())
            .expect("first activation");
        activate_pane_runtime(&second, two_instance_config(), PanePaddingConfig::default())
            .expect("second activation");
        let first_manager = first.spawn_runtime_handle().expect("first manager").0;
        let second_manager = second.spawn_runtime_handle().expect("second manager").0;
        assert!(!Arc::ptr_eq(&first_manager, &second_manager));
        assert!(!first.states().shares_entries_with(second.states()));

        // Activations on one host root see the host's communal handles but
        // keep their own capture barriers and revisions.
        let root = Arc::new(bmux_plugin::PluginStateRegistry::new());
        let left = PaneRuntimeHost::new(Arc::clone(&root), None);
        let right = PaneRuntimeHost::new(Arc::clone(&root), None);
        let pane = crate::ids::new_uuid();
        assert!(crate::private_input::suppress(&left, pane));
        assert_eq!(
            crate::private_input::with_capture_barrier(&left, pane, |capture| capture),
            Some(false)
        );
        assert_eq!(
            crate::private_input::with_capture_barrier(&right, pane, |capture| capture),
            Some(true)
        );
        assert_eq!(left.next_focus_revision(), 1);
        assert_eq!(left.next_focus_revision(), 2);
        assert_eq!(right.next_focus_revision(), 1);
        crate::handlers::attach_commands::mark_test_observer(
            &left,
            ClientId(crate::ids::new_uuid()),
            SessionId(pane),
        );
        assert!(right.observers().lock().unwrap().is_empty());

        // Each activation owns its reservation store and HUMAN input epochs.
        let domain = tempfile::tempdir().expect("reservation domain");
        reservations::activate_at_for_test(&left, domain.path());
        assert_eq!(reservations::idle_blocker(&left), None);
        assert!(
            reservations::idle_blocker(&right)
                .is_some_and(|blocker| blocker.contains("not activated")),
            "an activation never reaches another activation's store"
        );
        let execution =
            bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalExecutionRef {
                session_id: crate::ids::new_uuid(),
                pane_id: pane,
                execution_token: crate::ids::new_uuid(),
            };
        assert_eq!(
            human_input::open_unvalidated_for_test(&left, execution.clone()),
            Ok(1)
        );
        assert_eq!(
            human_input::open_unvalidated_for_test(&left, execution.clone()),
            Ok(2)
        );
        assert_eq!(
            human_input::open_unvalidated_for_test(&right, execution),
            Ok(1)
        );
        // Idle authority is per activation too.
        let admission = managed_idle::admit(&left).expect("left admission");
        assert!(!managed_idle::admissions_settled_for_test(&left));
        assert!(managed_idle::admissions_settled_for_test(&right));
        drop(admission);

        for host in [&first, &second] {
            let removed = host
                .spawn_runtime_handle()
                .expect("manager")
                .0
                .lock()
                .expect("manager lock")
                .remove_all_runtimes();
            shutdown_removed_runtimes(host, removed).await.unwrap();
        }
    }

    #[switchy_async::test]
    async fn activated_exit_consumer_survives_initialization_and_publishes_exit() {
        let bus = Arc::new(bmux_plugin::EventBus::new());
        let host = PaneRuntimeHost::new(
            Arc::new(bmux_plugin::PluginStateRegistry::new()),
            Some(Arc::clone(&bus)),
        );
        bus.register_channel::<PaneEvent>(pane_runtime_events::EVENT_KIND);
        let mut events = bus
            .subscribe::<PaneEvent>(&pane_runtime_events::EVENT_KIND)
            .unwrap();
        activate_pane_runtime(
            &host,
            PaneRuntimePluginConfig {
                shell: "/bin/sh".into(),
                pane_term: "xterm-256color".into(),
                bracketed_paste: true,
                shell_integration_root: None,
            },
            PanePaddingConfig::default(),
        )
        .expect("activate pane runtime");
        let manager = host.spawn_runtime_handle().expect("activated runtime").0;
        let session_id = SessionId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let reason = "activation exit regression";
        let mut runtime = runtime_with_panes(&[pane_id]);
        let pane = runtime.panes.get_mut(&pane_id).unwrap();
        pane.exited.store(true, Ordering::SeqCst);
        *pane.exit_reason.lock().unwrap() = Some(reason.to_owned());
        let exited = Arc::clone(&pane.exited);
        let exit_tx = {
            let mut manager = manager.lock().unwrap();
            manager.runtimes.insert(session_id, runtime);
            manager.pane_exit_tx.clone()
        };
        // Let the activation task poll its shutdown receiver before enqueueing
        // exit. The old disconnected watch sender closes this listener here.
        switchy_async::task::yield_now().await;
        assert!(
            !exit_tx.is_closed(),
            "activation dropped the pane exit listener"
        );
        exit_tx
            .send(PaneExitEvent {
                session_id,
                pane_id,
                exited,
            })
            .unwrap();
        let event = switchy_async::time::timeout(Duration::from_secs(1), async {
            loop {
                let event = events.recv().await.unwrap();
                if matches!(&*event, PaneEvent::Exited { session_id: sid, pane_id: pid, .. }
                    if *sid == session_id.0 && *pid == pane_id)
                {
                    break event;
                }
            }
        })
        .await
        .expect("activated consumer must publish the typed exit");
        assert!(
            matches!(&*event, PaneEvent::Exited { reason: Some(actual), .. } if actual == reason)
        );
        let mut removed = manager
            .lock()
            .unwrap()
            .runtimes
            .remove(&session_id)
            .unwrap();
        removed.panes.remove(&pane_id).unwrap().task.abort();
    }

    #[switchy_async::test]
    async fn exit_of_a_program_a_restart_replaced_is_not_published() {
        let bus = Arc::new(bmux_plugin::EventBus::new());
        let host = PaneRuntimeHost::new(
            Arc::new(bmux_plugin::PluginStateRegistry::new()),
            Some(Arc::clone(&bus)),
        );
        bus.register_channel::<PaneEvent>(pane_runtime_events::EVENT_KIND);
        let mut events = bus
            .subscribe::<PaneEvent>(&pane_runtime_events::EVENT_KIND)
            .unwrap();
        activate_pane_runtime(
            &host,
            PaneRuntimePluginConfig {
                shell: "/bin/sh".into(),
                pane_term: "xterm-256color".into(),
                bracketed_paste: true,
                shell_integration_root: None,
            },
            PanePaddingConfig::default(),
        )
        .expect("activate pane runtime");
        let manager = host.spawn_runtime_handle().expect("activated runtime").0;
        let session_id = SessionId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        // The pane runs its restarted program; the replaced one's flag is another.
        let runtime = runtime_with_panes(&[pane_id]);
        let replaced = Arc::new(AtomicBool::new(true));
        let exit_tx = {
            let mut manager = manager.lock().unwrap();
            manager.runtimes.insert(session_id, runtime);
            manager.pane_exit_tx.clone()
        };
        switchy_async::task::yield_now().await;
        exit_tx
            .send(PaneExitEvent {
                session_id,
                pane_id,
                exited: replaced,
            })
            .unwrap();
        let published = switchy_async::time::timeout(Duration::from_millis(200), async {
            loop {
                let event = events.recv().await.unwrap();
                if matches!(&*event, PaneEvent::Exited { pane_id: pid, .. } if *pid == pane_id) {
                    break;
                }
            }
        })
        .await;
        assert!(
            published.is_err(),
            "a replaced program's exit must not report the running pane exited"
        );
        let mut removed = manager
            .lock()
            .unwrap()
            .runtimes
            .remove(&session_id)
            .unwrap();
        removed.panes.remove(&pane_id).unwrap().task.abort();
    }

    #[cfg(unix)]
    #[switchy_async::test(flavor = "multi_thread", worker_threads = 2)]
    async fn owned_terminal_accepts_input_resize_and_explicit_shutdown() {
        let host = crate::runtime::PaneRuntimeHost::detached();
        let host = &host;
        let (exit_tx, _exit_rx) = mpsc::unbounded_channel();
        let mut manager = SessionRuntimeManager::new(
            PaneRuntimeHost::detached(),
            "/bin/sh".into(),
            "xterm-256color".into(),
            protocol_profile_for_term("xterm-256color"),
            true,
            None,
            PanePaddingConfig::default(),
            exit_tx,
        );
        // Repeat to prove retiring a direct terminal does not poison later starts.
        for _ in 0..2 {
            let session_id = SessionId(crate::ids::new_uuid());
            let mut prepared = manager
                .prepare_fresh_program_runtime(session_id, None, None, (24, 80))
                .unwrap();
            let pane = prepared
                .runtime
                .as_ref()
                .unwrap()
                .panes
                .values()
                .next()
                .unwrap();
            let execution = Arc::clone(&pane.execution);
            let _cleanup = ExecutionTestCleanup(Arc::clone(&execution));
            prepared.wait_until_ready().await.unwrap();
            manager
                .commit_fresh_runtime(session_id, &mut prepared, || Ok(()))
                .unwrap();
            let pane = manager.runtimes[&session_id].panes.values().next().unwrap();
            pane.resize_pty(31, 91);
            assert_eq!(*pane.last_requested_size.lock().unwrap(), (31, 91));
            pane.input_tx
                .send(PaneRuntimeCommand::Input(b"printf DIRECT_INPUT\n".to_vec()))
                .unwrap();
            switchy_async::time::timeout(Duration::from_secs(5), async {
                while execution.output_watermark.load(Ordering::SeqCst) < 12 {
                    switchy_async::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert!(execution.session.lock().unwrap().is_some());
            assert!(!pane.exited.load(Ordering::SeqCst));
            let removed = manager.remove_runtime(session_id).unwrap();
            switchy_async::time::timeout(
                Duration::from_secs(5),
                shutdown_runtime_handle(host, removed),
            )
            .await
            .unwrap();
            assert!(execution.output_drained.load(Ordering::SeqCst));
            assert!(!execution.cleanup_failed.load(Ordering::SeqCst));
        }
    }

    #[cfg(windows)]
    #[switchy_async::test(flavor = "multi_thread", worker_threads = 2)]
    async fn windows_program_startup_answers_conpty_query_then_captures_and_drains() {
        let host = crate::runtime::PaneRuntimeHost::detached();
        let session_id = SessionId(crate::ids::new_uuid());
        let (exit_tx, _exit_rx) = mpsc::unbounded_channel();
        let program = std::env::var("COMSPEC").expect("Windows command interpreter");
        let mut manager = SessionRuntimeManager::new(
            host.clone(),
            program.clone(),
            "xterm-256color".into(),
            protocol_profile_for_term("xterm-256color"),
            true,
            None,
            PanePaddingConfig::default(),
            exit_tx,
        );
        let mut prepared = manager
            .prepare_fresh_program_runtime(
                session_id,
                None,
                Some(PaneLaunchSpec {
                    program,
                    args: vec!["/d".into(), "/c".into(), "echo conpty-program-tail".into()],
                    cwd: None,
                    env: BTreeMap::new(),
                }),
                (17, 73),
            )
            .unwrap();
        let execution = Arc::clone(
            &prepared
                .runtime
                .as_ref()
                .unwrap()
                .panes
                .values()
                .next()
                .unwrap()
                .execution,
        );
        let _cleanup = ExecutionTestCleanup(Arc::clone(&execution));
        switchy_async::time::timeout(Duration::from_secs(5), prepared.wait_until_ready())
            .await
            .expect("ConPTY cursor negotiation must not block program startup")
            .unwrap();
        manager
            .commit_fresh_runtime(session_id, &mut prepared, || Ok(()))
            .unwrap();
        let pane = manager.runtimes[&session_id].panes.values().next().unwrap();
        switchy_async::time::timeout(Duration::from_secs(5), async {
            while !execution.output_drained.load(Ordering::SeqCst)
                || !pane.exited.load(Ordering::SeqCst)
                || !execution.terminal_owner_retired.load(Ordering::SeqCst)
            {
                switchy_async::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|error| {
            let outcome = execution.outcome.try_lock().ok()
                .map(|outcome| (outcome.0, outcome.1.is_some(), outcome.2.is_some()));
            panic!("program tail must drain and owner must retire: {error}; drained={} exited={} session_ended={} retired={} cleanup_failed={} capture_complete={} outcome={outcome:?}",
                execution.output_drained.load(Ordering::SeqCst),
                pane.exited.load(Ordering::SeqCst),
                execution.terminal_session_ended.load(Ordering::SeqCst),
                execution.terminal_owner_retired.load(Ordering::SeqCst),
                execution.cleanup_failed.load(Ordering::SeqCst),
                execution.durable_capture_complete());
        });
        assert_eq!(execution.outcome.lock().unwrap().0, Some(0));
        assert!(!execution.cleanup_failed.load(Ordering::SeqCst));
        let output = pane
            .output_buffer
            .lock()
            .unwrap()
            .read_recent_with_offsets(8192)
            .bytes;
        assert!(String::from_utf8_lossy(&output).contains("conpty-program-tail"));
    }

    // Needs coreutils printf; Windows has windows_program_startup_answers_conpty_query_then_captures_and_drains.
    #[cfg(unix)]
    #[switchy_async::test(flavor = "multi_thread", worker_threads = 2)]
    async fn program_first_pane_captures_fast_literal_argv_and_drains() {
        let host = crate::runtime::PaneRuntimeHost::detached();
        let session_id = SessionId(crate::ids::new_uuid());
        let (exit_tx, _exit_rx) = mpsc::unbounded_channel();
        let mut manager = SessionRuntimeManager::new(
            host.clone(),
            "/bin/sh".into(),
            "xterm-256color".into(),
            protocol_profile_for_term("xterm-256color"),
            true,
            None,
            PanePaddingConfig::default(),
            exit_tx,
        );
        let mut prepared = manager
            .prepare_fresh_program_runtime(
                session_id,
                None,
                Some(PaneLaunchSpec {
                    program: std::env::split_paths(&std::env::var_os("PATH").unwrap())
                        .map(|directory| directory.join("printf"))
                        .find(|path| path.is_file())
                        .expect("coreutils printf on the Nix development PATH")
                        .to_string_lossy()
                        .into_owned(),
                    args: vec!["%s".into(), "literal;$(no-shell)".into()],
                    cwd: None,
                    env: BTreeMap::new(),
                }),
                (17, 73),
            )
            .unwrap();
        prepared.wait_until_ready().await.unwrap();
        manager
            .commit_fresh_runtime(session_id, &mut prepared, || Ok(()))
            .unwrap();
        let pane = manager.runtimes[&session_id].panes.values().next().unwrap();
        let _cleanup = ExecutionTestCleanup(Arc::clone(&pane.execution));
        switchy_async::time::timeout(Duration::from_secs(5), async {
            while !pane.execution.output_drained.load(Ordering::SeqCst)
                || !pane.exited.load(Ordering::SeqCst)
            {
                switchy_async::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(pane.execution.output_watermark.load(Ordering::SeqCst), 19);
        assert_eq!(pane.execution.outcome.lock().unwrap().0, Some(0));
        assert_eq!(*pane.last_requested_size.lock().unwrap(), (17, 73));
        assert!(pane.meta.launch.is_some());
    }

    #[cfg(unix)]
    #[switchy_async::test(flavor = "multi_thread", worker_threads = 2)]
    async fn initial_shell_uses_conventional_owner_preserves_cwd_and_natural_tail() {
        use std::os::unix::fs::PermissionsExt;
        let host = crate::runtime::PaneRuntimeHost::detached();
        let root = tempfile::tempdir().unwrap();
        let shell = root.path().join("owned-shell");
        std::fs::write(&shell, "#!/bin/sh\npwd > witness\nprintf TAIL\n").unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (exit_tx, _exit_rx) = mpsc::unbounded_channel();
        let mut manager = SessionRuntimeManager::new(
            host.clone(),
            shell.to_str().unwrap().into(),
            "xterm-256color".into(),
            protocol_profile_for_term("xterm-256color"),
            true,
            None,
            PanePaddingConfig::default(),
            exit_tx,
        );
        let session_id = SessionId(crate::ids::new_uuid());
        let mut prepared = manager
            .prepare_fresh_program_runtime(
                session_id,
                Some(
                    bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::InitialShellOptions {
                        cwd: Some(root.path().to_str().unwrap().into()),
                    },
                ),
                None,
                (24, 80),
            )
            .unwrap();
        let execution = Arc::clone(
            &prepared
                .runtime
                .as_ref()
                .unwrap()
                .panes
                .values()
                .next()
                .unwrap()
                .execution,
        );
        let _cleanup = ExecutionTestCleanup(Arc::clone(&execution));
        prepared.wait_until_ready().await.unwrap();
        manager
            .commit_fresh_runtime(session_id, &mut prepared, || Ok(()))
            .unwrap();
        let pane = manager.runtimes[&session_id].panes.values().next().unwrap();
        switchy_async::time::timeout(Duration::from_secs(5), async {
            while pane.cleanup.borrow().is_none() {
                switchy_async::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(pane.cleanup.borrow().as_ref().unwrap().is_ok());
        assert!(execution.terminal_owner_retired.load(Ordering::SeqCst));
        assert!(execution.output_drained.load(Ordering::SeqCst));
        assert_eq!(execution.output_watermark.load(Ordering::SeqCst), 4);
        assert_eq!(execution.outcome.lock().unwrap().0, Some(0));
        assert!(!execution.group_kill_issued.load(Ordering::SeqCst));
        assert_eq!(
            std::fs::read_to_string(root.path().join("witness"))
                .unwrap()
                .trim(),
            root.path().canonicalize().unwrap().to_str().unwrap()
        );
    }

    // Runs /bin/sh and dd.
    #[cfg(unix)]
    #[switchy_async::test(flavor = "multi_thread", worker_threads = 2)]
    async fn conventional_session_drains_startup_backpressure_and_retires_owner() {
        let session_id = SessionId(crate::ids::new_uuid());
        let mut manager =
            manager_with_runtime(session_id, runtime_with_panes(&[crate::ids::new_uuid()]));
        manager.runtimes.clear();
        // More than the startup capture queue: publication must unblock capture
        // without mistaking leader exit for EOF or dropping the buffered tail.
        let mut prepared = manager
            .prepare_fresh_program_runtime(
                session_id,
                None,
                Some(PaneLaunchSpec {
                    program: "/bin/sh".into(),
                    args: vec![
                        "-c".into(),
                        "dd if=/dev/zero bs=8192 count=40 2>/dev/null; printf tail".into(),
                    ],
                    cwd: None,
                    env: BTreeMap::new(),
                }),
                (24, 80),
            )
            .unwrap();
        let execution = Arc::clone(
            &prepared
                .runtime
                .as_ref()
                .unwrap()
                .panes
                .values()
                .next()
                .unwrap()
                .execution,
        );

        let _cleanup = ExecutionTestCleanup(Arc::clone(&execution));
        prepared.wait_until_ready().await.unwrap();
        manager
            .commit_fresh_runtime(session_id, &mut prepared, || Ok(()))
            .unwrap();
        let pane = manager.runtimes[&session_id].panes.values().next().unwrap();
        assert!(execution.session.lock().unwrap().is_some());
        let drained = switchy_async::time::timeout(Duration::from_secs(5), async {
            while !execution.output_drained.load(Ordering::SeqCst)
                || !pane.exited.load(Ordering::SeqCst)
            {
                switchy_async::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            drained.is_ok(),
            "supervised output must reach real EOF before sweep: exited={} drained={} watermark={} outcome={:?}",
            pane.exited.load(Ordering::SeqCst),
            execution.output_drained.load(Ordering::SeqCst),
            execution.output_watermark.load(Ordering::SeqCst),
            execution.outcome.lock().unwrap()
        );
        assert_eq!(
            execution.output_watermark.load(Ordering::SeqCst),
            40 * 8192 + 4
        );
        assert_eq!(execution.outcome.lock().unwrap().0, Some(0));
        assert!(!execution.group_kill_issued.load(Ordering::SeqCst));
        switchy_async::time::timeout(Duration::from_secs(5), async {
            while pane.cleanup.borrow().is_none() {
                switchy_async::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(execution.terminal_session_ended.load(Ordering::SeqCst));
        // Natural terminal retirement must not claim a descendant group sweep.
        assert!(!execution.group_kill_issued.load(Ordering::SeqCst));
    }

    // Runs /bin/sh and dd.
    #[cfg(unix)]
    #[switchy_async::test(flavor = "multi_thread", worker_threads = 2)]
    async fn supervised_fast_exit_drains_startup_backpressure_before_retirement() {
        let session_id = SessionId(crate::ids::new_uuid());
        let mut manager =
            manager_with_runtime(session_id, runtime_with_panes(&[crate::ids::new_uuid()]));
        manager.runtimes.clear();
        // More than the startup capture queue: publication must unblock capture
        // without mistaking leader exit for EOF or dropping the buffered tail.
        let mut prepared = manager
            .prepare_fresh_program_runtime(
                session_id,
                None,
                Some(PaneLaunchSpec {
                    program: "/bin/sh".into(),
                    args: vec![
                        "-c".into(),
                        "dd if=/dev/zero bs=8192 count=40 2>/dev/null; printf tail".into(),
                    ],
                    cwd: None,
                    env: BTreeMap::new(),
                }),
                (24, 80),
            )
            .unwrap();
        let execution = Arc::clone(
            &prepared
                .runtime
                .as_ref()
                .unwrap()
                .panes
                .values()
                .next()
                .unwrap()
                .execution,
        );
        let _cleanup = ExecutionTestCleanup(Arc::clone(&execution));
        prepared.wait_until_ready().await.unwrap();
        manager
            .commit_fresh_runtime(session_id, &mut prepared, || Ok(()))
            .unwrap();
        let pane = manager.runtimes[&session_id].panes.values().next().unwrap();
        assert!(execution.session.lock().unwrap().is_some());
        let drained = switchy_async::time::timeout(Duration::from_secs(5), async {
            while execution.output_watermark.load(Ordering::SeqCst) != 40 * 8192 + 4
                || !pane.exited.load(Ordering::SeqCst)
            {
                switchy_async::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            drained.is_ok(),
            "legacy capture must retain every byte and target outcome before explicit sweep: exited={} drained={} watermark={} outcome={:?}",
            pane.exited.load(Ordering::SeqCst),
            execution.output_drained.load(Ordering::SeqCst),
            execution.output_watermark.load(Ordering::SeqCst),
            execution.outcome.lock().unwrap()
        );
        assert_eq!(
            execution.output_watermark.load(Ordering::SeqCst),
            40 * 8192 + 4
        );
        assert_eq!(execution.outcome.lock().unwrap().0, Some(0));
        // Conventional owner retirement must yield natural EOF without Sweep.
        // Completion is the reader watermark, never target exit or tcdrain.
        assert!(!execution.group_kill_issued.load(Ordering::SeqCst));
        switchy_async::time::timeout(Duration::from_secs(5), async {
            while !execution.output_drained.load(Ordering::SeqCst)
                || pane.cleanup.borrow().is_none()
            {
                switchy_async::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("authorized sweep must release real EOF and acknowledge supervision");
        assert_eq!(
            execution.output_watermark.load(Ordering::SeqCst),
            40 * 8192 + 4
        );
        assert_eq!(execution.outcome.lock().unwrap().0, Some(0));
        assert!(pane.cleanup.borrow().as_ref().unwrap().is_ok());
        switchy_async::time::timeout(Duration::from_secs(5), async {
            while pane.cleanup.borrow().is_none() {
                switchy_async::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("supervised cleanup acknowledgement");
        assert_eq!(*pane.cleanup.borrow(), Some(Ok(())));
        assert_eq!(execution.outcome.lock().unwrap().0, Some(0));
        assert!(execution.outcome.lock().unwrap().2.is_none());
    }

    struct ExecutionTestCleanup(Arc<execution::ExecutionCapture>);
    impl Drop for ExecutionTestCleanup {
        fn drop(&mut self) {
            let host = crate::runtime::PaneRuntimeHost::detached();
            let host = &host;
            if self.0.terminal_owner_retired.load(Ordering::SeqCst) {
                return;
            }
            if let Err(error) = self
                .0
                .signal_owned_group(host, crate::platform::StopAction::Force)
            {
                eprintln!("test process cleanup requires recovery: {error}");
            }
        }
    }

    #[cfg(unix)]
    struct WitnessProcessCleanup {
        pid: u32,
        identity: ProcessStartIdentity,
    }
    #[cfg(unix)]
    impl Drop for WitnessProcessCleanup {
        fn drop(&mut self) {
            if process_start_identity_for_pid(self.pid) == Some(self.identity)
                && let Ok(pid) = i32::try_from(self.pid)
            {
                // SAFETY: test-owned witness PID is checked against its start identity.
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }

    #[cfg(unix)]
    fn prepare_escaped_terminal_witness(
        manager: &SessionRuntimeManager,
        session_id: SessionId,
        witness: &std::path::Path,
    ) -> PreparedFreshRuntime {
        manager
            .prepare_fresh_program_runtime(
                session_id,
                None,
                Some(PaneLaunchSpec {
                    program: "/bin/sh".into(),
                    args: vec![
                        "-c".into(),
                        r#"trap '' HUP TERM
python3 -c 'import os,signal,time,errno,sys
root=sys.argv[1]
original_sid=os.getsid(0)
if os.fork(): os._exit(0)
assert os.getpid()!=os.getpgrp()
os.setsid()
assert os.getsid(0)!=original_sid
open(root+"/original.sid","w").write(str(original_sid))
for sig in (signal.SIGHUP,signal.SIGTERM): signal.signal(sig,signal.SIG_IGN)
open(root+"/descendant.pid","w").write(str(os.getpid()))
open(root+"/ready","w").close()
while not os.path.exists(root+"/probe"): time.sleep(0.01)
try:
 os.write(1,b"UNAUTHORIZED-LATE-OUTPUT")
 result="unexpected writable terminal"
except OSError as error: result=str(error.errno)
open(root+"/late-result.tmp","w").write(result)
os.rename(root+"/late-result.tmp",root+"/late-result")
while not os.path.exists(root+"/release"): time.sleep(0.01)
for fd in (0,1,2): os.close(fd)
open(root+"/released","w").close()
while True: time.sleep(1)' "$1" < /dev/tty > /dev/tty 2>&1 &
while [ ! -f "$1/ready" ]; do sleep 0.01; done
printf TAIL
exit 7"#
                            .into(),
                        "fixture".into(),
                        witness.to_string_lossy().into_owned(),
                    ],
                    cwd: None,
                    env: BTreeMap::new(),
                }),
                (24, 80),
            )
            .unwrap()
    }

    #[cfg(unix)]
    #[switchy_async::test(flavor = "multi_thread", worker_threads = 2)]
    // Keep the real owner/descendant/capture ordering visible in one acceptance
    // fixture; splitting it obscures which boundary each assertion observes.
    #[allow(clippy::too_many_lines)]
    async fn ended_terminal_preserves_escaped_descendant_without_late_signal_authority() {
        let host = crate::runtime::PaneRuntimeHost::detached();
        let host = &host;
        let session_id = SessionId(crate::ids::new_uuid());
        let mut manager =
            manager_with_runtime(session_id, runtime_with_panes(&[crate::ids::new_uuid()]));
        manager.runtimes.clear();
        let witness = tempfile::tempdir().unwrap();
        let mut prepared = prepare_escaped_terminal_witness(&manager, session_id, witness.path());
        let _cleanup = ExecutionTestCleanup(Arc::clone(
            &prepared
                .runtime
                .as_ref()
                .unwrap()
                .panes
                .values()
                .next()
                .unwrap()
                .execution,
        ));
        let capture_root = tempfile::tempdir().unwrap();
        *prepared
            .runtime
            .as_ref()
            .unwrap()
            .panes
            .values()
            .next()
            .unwrap()
            .execution
            .durable_capture
            .lock()
            .unwrap() = Some(durable_capture::DurableCapture::create(capture_root.path()).unwrap());
        prepared.wait_until_ready().await.unwrap();
        manager
            .commit_fresh_runtime(session_id, &mut prepared, || Ok(()))
            .unwrap();
        let pane = manager.runtimes[&session_id].panes.values().next().unwrap();
        switchy_async::time::timeout(Duration::from_secs(5), async {
            while !pane.exited.load(Ordering::SeqCst) {
                switchy_async::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let descendant: u32 = std::fs::read_to_string(witness.path().join("descendant.pid"))
            .unwrap()
            .parse()
            .unwrap();
        let identity = process_start_identity_for_pid(descendant)
            .expect("descendant must remain live after leader exit");
        let original_sid: i32 = std::fs::read_to_string(witness.path().join("original.sid"))
            .unwrap()
            .parse()
            .unwrap();
        let descendant_pid = i32::try_from(descendant).unwrap();
        // SAFETY: identity-checked test child; verify this really escaped the
        // terminal session rather than weakening same-session group cleanup.
        let actual_sid = unsafe { libc::getsid(descendant_pid) };
        assert_eq!(actual_sid, descendant_pid);
        assert_ne!(actual_sid, original_sid);
        let _witness_cleanup = WitnessProcessCleanup {
            pid: descendant,
            identity,
        };
        // Retiring the exact owner must not depend on a descendant closing the
        // PTY. Neither retirement nor session end is a capture EOF receipt.
        switchy_async::time::timeout(Duration::from_secs(5), async {
            while !pane.execution.terminal_owner_retired.load(Ordering::SeqCst) {
                switchy_async::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("owner retirement independent of capture EOF");
        assert!(pane.execution.terminal_session_ended.load(Ordering::SeqCst));
        assert_eq!(pane.execution.outcome.lock().unwrap().0, Some(7));
        assert!(!pane.execution.group_kill_issued.load(Ordering::SeqCst));
        assert!(
            !pane
                .execution
                .signal_owned_group(host, crate::platform::StopAction::Cooperative)
                .unwrap()
        );
        assert!(
            !pane
                .execution
                .signal_owned_group(host, crate::platform::StopAction::Force)
                .unwrap()
        );
        assert_eq!(process_start_identity_for_pid(descendant), Some(identity));
        std::fs::write(witness.path().join("probe"), b"").unwrap();
        switchy_async::time::timeout(Duration::from_secs(5), async {
            while !witness.path().join("late-result").exists() {
                switchy_async::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("surviving witness probes its retained terminal");
        let late_result = std::fs::read_to_string(witness.path().join("late-result")).unwrap();
        #[cfg(target_os = "linux")]
        {
            assert_eq!(late_result, "unexpected writable terminal");
            assert!(!pane.execution.output_drained.load(Ordering::SeqCst));
            assert!(pane.cleanup.borrow().is_none());
            let (boundary, _) =
                durable_capture::DurableCapture::read(capture_root.path(), 0, 128).unwrap();
            assert!(!boundary.eof);
            assert!(boundary.error.is_none());
        }
        // Other kernels can revoke the slave on session-leader exit. Capture
        // must report the observed read boundary rather than assume either path.
        assert!(
            late_result == "unexpected writable terminal" || late_result == libc::EIO.to_string(),
            "{late_result}"
        );
        assert_eq!(process_start_identity_for_pid(descendant), Some(identity));
        std::fs::write(witness.path().join("release"), b"").unwrap();
        switchy_async::time::timeout(Duration::from_secs(5), async {
            while !witness.path().join("released").exists()
                || !matches!(&*pane.cleanup.borrow(), Some(Ok(())))
            {
                switchy_async::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("actual descriptor release permits capture EOF and cleanup");
        assert!(pane.execution.output_drained.load(Ordering::SeqCst));
        let (boundary, bytes) =
            durable_capture::DurableCapture::read(capture_root.path(), 0, 128).unwrap();
        let expected: &[u8] = if late_result == "unexpected writable terminal" {
            b"TAILUNAUTHORIZED-LATE-OUTPUT"
        } else {
            b"TAIL"
        };
        assert!(boundary.eof);
        assert!(boundary.error.is_none());
        assert_eq!(bytes, expected);
        assert_eq!(boundary.bytes, expected.len() as u64);
        assert_eq!(
            pane.execution.output_watermark.load(Ordering::SeqCst),
            expected.len() as u64
        );
        assert!(pane.execution.outcome.lock().unwrap().2.is_none());
        assert!(
            pane.execution
                .cleanup_complete(matches!(&*pane.cleanup.borrow(), Some(Ok(()))))
        );
        assert!(pane.execution.master.lock().unwrap().is_none());
        assert!(!pane.execution.group_kill_issued.load(Ordering::SeqCst));
        assert_eq!(process_start_identity_for_pid(descendant), Some(identity));
        // Only WitnessProcessCleanup may terminate this identity-checked fixture.
    }

    #[cfg(unix)]
    #[switchy_async::test(flavor = "multi_thread", worker_threads = 2)]
    async fn injected_reader_failure_cleans_live_process_and_acknowledges_threads() {
        let session_id = SessionId(crate::ids::new_uuid());
        let mut manager =
            manager_with_runtime(session_id, runtime_with_panes(&[crate::ids::new_uuid()]));
        manager.runtimes.clear();
        let mut prepared = manager
            .prepare_fresh_program_runtime(
                session_id,
                None,
                Some(PaneLaunchSpec {
                    program: "/bin/sh".into(),
                    args: vec!["-c".into(), "while :; do printf x; sleep 0.05; done".into()],
                    cwd: None,
                    env: BTreeMap::new(),
                }),
                (24, 80),
            )
            .unwrap();
        let _cleanup = ExecutionTestCleanup(Arc::clone(
            &prepared
                .runtime
                .as_ref()
                .unwrap()
                .panes
                .values()
                .next()
                .unwrap()
                .execution,
        ));
        prepared
            .runtime
            .as_ref()
            .unwrap()
            .panes
            .values()
            .next()
            .unwrap()
            .execution
            .inject_reader_failure
            .store(true, Ordering::SeqCst);
        prepared.wait_until_ready().await.unwrap();
        manager
            .commit_fresh_runtime(session_id, &mut prepared, || Ok(()))
            .unwrap();
        let pane = manager.runtimes[&session_id].panes.values().next().unwrap();
        switchy_async::time::timeout(Duration::from_secs(5), async {
            while pane.cleanup.borrow().is_none() {
                switchy_async::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("reader cleanup acknowledgement timed out");
        let cleanup = pane
            .cleanup
            .borrow()
            .clone()
            .expect("cleanup acknowledgement");
        assert!(
            cleanup.is_ok(),
            "reader cleanup explicitly failed: {cleanup:?}; outcome: {:?}",
            pane.execution.outcome.lock().unwrap()
        );
        assert!(pane.exited.load(Ordering::SeqCst));
        assert_eq!(
            pane.execution.outcome.lock().unwrap().1.as_deref(),
            Some("9"),
            "original target SIGKILL outcome must survive owner retirement"
        );
        assert!(pane.execution.target_stop_issued.load(Ordering::SeqCst));
        assert!(pane.execution.terminal_owner_retired.load(Ordering::SeqCst));
        assert!(
            pane.execution.group_kill_issued.load(Ordering::SeqCst),
            "a forced stop sweeps and verifies the whole terminal session"
        );
        assert!(!pane.execution.output_drained.load(Ordering::SeqCst));
        assert!(
            pane.execution
                .outcome
                .lock()
                .unwrap()
                .2
                .as_ref()
                .unwrap()
                .contains("injected")
        );
    }

    #[test]
    fn spawn_cleanup_preserves_typed_denial_and_reports_cleanup_failure() {
        use bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SpawnError;
        let denied = || {
            super::spawn_failure(SpawnError::Denied {
                reason: "revoked".into(),
            })
        };
        assert!(
            matches!(super::spawn_wire_error(super::spawn_error_after_cleanup(denied(), Vec::new())), SpawnError::Denied { reason } if reason == "revoked")
        );
        assert!(
            matches!(super::spawn_wire_error(super::spawn_error_after_cleanup(denied(), vec!["reader still live".into()])), SpawnError::CleanupFailed { reason } if reason.contains("revoked") && reason.contains("reader still live"))
        );
    }

    #[switchy_async::test]
    async fn supervision_join_reports_live_thread_deadline_without_blocking_executor() {
        let (release, blocked) = switchy_thread::sync::mpsc::channel();
        let (done, finished) = tokio::sync::oneshot::channel();
        let thread = switchy_thread::thread::spawn(move || {
            let _ = blocked.recv();
            let _ = done.send(());
        });
        let mut threads = [Some(thread), None];
        let mut join = super::supervision_join(&mut threads);
        let result = super::join_supervision_threads(&mut join, std::time::Duration::ZERO).await;
        assert!(result.unwrap_err().contains("deadline"));
        assert!(
            !join.is_finished(),
            "deadline must retain the live OS join owner"
        );
        release.send(()).unwrap();
        finished.await.unwrap();
        super::join_supervision_threads(&mut join, std::time::Duration::from_secs(2))
            .await
            .unwrap();
        assert!(threads.iter().all(Option::is_none));
    }

    #[switchy_async::test]
    async fn supervision_join_acknowledges_completion_and_reports_panics() {
        let thread = switchy_thread::thread::spawn(|| {});
        super::join_supervision_threads(
            &mut super::supervision_join(&mut [Some(thread), None]),
            std::time::Duration::from_secs(2),
        )
        .await
        .unwrap();
        let thread = switchy_thread::thread::spawn(|| panic!("supervision failure"));
        assert!(
            super::join_supervision_threads(
                &mut super::supervision_join(&mut [Some(thread), None]),
                std::time::Duration::from_secs(2)
            )
            .await
            .unwrap_err()
            .contains("panicked")
        );
    }

    use super::*;
    // Native checks of the platform layer's Unix target.
    #[cfg(unix)]
    use crate::platform::{
        bounded_probe_output, parse_ps_process_entry,
        process_directory as observe_process_directory, send_process_signal_command,
    };

    #[test]
    fn nested_program_environment_cannot_inherit_managed_launch_authority() {
        let mut command = SpawnCommand::new("sh");
        for key in [
            "BMUX_MANAGED_TARGET_IDENTITY",
            "BMUX_MANAGED_TARGET_ID",
            "BMUX_MANAGED_IDLE_GRACE_SECONDS",
        ] {
            command.env(key, "parent-owned");
        }
        command.env("BMUX_MANAGED_EXECUTABLE_SHA256", "parent-owned");
        clear_managed_launch_env(&mut command);
        assert!(command.get_env("BMUX_MANAGED_EXECUTABLE_SHA256").is_none());
        for key in [
            "BMUX_MANAGED_TARGET_IDENTITY",
            "BMUX_MANAGED_TARGET_ID",
            "BMUX_MANAGED_IDLE_GRACE_SECONDS",
        ] {
            assert!(command.get_env(key).is_none());
        }
    }

    #[test]
    fn pane_terminal_environment_hides_outer_host_and_advertises_virtual_images() {
        let mut command = SpawnCommand::new("sh");
        command.env("TERM_PROGRAM", "ghostty");
        command.env("LC_TERMINAL", "iTerm2");
        command.env("KITTY_WINDOW_ID", "42");

        configure_pane_terminal_env(&mut command, "bmux-256color", Some("kitty"));

        assert_eq!(
            command.get_env("TERM"),
            Some(std::ffi::OsStr::new("bmux-256color"))
        );
        assert_eq!(
            command.get_env("TERM_PROGRAM"),
            Some(std::ffi::OsStr::new("bmux"))
        );
        assert_eq!(
            command.get_env("BMUX_PANE"),
            Some(std::ffi::OsStr::new("1"))
        );
        assert_eq!(
            command.get_env("BMUX_IMAGE_PROTOCOL"),
            Some(std::ffi::OsStr::new("kitty"))
        );
        assert_eq!(command.get_env("LC_TERMINAL"), None);
        assert_eq!(command.get_env("KITTY_WINDOW_ID"), None);
    }

    #[test]
    fn pane_terminal_environment_does_not_advertise_disabled_images() {
        let mut command = SpawnCommand::new("sh");
        command.env("BMUX_IMAGE_PROTOCOL", "kitty");

        configure_pane_terminal_env(&mut command, "bmux-256color", None);

        assert_eq!(command.get_env("BMUX_IMAGE_PROTOCOL"), None);
    }

    fn leaf(id: Uuid) -> PaneLayoutNode {
        PaneLayoutNode::Leaf { pane_id: id }
    }

    #[switchy_async::test]
    async fn zoom_transition_reselects_padding_rule_and_restores_split_size() {
        let session_id = SessionId(crate::ids::new_uuid());
        let first = crate::ids::new_uuid();
        let second = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[first, second]);
        runtime.layout_root = Some(PaneLayoutNode::Split {
            direction: PaneSplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(leaf(first)),
            second: Box::new(leaf(second)),
        });
        let mut manager = manager_with_runtime(session_id, runtime);
        manager.padding_config = PanePaddingConfig::parse(Some(
            &toml::toml! {
                [padding]
                [[padding.pane_rules]]
                min_width = 100
                max_content_width = 40
                horizontal_alignment = "center"
            }
            .into(),
        ))
        .unwrap();
        manager.apply_stored_attach_viewport(session_id);
        let last_requested_size =
            Arc::clone(&manager.runtimes[&session_id].panes[&first].last_requested_size);
        assert_eq!(*last_requested_size.lock().unwrap(), (38, 58));

        let (_, zoomed) = manager.toggle_zoom(session_id).expect("zoom pane");
        assert!(zoomed);
        assert_eq!(*last_requested_size.lock().unwrap(), (38, 40));
        let (_, zoomed) = manager.toggle_zoom(session_id).expect("unzoom pane");
        assert!(!zoomed);
        assert_eq!(*last_requested_size.lock().unwrap(), (38, 58));
    }

    #[switchy_async::test]
    async fn floating_resize_reselects_padding_rule_and_resizes_pty() {
        let session_id = SessionId(crate::ids::new_uuid());
        let tiled = crate::ids::new_uuid();
        let floating = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[tiled, floating]);
        runtime.layout_root = Some(PaneLayoutNode::Leaf { pane_id: tiled });
        runtime.floating_surfaces = vec![floating_surface(floating, FloatingPaneScope::PerSession)];
        let mut manager = manager_with_runtime(session_id, runtime);
        manager.padding_config = PanePaddingConfig::parse(Some(
            &toml::toml! {
                [padding]
                [[padding.pane_rules]]
                min_width = 18
                max_content_width = 10
            }
            .into(),
        ))
        .unwrap();
        let last_requested_size =
            Arc::clone(&manager.runtimes[&session_id].panes[&floating].last_requested_size);
        manager.apply_stored_attach_viewport(session_id);
        assert_eq!(*last_requested_size.lock().unwrap(), (3, 8));

        manager
            .resize_floating_pane(session_id, floating, 20, 8)
            .expect("resize floating pane across rule boundary");

        assert_eq!(*last_requested_size.lock().unwrap(), (6, 10));
        let state = manager.padding_state(session_id, Some(floating)).unwrap();
        assert_eq!(state.matched_rule_index, Some(0));
        assert_eq!(state.effective_content_rect.w, 10);
    }

    #[switchy_async::test]
    async fn padding_geometry_is_shared_across_attached_clients() {
        let session_id = SessionId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let first_client = ClientId(crate::ids::new_uuid());
        let second_client = ClientId(crate::ids::new_uuid());
        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime
            .attached_clients
            .extend([first_client, second_client]);
        let mut manager = manager_with_runtime(session_id, runtime);
        let spec = PanePaddingSpec {
            max_content_width: Some(40),
            horizontal_alignment: crate::padding::HorizontalAlignment::Center,
            ..PanePaddingSpec::default()
        };
        manager
            .set_padding_override(session_id, Some(pane_id), Some(spec))
            .expect("set shared pane override");

        let first = manager
            .build_attach_scene_for_client(session_id, first_client)
            .expect("first client scene");
        let second = manager
            .build_attach_scene_for_client(session_id, second_client)
            .expect("second client scene");

        assert_eq!(
            first.surfaces[0].content_rect,
            second.surfaces[0].content_rect
        );
        assert_eq!(first.surfaces[0].content_rect.w, 40);
        assert_eq!(
            manager.runtimes[&session_id].panes[&pane_id].padding_override,
            Some(spec)
        );
    }

    #[switchy_async::test]
    async fn begin_attach_registers_input_producer_before_accepting_input() {
        let host = crate::runtime::PaneRuntimeHost::detached();
        let host = &host;
        let session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane_id]);
        let (input_tx, mut input_rx) = mpsc::unbounded_channel();
        runtime.panes.get_mut(&pane_id).unwrap().input_tx = input_tx;
        let mut manager = manager_with_runtime(session_id, runtime);

        manager.begin_attach(session_id, client_id).unwrap();
        assert_eq!(
            manager.write_input(session_id, client_id, b"attached input".to_vec()),
            Ok((14, pane_id))
        );
        assert!(
            std::iter::from_fn(|| input_rx.try_recv().ok()).any(|command| {
                matches!(
                    command,
                    PaneRuntimeCommand::Input(bytes) if bytes == b"attached input"
                )
            })
        );
        manager.end_attach(session_id, client_id);
        assert!(
            crate::private_input::with_producer_admission(host, pane_id, client_id.0, || ())
                .is_none()
        );
        assert!(
            manager
                .write_input(session_id, client_id, b"denied".to_vec())
                .is_err()
        );
        let pane = &manager.runtimes[&session_id].panes[&pane_id];
        assert!(!pane.exited.load(Ordering::SeqCst));
        assert!(!pane.execution.target_stop_issued.load(Ordering::SeqCst));
        assert!(!pane.execution.group_kill_issued.load(Ordering::SeqCst));
        manager.begin_attach(session_id, client_id).unwrap();
        assert_eq!(
            manager.write_input(session_id, client_id, b"again".to_vec()),
            Ok((5, pane_id))
        );
    }

    #[switchy_async::test]
    async fn begin_attach_rejects_unregistered_producer_during_private_input() {
        let session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let existing_client = crate::ids::new_uuid();
        let pane_id = crate::ids::new_uuid();
        let nonce = crate::ids::new_uuid();
        let runtime = runtime_with_panes(&[pane_id]);
        let mut manager = manager_with_runtime(session_id, runtime);
        // The manager owns the activation host the test observes through.
        let host = manager.host.clone();
        let host = &host;
        assert!(crate::private_input::register_producer(
            host,
            pane_id,
            existing_client
        ));
        let generation = crate::private_input::begin_barrier(host, pane_id, nonce, true).unwrap();

        // An in-flight barrier may admit membership only by invalidating its
        // proof; a completed private interval must reject a new producer.
        assert!(crate::private_input::acknowledge_producer(
            host,
            pane_id,
            nonce,
            generation,
            existing_client,
            true
        ));
        assert!(crate::private_input::finish_barrier(
            host, pane_id, nonce, generation
        ));
        assert_eq!(
            manager.begin_attach(session_id, client_id),
            Err(SessionRuntimeError::Closed)
        );
        assert!(
            !manager.runtimes[&session_id]
                .attached_clients
                .contains(&client_id)
        );
    }

    #[switchy_async::test]
    async fn failed_retarget_preserves_previous_attach_registration() {
        let previous_session_id = SessionId(crate::ids::new_uuid());
        let missing_session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime.attached_clients.insert(client_id);
        let manager = manager_with_runtime(previous_session_id, runtime);
        let adapter = adapter_for_manager(manager);

        let result = bmux_pane_runtime_state::SessionRuntimeManagerApi::retarget_attach_stream(
            &adapter,
            Some(previous_session_id),
            missing_session_id,
            client_id,
            80,
            24,
            0,
            0,
            0,
            0,
            0,
            0,
        );
        assert_eq!(result, Err(SessionRuntimeError::NotFound));
        let manager = adapter.inner.lock().expect("runtime manager lock");
        assert!(
            manager.runtimes[&previous_session_id]
                .attached_clients
                .contains(&client_id)
        );
    }

    #[switchy_async::test]
    async fn over_reserved_viewport_normalizes_once_for_scene_and_response() {
        let session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime.attached_clients.insert(client_id);
        let mut manager = manager_with_runtime(session_id, runtime);

        let response = manager
            .set_attach_viewport(session_id, client_id, 10, 5, 4, 9, 4, 9, 0, 0)
            .expect("set over-reserved viewport");
        assert_eq!(response, (10, 5, 4, 0, 0, 9));
        assert_eq!(
            manager.runtimes[&session_id].attach_viewport,
            Some(AttachViewport::normalized(10, 5, 4, 9, 4, 9))
        );
        let scene = manager
            .build_attach_scene_for_client(session_id, client_id)
            .expect("build clamped attach scene");
        assert_eq!(
            scene.surfaces[0].rect,
            AttachRect {
                x: 9,
                y: 4,
                w: 1,
                h: 1,
            }
        );
    }

    #[switchy_async::test]
    async fn horizontal_viewport_insets_drive_scene_geometry_and_pty_size() {
        let session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime.attached_clients.insert(client_id);
        let mut manager = manager_with_runtime(session_id, runtime);
        let last_requested_size =
            Arc::clone(&manager.runtimes[&session_id].panes[&pane_id].last_requested_size);

        let viewport = manager
            .set_attach_viewport(session_id, client_id, 120, 40, 2, 7, 3, 11, 0, 0)
            .expect("set four-edge viewport");

        assert_eq!(viewport, (120, 40, 2, 7, 3, 11));
        assert_eq!(*last_requested_size.lock().unwrap(), (33, 100));
        let scene = manager
            .build_attach_scene_for_client(session_id, client_id)
            .expect("build attach scene");
        let surface = scene
            .surfaces
            .iter()
            .find(|surface| surface.pane_id == Some(pane_id))
            .expect("pane surface");
        assert_eq!(
            surface.rect,
            AttachRect {
                x: 11,
                y: 2,
                w: 102,
                h: 35,
            }
        );
    }

    #[switchy_async::test]
    async fn observer_open_preserves_focus_and_pty_size_and_cannot_escalate() {
        use crate::handlers::attach_commands::{mark_test_observer, observer_operation_allowed};
        let session = SessionId(crate::ids::new_uuid());
        let owner = ClientId(crate::ids::new_uuid());
        let observer = ClientId(crate::ids::new_uuid());
        let pane = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane]);
        runtime.attached_clients.insert(owner);
        let mut manager = manager_with_runtime(session, runtime);
        // The manager owns the activation host the test observes through.
        let host = manager.host.clone();
        let host = &host;
        manager
            .set_attach_viewport(session, owner, 120, 40, 2, 7, 3, 11, 0, 0)
            .unwrap();
        let viewport = manager.runtimes[&session].attach_viewport;
        let focus = manager.runtimes[&session].focused_pane_id;
        let size = *manager.runtimes[&session].panes[&pane]
            .last_requested_size
            .lock()
            .unwrap();
        mark_test_observer(host, observer, session);
        manager.begin_observer_attach(session, observer).unwrap();
        assert_eq!(manager.runtimes[&session].focused_pane_id, focus);
        assert_eq!(manager.runtimes[&session].attach_viewport, viewport);
        assert_eq!(
            *manager.runtimes[&session].panes[&pane]
                .last_requested_size
                .lock()
                .unwrap(),
            size
        );
        assert!(manager.begin_attach(session, observer).is_err());
        assert!(
            manager
                .set_attach_viewport(session, observer, 9, 9, 0, 0, 0, 0, 0, 0)
                .is_err()
        );
        assert!(
            manager
                .write_input(session, observer, b"not allowed".to_vec())
                .is_err()
        );
        for operation in [
            "attach-open",
            "attach-session",
            "attach-context",
            "attach-retarget-context",
            "attach-input",
            "attach-set-viewport",
        ] {
            assert!(!observer_operation_allowed(
                host,
                observer,
                "attach-runtime-commands",
                operation
            ));
        }
        assert!(!observer_operation_allowed(
            host,
            observer,
            "pane-runtime-commands",
            "focus-pane"
        ));
        assert!(!observer_operation_allowed(
            host,
            observer,
            "pane-runtime-commands",
            "pane-direct-input"
        ));
        // Even a detached fallback does not establish shared sizing authority.
        manager.runtimes.get_mut(&session).unwrap().attach_viewport = None;
        let fallback = manager.observe_viewport(session, observer, 80, 24).unwrap();
        assert_eq!((fallback.cols, fallback.rows), (80, 24));
        assert!(manager.runtimes[&session].attach_viewport.is_none());
        assert_eq!(
            *manager.runtimes[&session].panes[&pane]
                .last_requested_size
                .lock()
                .unwrap(),
            size
        );
        assert_eq!(manager.runtimes[&session].focused_pane_id, focus);
        // Ordinary owner still has the unchanged interactive path.
        manager.begin_attach(session, owner).unwrap();
        manager
            .set_attach_viewport(session, owner, 100, 30, 0, 0, 0, 0, 0, 0)
            .unwrap();
    }

    #[switchy_async::test]
    async fn interactive_attach_registers_producers_without_bypassing_private_barriers() {
        let session = SessionId(crate::ids::new_uuid());
        let client = ClientId(crate::ids::new_uuid());
        let newcomer = ClientId(crate::ids::new_uuid());
        let panes = [crate::ids::new_uuid(), crate::ids::new_uuid()];
        let mut manager = manager_with_runtime(session, runtime_with_panes(&panes));
        // The manager owns the activation host the test observes through.
        let host = manager.host.clone();
        let host = &host;
        manager.begin_attach(session, client).unwrap();
        for pane in panes {
            assert_eq!(
                crate::private_input::with_producer_admission(host, pane, client.0, || true),
                Some(true)
            );
        }
        let nonce = crate::ids::new_uuid();
        let generation = crate::private_input::begin_barrier(host, panes[0], nonce, true).unwrap();
        // Repeated admission/attachment does not clear the private transition.
        manager.begin_attach(session, client).unwrap();
        assert!(
            crate::private_input::with_producer_admission(host, panes[0], client.0, || ())
                .is_none()
        );
        assert!(crate::private_input::acknowledge_producer(
            host, panes[0], nonce, generation, client.0, true
        ));
        assert!(crate::private_input::finish_barrier(
            host, panes[0], nonce, generation
        ));
        assert!(manager.begin_attach(session, newcomer).is_err());
        assert!(
            !manager.runtimes[&session]
                .attached_clients
                .contains(&newcomer)
        );
        assert!(
            crate::private_input::with_producer_admission(host, panes[0], newcomer.0, || ())
                .is_none()
        );
    }

    #[switchy_async::test]
    async fn observation_preserves_sizing_authority_and_last_detached_geometry() {
        let session = SessionId(crate::ids::new_uuid());
        let owner = ClientId(crate::ids::new_uuid());
        let observer = ClientId(crate::ids::new_uuid());
        let pane = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane]);
        runtime.attached_clients.extend([owner, observer]);
        runtime.attach_viewport = None;
        let mut manager = manager_with_runtime(session, runtime);
        // An unset viewport is seeded once, never reasserted by observation.
        let seeded = manager.observe_viewport(session, observer, 80, 24).unwrap();
        assert_eq!((seeded.cols, seeded.rows), (80, 24));
        manager
            .set_attach_viewport(session, owner, 120, 40, 2, 7, 3, 11, 0, 0)
            .unwrap();
        let expected = manager.runtimes[&session].attach_viewport.unwrap();
        let size = *manager.runtimes[&session].panes[&pane]
            .last_requested_size
            .lock()
            .unwrap();
        for _ in 0..3 {
            assert_eq!(
                manager.observe_viewport(session, observer, 80, 24).unwrap(),
                expected
            );
            assert_eq!(
                *manager.runtimes[&session].panes[&pane]
                    .last_requested_size
                    .lock()
                    .unwrap(),
                size
            );
        }
        manager.end_attach(session, owner);
        assert_eq!(
            manager.observe_viewport(session, observer, 90, 30).unwrap(),
            expected
        );
        assert!(matches!(
            manager.observe_viewport(session, ClientId(crate::ids::new_uuid()), 80, 24),
            Err(SessionRuntimeError::NotAttached)
        ));
    }

    #[switchy_async::test]
    async fn viewport_height_resize_reselects_padding_rule_and_resizes_pty() {
        let session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime.attached_clients.insert(client_id);
        let mut manager = manager_with_runtime(session_id, runtime);
        manager.padding_config = PanePaddingConfig::parse(Some(
            &toml::toml! {
                [padding]
                [[padding.pane_rules]]
                min_height = 40
                max_content_height = 20
                vertical_alignment = "center"
            }
            .into(),
        ))
        .unwrap();
        let last_requested_size =
            Arc::clone(&manager.runtimes[&session_id].panes[&pane_id].last_requested_size);

        manager
            .set_attach_viewport(session_id, client_id, 80, 30, 0, 0, 0, 0, 0, 0)
            .expect("set short viewport");
        assert_eq!(*last_requested_size.lock().unwrap(), (28, 78));
        manager
            .set_attach_viewport(session_id, client_id, 80, 50, 0, 0, 0, 0, 0, 0)
            .expect("set tall viewport");
        assert_eq!(*last_requested_size.lock().unwrap(), (20, 78));
        let state = manager.padding_state(session_id, Some(pane_id)).unwrap();
        assert_eq!(state.effective_content_rect.y, 15);
    }

    #[switchy_async::test]
    async fn viewport_resize_reselects_padding_rule_and_resizes_pty() {
        let session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime.attached_clients.insert(client_id);
        let mut manager = manager_with_runtime(session_id, runtime);
        manager.padding_config = PanePaddingConfig::parse(Some(
            &toml::toml! {
                [padding]
                [[padding.pane_rules]]
                min_width = 100
                max_content_width = 40
                horizontal_alignment = "center"
            }
            .into(),
        ))
        .unwrap();
        let last_requested_size =
            Arc::clone(&manager.runtimes[&session_id].panes[&pane_id].last_requested_size);

        manager
            .set_attach_viewport(session_id, client_id, 90, 30, 0, 0, 0, 0, 0, 0)
            .expect("set narrow viewport");
        assert_eq!(*last_requested_size.lock().unwrap(), (28, 88));
        manager
            .set_attach_viewport(session_id, client_id, 120, 30, 0, 0, 0, 0, 0, 0)
            .expect("set wide viewport");
        assert_eq!(*last_requested_size.lock().unwrap(), (28, 40));
    }

    #[switchy_async::test]
    async fn floating_geometry_remains_host_viewport_relative_with_reserved_content_edges() {
        let session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let tiled = crate::ids::new_uuid();
        let floating = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[tiled, floating]);
        runtime.layout_root = Some(PaneLayoutNode::Leaf { pane_id: tiled });
        runtime.attach_viewport = Some(AttachViewport::normalized(100, 30, 2, 3, 4, 10));
        let mut floating_surface = floating_surface(floating, FloatingPaneScope::PerSession);
        floating_surface.rect = LayoutRect {
            x: 1,
            y: 1,
            w: 20,
            h: 8,
        };
        runtime.floating_surfaces = vec![floating_surface];

        let scene = build_attach_scene(
            session_id,
            &runtime,
            &PanePaddingConfig::default(),
            runtime.attach_viewport,
            client_id,
            None,
        );
        let tiled_surface = scene
            .surfaces
            .iter()
            .find(|surface| surface.pane_id == Some(tiled))
            .expect("tiled surface");
        assert_eq!(
            tiled_surface.rect,
            AttachRect {
                x: 10,
                y: 2,
                w: 87,
                h: 24,
            }
        );
        let floating_surface = scene
            .surfaces
            .iter()
            .find(|surface| surface.pane_id == Some(floating))
            .expect("floating surface");
        assert_eq!(
            floating_surface.rect,
            AttachRect {
                x: 1,
                y: 1,
                w: 20,
                h: 8,
            }
        );
    }

    #[switchy_async::test]
    async fn attach_scene_uses_authoritative_padding_for_tiled_zoomed_and_floating_panes() {
        let session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let tiled = crate::ids::new_uuid();
        let floating = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[tiled, floating]);
        runtime.layout_root = Some(PaneLayoutNode::Leaf { pane_id: tiled });
        runtime.floating_surfaces = vec![floating_surface(floating, FloatingPaneScope::PerSession)];
        let config = PanePaddingConfig::parse(Some(
            &toml::toml! {
                [padding]
                left = 1
                max_content_width = 40
                horizontal_alignment = "center"
            }
            .into(),
        ))
        .unwrap();
        let viewport = runtime.attach_viewport;

        let scene = build_attach_scene(session_id, &runtime, &config, viewport, client_id, None);
        let tiled_surface = scene
            .surfaces
            .iter()
            .find(|surface| surface.pane_id == Some(tiled))
            .expect("tiled surface");
        assert_eq!(
            tiled_surface.rect,
            AttachRect {
                x: 0,
                y: 0,
                w: 120,
                h: 40
            }
        );
        assert_eq!(
            tiled_surface.content_rect,
            AttachRect {
                x: 40,
                y: 1,
                w: 40,
                h: 38,
            }
        );
        let floating_surface = scene
            .surfaces
            .iter()
            .find(|surface| surface.pane_id == Some(floating))
            .expect("floating surface");
        assert_eq!(
            floating_surface.rect,
            AttachRect {
                x: 1,
                y: 1,
                w: 10,
                h: 5
            }
        );
        assert_eq!(
            floating_surface.content_rect,
            AttachRect {
                x: 3,
                y: 2,
                w: 7,
                h: 3,
            }
        );

        runtime.zoomed_pane_id = Some(tiled);
        let zoomed = build_attach_scene(session_id, &runtime, &config, viewport, client_id, None);
        let zoomed_tiled = zoomed
            .surfaces
            .iter()
            .find(|surface| surface.pane_id == Some(tiled))
            .expect("zoomed tiled surface");
        assert_eq!(zoomed_tiled.content_rect, tiled_surface.content_rect);
    }

    #[test]
    fn scene_root_reserves_all_generic_viewport_edges() {
        let root = scene_root_from_viewport(Some(AttachViewport {
            cols: 120,
            rows: 40,
            top_inset: 2,
            right_inset: 7,
            bottom_inset: 3,
            left_inset: 11,
        }));

        assert_eq!(
            root,
            LayoutRect {
                x: 11,
                y: 2,
                w: 102,
                h: 35,
            }
        );
    }

    #[test]
    fn scene_root_clamps_insets_to_preserve_one_content_cell() {
        let root = scene_root_from_viewport(Some(AttachViewport {
            cols: 5,
            rows: 4,
            top_inset: 3,
            right_inset: 4,
            bottom_inset: 3,
            left_inset: 4,
        }));

        assert_eq!(root.w, 1);
        assert_eq!(root.h, 1);
        assert!(root.x < 5);
        assert!(root.y < 4);
    }

    #[switchy_async::test]
    async fn attach_scene_reselects_geometry_rules_across_viewport_boundary() {
        let session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let runtime = runtime_with_panes(&[pane_id]);
        let config = PanePaddingConfig::parse(Some(
            &toml::toml! {
                [padding]
                [[padding.pane_rules]]
                min_width = 100
                max_content_width = 40
                horizontal_alignment = "center"
            }
            .into(),
        ))
        .unwrap();
        let narrow = Some(AttachViewport {
            cols: 90,
            rows: 30,
            top_inset: 0,
            right_inset: 0,
            bottom_inset: 0,
            left_inset: 0,
        });
        let wide = Some(AttachViewport {
            cols: 120,
            rows: 30,
            top_inset: 0,
            right_inset: 0,
            bottom_inset: 0,
            left_inset: 0,
        });

        let narrow_scene =
            build_attach_scene(session_id, &runtime, &config, narrow, client_id, None);
        assert_eq!(narrow_scene.surfaces[0].content_rect.w, 88);
        let wide_scene = build_attach_scene(session_id, &runtime, &config, wide, client_id, None);
        assert_eq!(wide_scene.surfaces[0].content_rect.w, 40);
        assert_eq!(wide_scene.surfaces[0].content_rect.x, 40);
    }

    #[switchy_async::test]
    async fn metadata_reconciliation_ignores_intervening_viewport_change_without_padding() {
        let session_id = SessionId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let mut manager = manager_with_runtime(session_id, runtime_with_panes(&[pane_id]));
        let before = manager.padding_state(session_id, Some(pane_id)).unwrap();
        // Deterministically interleave a navigation-driven viewport update
        // between the output reader's metadata snapshots.
        manager
            .runtimes
            .get_mut(&session_id)
            .unwrap()
            .attach_viewport = Some(AttachViewport::normalized(80, 24, 0, 0, 0, 0));
        let current = manager.padding_state(session_id, Some(pane_id)).unwrap();
        assert_ne!(
            before.effective_content_rect,
            current.effective_content_rect
        );
        let revision = manager.runtimes[&session_id].attach_view_revision;
        let reconciled = manager
            .reconcile_padding_after_metadata_change(session_id, pane_id, &before)
            .unwrap();
        assert_eq!(
            reconciled.effective_content_rect,
            current.effective_content_rect
        );
        assert_eq!(manager.runtimes[&session_id].attach_view_revision, revision);
        assert!(manager.pending_layout_revisions.is_empty());
    }

    #[switchy_async::test]
    async fn padding_mutation_captures_one_revision_and_drains_after_unlock() {
        let session_id = SessionId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let manager = Mutex::new(manager_with_runtime(
            session_id,
            runtime_with_panes(&[pane_id]),
        ));
        let before = manager.lock().unwrap().runtimes[&session_id].attach_view_revision;
        with_padding_manager(&manager, |manager| {
            manager.set_padding_override(
                session_id,
                Some(pane_id),
                Some(PanePaddingSpec {
                    left: 3,
                    ..PanePaddingSpec::default()
                }),
            )?;
            assert_eq!(manager.pending_layout_revisions[&session_id], before + 1);
            Ok(())
        })
        .unwrap();
        let manager = manager
            .try_lock()
            .expect("mutation must release manager lock");
        assert_eq!(
            manager.runtimes[&session_id].attach_view_revision,
            before + 1
        );
        assert!(manager.pending_layout_revisions.is_empty());
    }

    #[switchy_async::test]
    async fn command_and_prompt_metadata_transitions_reselect_padding_rule() {
        let session_id = SessionId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let runtime = runtime_with_panes(&[pane_id]);
        let mut manager = manager_with_runtime(session_id, runtime);
        manager.padding_config = PanePaddingConfig::parse(Some(
            &toml::toml! {
                [padding]
                [[padding.pane_rules]]
                match_command = "journal*"
                max_content_width = 40
                horizontal_alignment = "center"
            }
            .into(),
        ))
        .unwrap();
        assert_eq!(
            manager
                .padding_state(session_id, None)
                .unwrap()
                .effective_content_rect
                .w,
            118
        );
        let before = manager.padding_state(session_id, None).unwrap();
        manager.runtimes[&session_id].panes[&pane_id]
            .resurrection_state
            .lock()
            .unwrap()
            .apply_event(PaneShellMetadataEvent::CommandStart {
                command: "journalctl -f".to_string(),
                cwd: "/tmp".to_string(),
            });

        let changed = manager
            .reconcile_padding_after_metadata_change(session_id, pane_id, &before)
            .expect("re-evaluate command metadata rule");
        assert_eq!(changed.matched_rule_index, Some(0));
        assert_eq!(changed.effective_content_rect.w, 40);

        let before = changed;
        manager.runtimes[&session_id].panes[&pane_id]
            .resurrection_state
            .lock()
            .unwrap()
            .apply_event(PaneShellMetadataEvent::Prompt {
                cwd: "/tmp".to_string(),
            });
        let changed = manager
            .reconcile_padding_after_metadata_change(session_id, pane_id, &before)
            .expect("re-evaluate prompt metadata rule");
        assert_eq!(changed.matched_rule_index, None);
        assert_eq!(changed.effective_content_rect.w, 118);
    }

    #[switchy_async::test]
    async fn equivalent_metadata_rule_results_avoid_resize_and_revision_churn() {
        let session_id = SessionId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let runtime = runtime_with_panes(&[pane_id]);
        let mut manager = manager_with_runtime(session_id, runtime);
        manager.padding_config = PanePaddingConfig::parse(Some(
            &toml::toml! {
                [padding]
                [[padding.pane_rules]]
                match_command = "journal*"
                left = 0
            }
            .into(),
        ))
        .unwrap();
        let before = manager.padding_state(session_id, None).unwrap();
        let last_requested_size =
            Arc::clone(&manager.runtimes[&session_id].panes[&pane_id].last_requested_size);
        let size_before = *last_requested_size.lock().unwrap();
        let revision_before = manager.runtimes[&session_id].attach_view_revision;

        manager.runtimes[&session_id].panes[&pane_id]
            .resurrection_state
            .lock()
            .unwrap()
            .apply_event(PaneShellMetadataEvent::CommandStart {
                command: "journalctl -f".to_string(),
                cwd: "/tmp".to_string(),
            });
        let changed = manager
            .reconcile_padding_after_metadata_change(session_id, pane_id, &before)
            .expect("re-evaluate equivalent metadata rule");

        assert_eq!(changed.matched_rule_index, Some(0));
        assert_eq!(
            changed.effective_content_rect,
            before.effective_content_rect
        );
        assert_eq!(*last_requested_size.lock().unwrap(), size_before);
        assert_eq!(
            manager.runtimes[&session_id].attach_view_revision,
            revision_before
        );
    }

    #[switchy_async::test]
    async fn padding_override_updates_state_resizes_and_clears() {
        let session_id = SessionId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let runtime = runtime_with_panes(&[pane_id]);
        let mut manager = manager_with_runtime(session_id, runtime);
        let centered = PanePaddingSpec {
            max_content_width: Some(40),
            horizontal_alignment: crate::padding::HorizontalAlignment::Center,
            ..PanePaddingSpec::default()
        };

        let changed = manager
            .set_padding_override(session_id, None, Some(centered))
            .expect("set padding override");
        assert_eq!(changed.runtime_override, Some(centered));
        assert_eq!(changed.effective_content_rect.w, 40);
        assert_eq!(changed.effective_content_rect.x, 40);
        let last_requested_size =
            Arc::clone(&manager.runtimes[&session_id].panes[&pane_id].last_requested_size);
        assert_eq!(*last_requested_size.lock().unwrap(), (38, 40));

        let cleared = manager
            .set_padding_override(session_id, Some(pane_id), None)
            .expect("clear padding override");
        assert_eq!(cleared.runtime_override, None);
        assert_eq!(cleared.effective_content_rect.w, 118);
        assert_eq!(*last_requested_size.lock().unwrap(), (38, 118));
    }

    #[switchy_async::test]
    async fn replacement_padding_config_applies_to_panes_created_later() {
        let session_id = SessionId(crate::ids::new_uuid());
        let first_pane_id = crate::ids::new_uuid();
        let manager = manager_with_runtime(session_id, runtime_with_panes(&[first_pane_id]));
        let handle = PanePaddingRuntimeHandle(Arc::new(Mutex::new(manager)));
        let settings = toml::Value::Table(toml::toml! {
            [padding]
            left = 3
            max_content_width = 72
        });
        let config = PanePaddingConfig::parse(Some(&settings)).unwrap();
        handle.replace_padding_config(config).unwrap();

        let future_pane_id = crate::ids::new_uuid();
        {
            let mut manager = handle.0.lock().unwrap();
            let runtime = manager.runtimes.get_mut(&session_id).unwrap();
            runtime
                .panes
                .insert(future_pane_id, dummy_pane(future_pane_id));
            runtime.layout_root = Some(PaneLayoutNode::Leaf {
                pane_id: future_pane_id,
            });
            runtime.focused_pane_id = future_pane_id;
        }

        let future = handle.state(session_id, Some(future_pane_id)).unwrap();
        assert_eq!(future.declarative.left, 3);
        assert_eq!(future.declarative.max_content_width, Some(72));
        assert_eq!(future.runtime_override, None);
    }

    #[switchy_async::test]
    async fn expired_padding_preview_restores_underlying_geometry() {
        let session_id = SessionId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let owner = ClientId(crate::ids::new_uuid());
        let mut manager = manager_with_runtime(session_id, runtime_with_panes(&[pane_id]));
        let baseline = manager.padding_state(session_id, Some(pane_id)).unwrap();
        manager
            .begin_padding_preview(
                owner,
                vec![pane_id],
                PanePaddingSpec {
                    left: 4,
                    ..PanePaddingSpec::default()
                },
            )
            .expect("begin preview");

        manager.cancel_expired_padding_previews(Instant::now() + PANE_PADDING_PREVIEW_LEASE);

        assert!(manager.padding_previews.is_empty());
        let restored = manager.padding_state(session_id, Some(pane_id)).unwrap();
        assert_eq!(restored.effective, baseline.effective);
        assert_eq!(
            restored.effective_content_rect,
            baseline.effective_content_rect
        );
    }

    #[switchy_async::test]
    async fn updating_padding_preview_renews_its_lease() {
        let session_id = SessionId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let owner = ClientId(crate::ids::new_uuid());
        let mut manager = manager_with_runtime(session_id, runtime_with_panes(&[pane_id]));
        let (token, _) = manager
            .begin_padding_preview(owner, vec![pane_id], PanePaddingSpec::default())
            .expect("begin preview");
        manager
            .padding_previews
            .get_mut(&token)
            .unwrap()
            .lease_expires_at = Instant::now();

        manager
            .update_padding_preview(
                owner,
                token,
                vec![pane_id],
                PanePaddingSpec {
                    right: 3,
                    ..PanePaddingSpec::default()
                },
            )
            .expect("update preview");
        manager.cancel_expired_padding_previews(Instant::now());

        assert!(manager.padding_previews.contains_key(&token));
    }

    #[switchy_async::test]
    async fn cancelled_preview_restores_geometry() {
        let session_id = SessionId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let owner = ClientId(crate::ids::new_uuid());
        let manager = manager_with_runtime(session_id, runtime_with_panes(&[pane_id]));
        let handle = PanePaddingRuntimeHandle::from_manager_for_test(manager);
        let baseline = handle.state(session_id, Some(pane_id)).unwrap().effective;
        let (token, _) = handle
            .begin_preview(
                owner,
                vec![pane_id],
                PanePaddingSpec {
                    left: 3,
                    ..PanePaddingSpec::default()
                },
            )
            .unwrap();

        handle.cancel_preview(owner, token).unwrap();

        assert_eq!(
            handle.state(session_id, Some(pane_id)).unwrap().effective,
            baseline
        );
    }

    #[switchy_async::test]
    async fn replacing_padding_defaults_applies_live_and_rejects_zero_limits() {
        let session_id = SessionId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let manager = manager_with_runtime(session_id, runtime_with_panes(&[pane_id]));
        let handle = PanePaddingRuntimeHandle::from_manager_for_test(manager);
        let defaults = PanePaddingSpec {
            left: 4,
            right: 2,
            ..PanePaddingSpec::default()
        };

        handle.replace_padding_defaults(defaults).unwrap();

        assert_eq!(
            handle.state(session_id, Some(pane_id)).unwrap().effective,
            defaults
        );
        handle
            .replace_padding_defaults(PanePaddingSpec {
                max_content_width: Some(0),
                ..PanePaddingSpec::default()
            })
            .expect_err("zero width limit must be rejected");
        assert_eq!(
            handle.state(session_id, Some(pane_id)).unwrap().effective,
            defaults
        );
    }

    #[switchy_async::test]
    async fn failed_preview_commit_keeps_transaction_and_durable_state_unchanged() {
        let session_id = SessionId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let missing = crate::ids::new_uuid();
        let owner = ClientId(crate::ids::new_uuid());
        let mut manager = manager_with_runtime(session_id, runtime_with_panes(&[pane_id]));
        let preview_spec = PanePaddingSpec {
            left: 3,
            ..PanePaddingSpec::default()
        };
        let (token, _) = manager
            .begin_padding_preview(owner, vec![pane_id], preview_spec)
            .expect("begin preview");
        manager
            .padding_previews
            .get_mut(&token)
            .expect("transaction")
            .target_pane_ids
            .insert(missing);

        manager
            .commit_padding_preview(owner, token, PanePaddingOverridePersistence::Snapshot)
            .expect_err("missing target must reject commit");

        let transaction = manager
            .padding_previews
            .get(&token)
            .expect("preview remains");
        assert_eq!(transaction.spec, preview_spec);
        let pane = &manager.runtimes[&session_id].panes[&pane_id];
        assert_eq!(pane.padding_preview, Some(preview_spec));
        assert_eq!(pane.padding_override, None);
        assert!(manager.padding_overrides_for_snapshot().is_empty());
    }

    #[switchy_async::test]
    async fn clear_padding_overrides_is_atomic_across_targets() {
        let session_id = SessionId(crate::ids::new_uuid());
        let first = crate::ids::new_uuid();
        let second = crate::ids::new_uuid();
        let missing = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[first, second]);
        runtime.layout_root = Some(PaneLayoutNode::Split {
            direction: PaneSplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(leaf(first)),
            second: Box::new(leaf(second)),
        });
        let mut manager = manager_with_runtime(session_id, runtime);
        let override_spec = PanePaddingSpec {
            left: 2,
            ..PanePaddingSpec::default()
        };
        manager
            .set_padding_override(session_id, Some(first), Some(override_spec))
            .unwrap();
        manager
            .set_padding_override(session_id, Some(second), Some(override_spec))
            .unwrap();

        manager
            .clear_padding_overrides(&[first, missing, second])
            .expect_err("missing target must reject the whole clear");
        assert_eq!(
            manager
                .padding_state(session_id, Some(first))
                .unwrap()
                .runtime_override,
            Some(override_spec)
        );
        assert_eq!(
            manager
                .padding_state(session_id, Some(second))
                .unwrap()
                .runtime_override,
            Some(override_spec)
        );

        let cleared = manager
            .clear_padding_overrides(&[first, second])
            .expect("valid targets clear together");
        assert_eq!(cleared.len(), 2);
        assert!(cleared.iter().all(|state| state.runtime_override.is_none()));
    }

    #[switchy_async::test]
    async fn owner_detach_cancels_owned_preview_and_restores_geometry() {
        let session_id = SessionId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let owner = ClientId(crate::ids::new_uuid());
        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime.attached_clients.insert(owner);
        let mut manager = manager_with_runtime(session_id, runtime);
        let baseline = manager.padding_state(session_id, Some(pane_id)).unwrap();
        manager
            .begin_padding_preview(
                owner,
                vec![pane_id],
                PanePaddingSpec {
                    left: 4,
                    ..PanePaddingSpec::default()
                },
            )
            .expect("begin preview");

        manager.end_attach(session_id, owner);

        assert!(manager.padding_previews.is_empty());
        assert_eq!(
            manager
                .padding_state(session_id, Some(pane_id))
                .unwrap()
                .effective,
            baseline.effective
        );
    }

    #[switchy_async::test]
    async fn cancelling_all_previews_restores_every_target() {
        let session_id = SessionId(crate::ids::new_uuid());
        let first = crate::ids::new_uuid();
        let second = crate::ids::new_uuid();
        let first_owner = ClientId(crate::ids::new_uuid());
        let second_owner = ClientId(crate::ids::new_uuid());
        let mut runtime = runtime_with_panes(&[first, second]);
        runtime.layout_root = Some(PaneLayoutNode::Split {
            direction: PaneSplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(leaf(first)),
            second: Box::new(leaf(second)),
        });
        let mut manager = manager_with_runtime(session_id, runtime);
        let first_baseline = manager.padding_state(session_id, Some(first)).unwrap();
        let second_baseline = manager.padding_state(session_id, Some(second)).unwrap();
        manager
            .begin_padding_preview(
                first_owner,
                vec![first],
                PanePaddingSpec {
                    left: 2,
                    ..PanePaddingSpec::default()
                },
            )
            .unwrap();
        manager
            .begin_padding_preview(
                second_owner,
                vec![second],
                PanePaddingSpec {
                    right: 3,
                    ..PanePaddingSpec::default()
                },
            )
            .unwrap();

        manager.cancel_all_padding_previews();

        assert!(manager.padding_previews.is_empty());
        assert_eq!(
            manager
                .padding_state(session_id, Some(first))
                .unwrap()
                .effective,
            first_baseline.effective
        );
        assert_eq!(
            manager
                .padding_state(session_id, Some(second))
                .unwrap()
                .effective,
            second_baseline.effective
        );
    }

    #[switchy_async::test]
    async fn preview_geometry_updates_only_resize_pty_when_dimensions_change() {
        let session_id = SessionId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let owner = ClientId(crate::ids::new_uuid());
        let mut manager = manager_with_runtime(session_id, runtime_with_panes(&[pane_id]));
        let size = Arc::clone(&manager.runtimes[&session_id].panes[&pane_id].last_requested_size);
        manager.apply_stored_attach_viewport(session_id);
        let initial_size = *size.lock().unwrap();
        let initial_revision = manager.runtimes[&session_id].attach_view_revision;

        let (token, _) = manager
            .begin_padding_preview(
                owner,
                vec![pane_id],
                PanePaddingSpec {
                    horizontal_alignment: crate::padding::HorizontalAlignment::Right,
                    max_content_width: Some(80),
                    ..PanePaddingSpec::default()
                },
            )
            .expect("dimension preview");
        let dimension_size = *size.lock().unwrap();
        let dimension_revision = manager.runtimes[&session_id].attach_view_revision;
        assert_ne!(dimension_size, initial_size);
        assert!(dimension_revision > initial_revision);

        manager
            .update_padding_preview(
                owner,
                token,
                vec![pane_id],
                PanePaddingSpec {
                    horizontal_alignment: crate::padding::HorizontalAlignment::Left,
                    max_content_width: Some(80),
                    ..PanePaddingSpec::default()
                },
            )
            .expect("alignment-only preview");
        let alignment_revision = manager.runtimes[&session_id].attach_view_revision;
        assert_eq!(*size.lock().unwrap(), dimension_size);
        assert!(alignment_revision > dimension_revision);

        manager
            .update_padding_preview(
                owner,
                token,
                vec![pane_id],
                PanePaddingSpec {
                    horizontal_alignment: crate::padding::HorizontalAlignment::Left,
                    max_content_width: Some(80),
                    ..PanePaddingSpec::default()
                },
            )
            .expect("equivalent preview");
        assert_eq!(*size.lock().unwrap(), dimension_size);
        assert_eq!(
            manager.runtimes[&session_id].attach_view_revision,
            alignment_revision
        );
    }

    #[switchy_async::test]
    async fn cancelling_preview_restores_runtime_override_and_persistence() {
        let session_id = SessionId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let owner = ClientId(crate::ids::new_uuid());
        let mut manager = manager_with_runtime(session_id, runtime_with_panes(&[pane_id]));
        let underlying = PanePaddingSpec {
            left: 2,
            max_content_width: Some(70),
            ..PanePaddingSpec::default()
        };
        manager
            .set_padding_override(session_id, Some(pane_id), Some(underlying))
            .expect("set underlying override");
        let (token, _) = manager
            .begin_padding_preview(
                owner,
                vec![pane_id],
                PanePaddingSpec {
                    right: 5,
                    ..PanePaddingSpec::default()
                },
            )
            .expect("begin preview");

        manager
            .cancel_padding_preview(owner, token)
            .expect("cancel preview");

        let restored = manager.padding_state(session_id, Some(pane_id)).unwrap();
        assert_eq!(restored.runtime_override, Some(underlying));
        assert_eq!(restored.effective, underlying);
        assert_eq!(
            manager.runtimes[&session_id].panes[&pane_id].padding_override_persistence,
            PanePaddingOverridePersistence::Snapshot
        );
        assert_eq!(
            manager
                .padding_overrides_for_snapshot()
                .get(&(session_id, pane_id)),
            Some(&underlying)
        );
    }

    #[switchy_async::test]
    async fn closing_preview_target_retains_remaining_targets() {
        let session_id = SessionId(crate::ids::new_uuid());
        let first_pane = crate::ids::new_uuid();
        let second_pane = crate::ids::new_uuid();
        let owner = ClientId(crate::ids::new_uuid());
        let mut runtime = runtime_with_panes(&[first_pane, second_pane]);
        runtime.layout_root = Some(PaneLayoutNode::Split {
            direction: PaneSplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(leaf(first_pane)),
            second: Box::new(leaf(second_pane)),
        });
        runtime.focused_pane_id = first_pane;
        let mut manager = manager_with_runtime(session_id, runtime);
        let (token, _) = manager
            .begin_padding_preview(
                owner,
                vec![first_pane, second_pane],
                PanePaddingSpec {
                    left: 2,
                    ..PanePaddingSpec::default()
                },
            )
            .expect("begin preview");

        manager
            .close_pane(session_id, Some(PaneSelector::ById(first_pane)))
            .expect("close first target");

        let preview = manager
            .padding_previews
            .get(&token)
            .expect("preview remains");
        assert_eq!(preview.target_pane_ids, BTreeSet::from([second_pane]));
        assert_eq!(
            manager
                .padding_state(session_id, Some(second_pane))
                .unwrap()
                .effective
                .left,
            2
        );
    }

    #[switchy_async::test]
    async fn removing_session_drops_only_its_preview_targets() {
        let first_session = SessionId(crate::ids::new_uuid());
        let second_session = SessionId(crate::ids::new_uuid());
        let first_pane = crate::ids::new_uuid();
        let second_pane = crate::ids::new_uuid();
        let owner = ClientId(crate::ids::new_uuid());
        let mut manager = manager_with_runtime(first_session, runtime_with_panes(&[first_pane]));
        manager
            .runtimes
            .insert(second_session, runtime_with_panes(&[second_pane]));
        let (token, _) = manager
            .begin_padding_preview(
                owner,
                vec![first_pane, second_pane],
                PanePaddingSpec {
                    top: 3,
                    ..PanePaddingSpec::default()
                },
            )
            .expect("begin cross-session preview");

        let _removed = manager
            .remove_runtime(first_session)
            .expect("remove session");

        let preview = manager
            .padding_previews
            .get(&token)
            .expect("preview remains");
        assert_eq!(preview.target_pane_ids, BTreeSet::from([second_pane]));
        assert_eq!(
            manager
                .padding_state(second_session, Some(second_pane))
                .unwrap()
                .effective
                .top,
            3
        );
    }

    #[switchy_async::test]
    async fn invalid_padding_preview_update_preserves_previous_preview() {
        let session_id = SessionId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let owner = ClientId(crate::ids::new_uuid());
        let mut manager = manager_with_runtime(session_id, runtime_with_panes(&[pane_id]));
        let previous = PanePaddingSpec {
            left: 3,
            ..PanePaddingSpec::default()
        };
        let (token, _) = manager
            .begin_padding_preview(owner, vec![pane_id], previous)
            .expect("begin preview");

        manager
            .update_padding_preview(
                owner,
                token,
                vec![pane_id],
                PanePaddingSpec {
                    max_content_width: Some(0),
                    ..PanePaddingSpec::default()
                },
            )
            .expect_err("invalid update must fail");

        let preview = manager
            .padding_previews
            .get(&token)
            .expect("preview remains");
        assert_eq!(preview.spec, previous);
        let state = manager.padding_state(session_id, Some(pane_id)).unwrap();
        assert_eq!(state.effective, previous);
    }

    #[switchy_async::test]
    async fn non_overlapping_cross_client_padding_previews_coexist() {
        let session_id = SessionId(crate::ids::new_uuid());
        let first_pane = crate::ids::new_uuid();
        let second_pane = crate::ids::new_uuid();
        let first_owner = ClientId(crate::ids::new_uuid());
        let second_owner = ClientId(crate::ids::new_uuid());
        let mut runtime = runtime_with_panes(&[first_pane, second_pane]);
        runtime.layout_root = Some(PaneLayoutNode::Split {
            direction: PaneSplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(leaf(first_pane)),
            second: Box::new(leaf(second_pane)),
        });
        let mut manager = manager_with_runtime(session_id, runtime);

        manager
            .begin_padding_preview(
                first_owner,
                vec![first_pane],
                PanePaddingSpec {
                    left: 2,
                    ..PanePaddingSpec::default()
                },
            )
            .expect("first preview");
        manager
            .begin_padding_preview(
                second_owner,
                vec![second_pane],
                PanePaddingSpec {
                    right: 4,
                    ..PanePaddingSpec::default()
                },
            )
            .expect("non-overlapping preview");

        assert_eq!(manager.padding_previews.len(), 2);
        assert_eq!(
            manager
                .padding_state(session_id, Some(first_pane))
                .unwrap()
                .effective
                .left,
            2
        );
        assert_eq!(
            manager
                .padding_state(session_id, Some(second_pane))
                .unwrap()
                .effective
                .right,
            4
        );
    }

    #[switchy_async::test]
    async fn same_client_new_padding_preview_replaces_previous_preview() {
        let session_id = SessionId(crate::ids::new_uuid());
        let first_pane = crate::ids::new_uuid();
        let second_pane = crate::ids::new_uuid();
        let owner = ClientId(crate::ids::new_uuid());
        let mut runtime = runtime_with_panes(&[first_pane, second_pane]);
        runtime.layout_root = Some(PaneLayoutNode::Split {
            direction: PaneSplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(leaf(first_pane)),
            second: Box::new(leaf(second_pane)),
        });
        let mut manager = manager_with_runtime(session_id, runtime);
        let (old_token, _) = manager
            .begin_padding_preview(
                owner,
                vec![first_pane],
                PanePaddingSpec {
                    left: 2,
                    ..PanePaddingSpec::default()
                },
            )
            .expect("first preview");

        let (new_token, _) = manager
            .begin_padding_preview(
                owner,
                vec![second_pane],
                PanePaddingSpec {
                    right: 3,
                    ..PanePaddingSpec::default()
                },
            )
            .expect("replacement preview");

        assert_ne!(old_token, new_token);
        assert_eq!(manager.padding_previews.len(), 1);
        assert!(!manager.padding_previews.contains_key(&old_token));
        assert_eq!(
            manager
                .padding_state(session_id, Some(first_pane))
                .unwrap()
                .effective,
            PanePaddingSpec::default()
        );
        assert_eq!(
            manager
                .padding_state(session_id, Some(second_pane))
                .unwrap()
                .effective
                .right,
            3
        );
    }

    #[switchy_async::test]
    async fn overlapping_cross_client_padding_preview_is_rejected() {
        let session_id = SessionId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let first_owner = ClientId(crate::ids::new_uuid());
        let second_owner = ClientId(crate::ids::new_uuid());
        let mut manager = manager_with_runtime(session_id, runtime_with_panes(&[pane_id]));
        let spec = PanePaddingSpec {
            left: 2,
            ..PanePaddingSpec::default()
        };
        manager
            .begin_padding_preview(first_owner, vec![pane_id], spec)
            .expect("first preview");

        assert!(
            manager
                .begin_padding_preview(second_owner, vec![pane_id], spec)
                .is_err()
        );
    }

    #[switchy_async::test]
    async fn padding_preview_updates_cancels_and_commits_without_snapshot_leakage() {
        let session_id = SessionId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let owner = ClientId(crate::ids::new_uuid());
        let runtime = runtime_with_panes(&[pane_id]);
        let mut manager = manager_with_runtime(session_id, runtime);
        let initial = manager.padding_state(session_id, Some(pane_id)).unwrap();
        let centered = PanePaddingSpec {
            max_content_width: Some(40),
            horizontal_alignment: crate::padding::HorizontalAlignment::Center,
            ..PanePaddingSpec::default()
        };

        let (token, states) = manager
            .begin_padding_preview(owner, vec![pane_id], centered)
            .expect("begin preview");
        assert_eq!(states[0].effective, centered);
        assert_eq!(states[0].effective_content_rect.w, 40);
        assert!(manager.padding_overrides_for_snapshot().is_empty());

        let updated = PanePaddingSpec {
            left: 3,
            ..centered
        };
        let states = manager
            .update_padding_preview(owner, token, vec![pane_id], updated)
            .expect("update preview");
        assert_eq!(states[0].effective, updated);
        assert!(manager.padding_overrides_for_snapshot().is_empty());

        manager
            .cancel_padding_preview(owner, token)
            .expect("cancel preview");
        let cancelled = manager.padding_state(session_id, Some(pane_id)).unwrap();
        assert_eq!(cancelled.effective, initial.effective);
        assert_eq!(cancelled.runtime_override, None);

        let (token, _) = manager
            .begin_padding_preview(owner, vec![pane_id], centered)
            .expect("begin commit preview");
        let committed = manager
            .commit_padding_preview(owner, token, PanePaddingOverridePersistence::Runtime)
            .expect("commit runtime preview");
        assert_eq!(committed[0].runtime_override, Some(centered));
        assert!(manager.padding_overrides_for_snapshot().is_empty());
    }

    #[switchy_async::test]
    async fn invalid_padding_mutation_is_atomic() {
        let session_id = SessionId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let runtime = runtime_with_panes(&[pane_id]);
        let mut manager = manager_with_runtime(session_id, runtime);
        let before = manager.padding_state(session_id, Some(pane_id)).unwrap();

        let error = manager
            .set_padding_override(
                session_id,
                Some(pane_id),
                Some(PanePaddingSpec {
                    max_content_width: Some(0),
                    ..PanePaddingSpec::default()
                }),
            )
            .expect_err("zero maximum must fail");

        assert_eq!(error, SessionRuntimeError::Closed);
        let after = manager.padding_state(session_id, Some(pane_id)).unwrap();
        assert_eq!(after.runtime_override, before.runtime_override);
        assert_eq!(after.effective, before.effective);
        assert_eq!(after.effective_content_rect, before.effective_content_rect);
        assert!(manager.padding_overrides_for_snapshot().is_empty());
    }

    #[switchy_async::test]
    async fn clearing_override_omits_it_and_restores_declarative_geometry() {
        let session_id = SessionId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let runtime = runtime_with_panes(&[pane_id]);
        let mut manager = manager_with_runtime(session_id, runtime);
        manager.padding_config = PanePaddingConfig::parse(Some(
            &toml::toml! {
                [padding]
                left = 2
            }
            .into(),
        ))
        .unwrap();
        manager
            .set_padding_override(
                session_id,
                Some(pane_id),
                Some(PanePaddingSpec {
                    max_content_width: Some(40),
                    ..PanePaddingSpec::default()
                }),
            )
            .expect("set override");
        assert_eq!(manager.padding_overrides_for_snapshot().len(), 1);

        let cleared = manager
            .set_padding_override(session_id, Some(pane_id), None)
            .expect("clear override");

        assert_eq!(cleared.runtime_override, None);
        assert_eq!(cleared.declarative.left, 2);
        assert_eq!(cleared.effective, cleared.declarative);
        assert!(manager.padding_overrides_for_snapshot().is_empty());
    }

    #[switchy_async::test]
    async fn restart_preserves_padding_override_and_reapplies_its_size() {
        let host = crate::runtime::PaneRuntimeHost::detached();
        let host = &host;
        let session_id = SessionId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let runtime = runtime_with_panes(&[pane_id]);
        let mut manager = manager_with_runtime(session_id, runtime);
        let spec = PanePaddingSpec {
            max_content_width: Some(40),
            ..PanePaddingSpec::default()
        };
        manager
            .set_padding_override(session_id, Some(pane_id), Some(spec))
            .expect("set override before restart");

        manager
            .restart_pane(session_id, Some(PaneSelector::ById(pane_id)))
            .expect("restart pane");

        let restarted = &manager.runtimes[&session_id].panes[&pane_id];
        assert_eq!(restarted.padding_override, Some(spec));
        assert_eq!(*restarted.last_requested_size.lock().unwrap(), (38, 40));
        let removed = manager.remove_runtime(session_id).expect("remove runtime");
        shutdown_runtime_handle(host, removed).await;
    }

    #[switchy_async::test]
    async fn session_removal_omits_all_owned_padding_overrides() {
        let session_id = SessionId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let mut manager = manager_with_runtime(session_id, runtime_with_panes(&[pane_id]));
        manager
            .set_padding_override(
                session_id,
                Some(pane_id),
                Some(PanePaddingSpec {
                    left: 2,
                    ..PanePaddingSpec::default()
                }),
            )
            .expect("set pane override");
        assert_eq!(manager.padding_overrides_for_snapshot().len(), 1);

        let _removed = manager.remove_runtime(session_id).expect("remove runtime");

        assert!(manager.padding_overrides_for_snapshot().is_empty());
    }

    #[switchy_async::test]
    async fn padding_overrides_follow_pane_close_and_persistence_policy() {
        let session_id = SessionId(crate::ids::new_uuid());
        let first = crate::ids::new_uuid();
        let second = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[first, second]);
        runtime.layout_root = Some(PaneLayoutNode::Split {
            direction: PaneSplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(leaf(first)),
            second: Box::new(leaf(second)),
        });
        let mut manager = manager_with_runtime(session_id, runtime);
        let spec = PanePaddingSpec {
            left: 3,
            ..PanePaddingSpec::default()
        };
        manager
            .set_padding_override(session_id, Some(first), Some(spec))
            .expect("set first pane override");
        manager
            .set_padding_override(session_id, Some(second), Some(spec))
            .expect("set second pane override");
        assert_eq!(manager.padding_overrides_for_snapshot().len(), 2);

        manager
            .close_pane(session_id, Some(PaneSelector::ById(first)))
            .expect("close overridden pane");
        let overrides = manager.padding_overrides_for_snapshot();
        assert_eq!(overrides.len(), 1);
        assert!(!overrides.contains_key(&(session_id, first)));
        assert_eq!(overrides.get(&(session_id, second)), Some(&spec));

        manager.padding_config = PanePaddingConfig::parse(Some(
            &toml::toml! {
                [padding]
                persist_runtime_overrides = false
            }
            .into(),
        ))
        .unwrap();
        assert!(manager.padding_overrides_for_snapshot().is_empty());
    }

    #[switchy_async::test]
    async fn alignment_only_override_moves_content_without_resizing_pty() {
        let session_id = SessionId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let runtime = runtime_with_panes(&[pane_id]);
        let mut manager = manager_with_runtime(session_id, runtime);
        let left = PanePaddingSpec {
            max_content_width: Some(40),
            ..PanePaddingSpec::default()
        };
        let centered = PanePaddingSpec {
            horizontal_alignment: crate::padding::HorizontalAlignment::Center,
            ..left
        };
        manager
            .set_padding_override(session_id, None, Some(left))
            .expect("set left-aligned override");
        let last_requested_size =
            Arc::clone(&manager.runtimes[&session_id].panes[&pane_id].last_requested_size);
        let size_before = *last_requested_size.lock().unwrap();
        let revision_before = manager.runtimes[&session_id].attach_view_revision;

        let changed = manager
            .set_padding_override(session_id, None, Some(centered))
            .expect("move constrained content without resizing");

        assert_eq!(changed.effective_content_rect.w, 40);
        assert_eq!(changed.effective_content_rect.x, 40);
        assert_eq!(*last_requested_size.lock().unwrap(), size_before);
        assert_eq!(
            manager.runtimes[&session_id].attach_view_revision,
            revision_before + 1
        );
    }

    #[test]
    fn close_focus_fallback_uses_nearest_layout_neighbor() {
        let a = crate::ids::new_uuid();
        let b = crate::ids::new_uuid();
        let c = crate::ids::new_uuid();
        let root = PaneLayoutNode::Split {
            direction: PaneSplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(leaf(a)),
            second: Box::new(PaneLayoutNode::Split {
                direction: PaneSplitDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(leaf(b)),
                second: Box::new(leaf(c)),
            }),
        };
        let viewport = Some(AttachViewport {
            cols: 100,
            rows: 40,
            top_inset: 0,
            right_inset: 0,
            bottom_inset: 0,
            left_inset: 0,
        });

        assert_eq!(next_focus_after_close(&root, b, b, viewport), Some(c));
        assert_eq!(next_focus_after_close(&root, c, c, viewport), Some(b));
    }

    #[test]
    fn close_focus_keeps_existing_focus_when_closing_unfocused_pane() {
        let focused = crate::ids::new_uuid();
        let closed = crate::ids::new_uuid();
        let root = PaneLayoutNode::Split {
            direction: PaneSplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(leaf(focused)),
            second: Box::new(leaf(closed)),
        };

        assert_eq!(
            next_focus_after_close(&root, focused, closed, None),
            Some(focused)
        );
    }

    #[test]
    fn failed_checkpoint_close_still_fences_prompt_and_invocation() {
        let mut state = PaneResurrectionRuntime {
            execution_token: Some("saved-invocation".into()),
            active_command: Some("bcode tui".into()),
            active_command_source: Some(PaneCommandSource::Verbatim),
            ..PaneResurrectionRuntime::default()
        };
        let result = state.freeze_with(|_| anyhow::bail!("injected durable close failure"));
        assert!(result.is_err());
        state.apply_event(PaneShellMetadataEvent::Prompt {
            cwd: "/late".into(),
        });
        assert!(state.frozen);
        assert_eq!(state.execution_token.as_deref(), Some("saved-invocation"));
        assert_eq!(state.active_command.as_deref(), Some("bcode tui"));
        assert_eq!(
            state
                .freeze_with(|_| panic!("close retried"))
                .unwrap_err()
                .to_string(),
            "injected durable close failure"
        );
    }

    #[test]
    fn successful_checkpoint_freeze_is_idempotent() {
        let mut state = PaneResurrectionRuntime::default();
        state.freeze_with(|_| Ok(())).unwrap();
        state.freeze_with(|_| panic!("close retried")).unwrap();
    }

    #[switchy_async::test]
    async fn pane_shutdown_pending_startup_keeps_cleanup_authority_with_startup_owner() {
        let host = crate::runtime::PaneRuntimeHost::detached();
        let host = &host;
        let mut pane = dummy_pane(crate::ids::new_uuid());
        pane.startup = watch::channel(None).1;
        // Startup alone owns this cleanup: no session was adopted yet.
        let execution = Arc::clone(&pane.execution);
        let state = Arc::clone(&pane.resurrection_state);
        let (stop, stopped) = oneshot::channel();
        pane.stop_tx = Some(stop);
        let task = switchy_async::task::spawn(async move {
            stopped.await.unwrap();
        });
        let old = std::mem::replace(&mut pane.task, task);
        old.abort();
        let _ = old.await;
        shutdown_panes_owned(host, vec![pane], true).await.unwrap();
        assert!(!execution.cleanup_failed.load(Ordering::SeqCst));
        assert!(state.lock().unwrap().frozen);
    }

    #[switchy_async::test]
    async fn pane_shutdown_starts_every_runtime_before_waiting() {
        let host = crate::runtime::PaneRuntimeHost::detached();
        let host = &host;
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let mut removed = Vec::new();
        for _ in 0..3 {
            let pane_id = crate::ids::new_uuid();
            let mut runtime = runtime_with_panes(&[pane_id]);
            let pane = runtime.panes.get_mut(&pane_id).unwrap();
            let (stop, stopped) = oneshot::channel();
            pane.stop_tx = Some(stop);
            let barrier = Arc::clone(&barrier);
            let task = switchy_async::task::spawn(async move {
                stopped.await.unwrap();
                barrier.wait().await;
            });
            let old = std::mem::replace(&mut pane.task, task);
            old.abort();
            let _ = old.await;
            removed.push(RemovedRuntime {
                session_id: SessionId(crate::ids::new_uuid()),
                handle: runtime,
            });
        }
        switchy_async::time::timeout(
            Duration::from_secs(2),
            shutdown_removed_runtimes(host, removed),
        )
        .await
        .expect("cross-runtime cleanup deadlocked")
        .unwrap();
    }

    #[switchy_async::test]
    async fn pane_shutdown_error_receipt_retains_owner_until_reaper_settles() {
        let host = crate::runtime::PaneRuntimeHost::detached();
        let host = &host;
        let mut pane = dummy_pane(crate::ids::new_uuid());
        let execution = Arc::downgrade(&pane.execution);
        let (cleanup, receiver) = watch::channel(None);
        pane.cleanup = receiver;
        let (finish, finished) = oneshot::channel();
        let task = switchy_async::task::spawn(async move {
            cleanup.send_replace(Some(Err("injected acknowledgement failure".into())));
            let _ = finished.await;
        });
        let old = std::mem::replace(&mut pane.task, task);
        old.abort();
        let _ = old.await;
        let error = switchy_async::time::timeout(
            Duration::from_secs(2),
            shutdown_panes_result(host, vec![pane]),
        )
        .await
        .expect("failure receipt waited for reaper")
        .unwrap_err();
        assert!(error.contains("injected acknowledgement failure"));
        assert!(
            execution.upgrade().is_some(),
            "cleanup owner dropped before reaper settled"
        );
        finish.send(()).unwrap();
        switchy_async::time::timeout(Duration::from_secs(2), async {
            while execution.upgrade().is_some() {
                switchy_async::task::yield_now().await;
            }
        })
        .await
        .expect("cleanup owner not released after settlement");
    }

    #[switchy_async::test]
    async fn pane_shutdown_release_waits_for_failed_cleanup_owner() {
        let host = crate::runtime::PaneRuntimeHost::detached();
        let host = &host;
        let pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane_id]);
        let pane = runtime.panes.get_mut(&pane_id).unwrap();
        let (cleanup, receiver) = watch::channel(None);
        pane.cleanup = receiver;
        let (acknowledged, acknowledgement) = oneshot::channel();
        let (finish, finished) = oneshot::channel();
        let task = switchy_async::task::spawn(async move {
            cleanup.send_replace(Some(Err("late reaper".into())));
            acknowledged.send(()).unwrap();
            let _ = finished.await;
        });
        let old = std::mem::replace(&mut pane.task, task);
        old.abort();
        let _ = old.await;
        let release_host = host.clone();
        let mut release = switchy_async::task::spawn(async move {
            shutdown_removed_runtimes(
                &release_host,
                vec![RemovedRuntime {
                    session_id: SessionId(crate::ids::new_uuid()),
                    handle: runtime,
                }],
            )
            .await
        });
        acknowledgement.await.unwrap();
        assert!(
            switchy_async::time::timeout(Duration::from_millis(20), &mut release)
                .await
                .is_err(),
            "release authorized unloading before reaper settlement"
        );
        finish.send(()).unwrap();
        assert!(release.await.unwrap().unwrap_err().contains("late reaper"));
    }

    #[switchy_async::test]
    async fn pane_shutdown_starts_every_pane_before_waiting() {
        let host = crate::runtime::PaneRuntimeHost::detached();
        let host = &host;
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let mut panes = Vec::new();
        for _ in 0..3 {
            let mut pane = dummy_pane(crate::ids::new_uuid());
            let (stop, stopped) = oneshot::channel();
            pane.stop_tx = Some(stop);
            let barrier = Arc::clone(&barrier);
            let task = switchy_async::task::spawn(async move {
                stopped.await.unwrap();
                barrier.wait().await;
            });
            let old = std::mem::replace(&mut pane.task, task);
            old.abort();
            let _ = old.await;
            panes.push(pane);
        }
        switchy_async::time::timeout(Duration::from_secs(2), shutdown_panes(host, panes))
            .await
            .expect("serial shutdown deadlocks at the pane barrier");
    }

    #[switchy_async::test]
    async fn cancelled_shutdown_waiter_retains_cleanup_and_freezes_before_release() {
        let host = crate::runtime::PaneRuntimeHost::detached();
        let host = &host;
        let mut pane = dummy_pane(crate::ids::new_uuid());
        let state = Arc::clone(&pane.resurrection_state);
        let execution = Arc::downgrade(&pane.execution);
        let (stop, stopped) = oneshot::channel();
        let (started, start) = oneshot::channel();
        let (finish, finished) = oneshot::channel();
        pane.stop_tx = Some(stop);
        let task = switchy_async::task::spawn(async move {
            stopped.await.unwrap();
            let _ = started.send(());
            let _ = finished.await;
        });
        let old = std::mem::replace(&mut pane.task, task);
        old.abort();
        let _ = old.await;
        let waiter_host = host.clone();
        let waiter =
            switchy_async::task::spawn(
                async move { shutdown_pane_handle(&waiter_host, pane).await },
            );
        start.await.unwrap();
        waiter.abort();
        let _ = waiter.await;
        assert!(
            execution.upgrade().is_some(),
            "cancellation dropped cleanup ownership"
        );
        finish.send(()).unwrap();
        switchy_async::time::timeout(Duration::from_secs(2), async {
            while execution.upgrade().is_some() {
                switchy_async::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(state.lock().unwrap().frozen);
    }

    #[test]
    fn unavailable_inspection_preserves_command_cwd_and_invocation() {
        let mut state = PaneResurrectionRuntime {
            execution_token: Some("actual-invocation".into()),
            active_command: Some("bcode tui".into()),
            active_command_source: Some(PaneCommandSource::Inspection),
            last_known_cwd: Some("/work/project".into()),
            ..PaneResurrectionRuntime::default()
        };
        let version = state.inspection_version();
        state.apply_inspection(&version, None);
        assert_eq!(state.active_command.as_deref(), Some("bcode tui"));
        assert_eq!(state.last_known_cwd.as_deref(), Some("/work/project"));
        assert_eq!(state.execution_token.as_deref(), Some("actual-invocation"));
    }

    #[test]
    fn stale_inspection_cannot_replace_new_prompt_or_invocation() {
        let mut state = PaneResurrectionRuntime::default();
        let captured = state.inspection_version();
        state.apply_event(PaneShellMetadataEvent::Prompt { cwd: "/new".into() });
        state.apply_inspection(
            &captured,
            Some(ProcessInspectionResult {
                command: Some("old command".into()),
                cwd: Some("/old".into()),
            }),
        );
        assert!(state.active_command.is_none());
        assert_eq!(state.last_known_cwd.as_deref(), Some("/new"));
        let captured = state.inspection_version();
        state.execution_token = Some("next-invocation".into());
        state.apply_inspection(
            &captured,
            Some(ProcessInspectionResult {
                command: Some("old command".into()),
                cwd: Some("/old".into()),
            }),
        );
        assert!(state.active_command.is_none());
        assert_eq!(state.execution_token.as_deref(), Some("next-invocation"));
    }

    #[test]
    fn slow_snapshot_observation_does_not_hold_resurrection_lock() {
        static ACTIVE: AtomicUsize = AtomicUsize::new(0);
        let state = Arc::new(Mutex::new(PaneResurrectionRuntime::default()));
        let version = state.lock().unwrap().inspection_version();
        let observed_state = Arc::clone(&state);
        let observed = bounded_directory_probe(&ACTIVE, Duration::from_secs(1), move || {
            let mut state = observed_state
                .try_lock()
                .expect("observation must run outside the resurrection lock");
            state.apply_event(PaneShellMetadataEvent::CommandStart {
                command: "new command".into(),
                cwd: "/new".into(),
            });
            Some(ProcessInspectionResult {
                command: Some("old command".into()),
                cwd: Some("/old".into()),
            })
        })
        .unwrap();
        let mut state = state.lock().unwrap();
        state.apply_inspection(&version, observed);
        assert_eq!(state.active_command.as_deref(), Some("new command"));
        assert_eq!(state.last_known_cwd.as_deref(), Some("/new"));
    }

    #[cfg(unix)]
    #[test]
    fn slow_inspection_probe_is_killed_and_reaped() {
        let root = tempfile::tempdir().unwrap();
        let pidfile = root.path().join("pid");
        let error = bounded_probe_output(
            std::process::Command::new("/bin/sh")
                .args(["-c", "echo $$ > \"$1\"; while :; do :; done", "probe"])
                .arg(&pidfile),
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        let pid: i32 = std::fs::read_to_string(pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // WNOWAIT observes without stealing the retained reaper's child.
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    u32::try_from(pid).unwrap(),
                    &raw mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result == -1 {
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::ECHILD)
                );
                break;
            }
            assert!(Instant::now() < deadline, "inspection child was not reaped");
            switchy_thread::thread::sleep(Duration::from_millis(2));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_cwd_probe_reads_current_directory_without_subprocess() {
        let observed = resolve_process_working_directory(std::process::id()).unwrap();
        assert_eq!(
            std::fs::canonicalize(observed).unwrap(),
            std::env::current_dir().unwrap().canonicalize().unwrap()
        );
    }

    #[test]
    fn cleanup_summary_groups_causes_and_bounds_examples() {
        assert!(pane_cleanup_failure_summary([]).is_none());
        let summary = pane_cleanup_failure_summary((0..64).map(|_| {
            (
                crate::ids::new_uuid(),
                "group stop failed: session sweep failed".into(),
            )
        }))
        .unwrap();
        assert!(summary.contains("cleanup failed for 64 pane(s)"));
        assert_eq!(summary.matches("session sweep failed").count(), 1);
        assert_eq!(summary.matches(',').count(), 2);
        assert!(summary.len() < 300);

        let summary = pane_cleanup_failure_summary(
            (0..20).map(|index| (Uuid::nil(), format!("failure {index}"))),
        )
        .unwrap();
        assert!(summary.contains("12 additional causes; see server logs"));
    }

    #[switchy_async::test]
    async fn shutdown_reports_original_cleanup_cause_once_per_pane() {
        let host = crate::runtime::PaneRuntimeHost::detached();
        let host = &host;
        let id = crate::ids::new_uuid();
        let pane = dummy_pane(id);
        pane.execution.record_error("earlier reader failure".into());
        pane.execution
            .record_cleanup_error("group stop failed: session sweep failed".into());
        let error = shutdown_panes_owned(host, vec![pane], true)
            .await
            .unwrap_err();
        assert!(error.contains(&id.to_string()));
        assert!(error.contains("cleanup failed for 1 pane(s)"));
        assert_eq!(error.matches("session sweep failed").count(), 1);
        assert!(!error.contains("earlier reader failure"));
    }

    fn dummy_pane(id: Uuid) -> PaneRuntimeHandle {
        let (input_tx, _input_rx) = mpsc::unbounded_channel();
        let (stop_tx, stop_rx) = oneshot::channel();
        let task = switchy_async::task::spawn(async move {
            let _ = stop_rx.await;
        });
        let execution = Arc::new(execution::ExecutionCapture::default());
        let resurrection_state = Arc::new(switchy_thread::sync::Mutex::new(
            PaneResurrectionRuntime::default(),
        ));
        PaneRuntimeHandle {
            host: crate::runtime::PaneRuntimeHost::detached(),
            lifetime: PaneLifetime {
                execution: Arc::clone(&execution),
                resurrection: Arc::clone(&resurrection_state),
            },
            execution,
            publication: Arc::new(StartupPublication::default()),
            startup: watch::channel(Some(Ok(()))).1,
            cleanup: watch::channel(Some(Ok(()))).1,
            session_id: SessionId(Uuid::nil()),
            meta: PaneRuntimeMeta {
                id,
                name: None,
                shell: "sh".to_string(),
                launch: None,
                resurrection: PaneResurrectionSnapshot::default(),
            },
            process_start_identity: Arc::new(switchy_thread::sync::Mutex::new(None)),
            process_id: Arc::new(switchy_thread::sync::Mutex::new(None)),
            process_group_id: Arc::new(switchy_thread::sync::Mutex::new(None)),
            resurrection_state,
            exit_reason: Arc::new(switchy_thread::sync::Mutex::new(None)),
            stop_tx: Some(stop_tx),
            task,
            input_tx,
            output_buffer: Arc::new(switchy_thread::sync::Mutex::new(OutputFanoutBuffer::new(
                MAX_WINDOW_OUTPUT_BUFFER_BYTES,
            ))),
            terminal_grid: Arc::new(switchy_thread::sync::Mutex::new(
                TerminalGridStream::new(1, 1, GridLimits::default())
                    .expect("dummy pane terminal grid dimensions are valid"),
            )),
            signals: Arc::new(switchy_thread::sync::Mutex::new(
                RetainedPaneSignals::default(),
            )),
            frame_replicas: Arc::new(switchy_thread::sync::Mutex::new(BTreeMap::new())),
            exited: Arc::new(AtomicBool::new(false)),
            last_requested_size: Arc::new(switchy_thread::sync::Mutex::new((1, 1))),
            output_dirty: Arc::new(AtomicBool::new(false)),
            mouse_protocol_state: Arc::new(switchy_thread::sync::Mutex::new(
                AttachMouseProtocolState::default(),
            )),
            input_mode_state: Arc::new(switchy_thread::sync::Mutex::new(
                AttachInputModeState::default(),
            )),
            padding_override: None,
            padding_preview: None,
            padding_override_persistence: PanePaddingOverridePersistence::Snapshot,
            #[cfg(feature = "image-registry")]
            image_registry: Arc::new(switchy_thread::sync::Mutex::new(
                bmux_image::ImageRegistry::default(),
            )),
            #[cfg(feature = "image-registry")]
            cell_pixel_size: Arc::new(switchy_thread::sync::Mutex::new((1, 1))),
            #[cfg(feature = "image-registry")]
            image_dirty: Arc::new(AtomicBool::new(false)),
        }
    }

    fn runtime_with_panes(pane_ids: &[Uuid]) -> SessionRuntimeHandle {
        let panes = pane_ids
            .iter()
            .copied()
            .map(|pane_id| (pane_id, dummy_pane(pane_id)))
            .collect::<BTreeMap<_, _>>();
        SessionRuntimeHandle {
            panes,
            layout_root: pane_ids
                .first()
                .copied()
                .map(|pane_id| PaneLayoutNode::Leaf { pane_id }),
            focused_pane_id: pane_ids[0],
            zoomed_pane_id: None,
            floating_surfaces: Vec::new(),
            attached_clients: BTreeSet::new(),
            scrollback_pins: BTreeMap::new(),
            next_scrollback_pin_id: 1,
            attach_viewport: Some(AttachViewport {
                cols: 120,
                rows: 40,
                top_inset: 0,
                right_inset: 0,
                bottom_inset: 0,
                left_inset: 0,
            }),
            attach_view_revision: 0,
        }
    }

    #[switchy_async::test]
    async fn rejected_split_preserves_focus_zoom_and_execution_count() {
        let session_id = SessionId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime.zoomed_pane_id = Some(pane_id);
        let mut manager = manager_with_runtime(session_id, runtime);
        for target in [crate::ids::new_uuid(), pane_id] {
            // The second case has a real pane but stale layout membership.
            if target == pane_id {
                manager.runtimes.get_mut(&session_id).unwrap().layout_root =
                    Some(PaneLayoutNode::Leaf {
                        pane_id: crate::ids::new_uuid(),
                    });
            }
            assert!(
                manager
                    .split_pane(
                        session_id,
                        Some(PaneSelector::ById(target)),
                        PaneSplitDirection::Horizontal,
                    )
                    .is_err()
            );
            let runtime = &manager.runtimes[&session_id];
            assert_eq!(runtime.zoomed_pane_id, Some(pane_id));
            assert_eq!(runtime.focused_pane_id, pane_id);
            assert_eq!(runtime.panes.len(), 1);
            assert!(manager.pane_input_index.read().unwrap().is_empty());
        }
    }

    #[switchy_async::test]
    async fn source_capture_reads_published_identity_while_manager_and_metadata_are_busy() {
        let session = SessionId(crate::ids::new_uuid());
        let pane = crate::ids::new_uuid();
        let manager = Arc::new(Mutex::new(manager_with_runtime(
            session,
            runtime_with_panes(&[pane]),
        )));
        let locked = manager.lock().unwrap();
        let runtime = &locked.runtimes[&session].panes[&pane];
        locked.register_pane_input(session, runtime);
        let metadata = runtime.resurrection_state.lock().unwrap();
        let host = &locked.host;
        host.states()
            .register(&Arc::new(RwLock::new(SpawnRuntimeHandle(
                Arc::clone(&manager),
                switchy_async::runtime::Handle::current(),
                Arc::clone(&locked.pane_input_index),
            ))));
        assert_eq!(
            capture_spawn_source(host, session.0, pane)
                .unwrap()
                .execution_token,
            metadata.observation_execution
        );
    }

    #[switchy_async::test]
    async fn spawn_source_capture_is_explicit_live_and_incarnation_scoped() {
        use bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SpawnError;
        let session_id = SessionId(crate::ids::new_uuid());
        let first = crate::ids::new_uuid();
        let second = crate::ids::new_uuid();
        let mut manager = manager_with_runtime(session_id, runtime_with_panes(&[first, second]));
        manager.register_pane_input(session_id, &manager.runtimes[&session_id].panes[&second]);
        let index = Arc::clone(&manager.pane_input_index);
        let captured = indexed_spawn_source(&index, session_id, second).unwrap();
        {
            let _held = manager.runtimes[&session_id].panes[&second]
                .resurrection_state
                .lock()
                .unwrap();
            assert_eq!(
                indexed_spawn_source(&index, session_id, second),
                Ok(captured.clone())
            );
            assert_eq!(
                manager.prepare_spawn_directory_probe(&captured),
                Err(SpawnError::Busy)
            );
        }
        assert_eq!(captured.pane_id, second);
        assert_eq!(
            manager.prepare_spawn_directory_probe(&captured),
            Err(SpawnError::ObservationUnavailable)
        );
        {
            let mut observation = manager.runtimes[&session_id].panes[&second]
                .resurrection_state
                .lock()
                .unwrap();
            observation.last_known_cwd = Some("/restored-only".into());
        }
        assert_eq!(
            manager.prepare_spawn_directory_probe(&captured),
            Err(SpawnError::ObservationUnavailable)
        );
        manager.runtimes[&session_id].panes[&second]
            .resurrection_state
            .lock()
            .unwrap()
            .apply_event(PaneShellMetadataEvent::Prompt {
                cwd: "/current".into(),
            });
        let observed = manager.prepare_spawn_directory_probe(&captured).unwrap();
        assert_eq!(observed, SpawnDirectoryProbe::Shell("/current".into()));
        assert_eq!(manager.runtimes[&session_id].focused_pane_id, first);
        assert_eq!(
            indexed_spawn_source(&index, session_id, crate::ids::new_uuid()),
            Err(SpawnError::SourceUnavailable)
        );
        assert_eq!(
            indexed_spawn_source(&index, SessionId(crate::ids::new_uuid()), second),
            Err(SpawnError::SourceUnavailable)
        );
        manager
            .runtimes
            .get_mut(&session_id)
            .unwrap()
            .panes
            .insert(second, dummy_pane(second));
        manager.register_pane_input(session_id, &manager.runtimes[&session_id].panes[&second]);
        let replaced = indexed_spawn_source(&index, session_id, second).unwrap();
        assert_ne!(captured.execution_token, replaced.execution_token);
        assert_eq!(
            manager.prepare_spawn_directory_probe(&captured),
            Err(SpawnError::StaleExecution)
        );
        manager.runtimes[&session_id].panes[&second]
            .exited
            .store(true, Ordering::SeqCst);
        assert_eq!(
            indexed_spawn_source(&index, session_id, second),
            Err(SpawnError::SourceUnavailable)
        );
    }

    #[switchy_async::test]
    async fn startup_stop_wins_over_ready_or_pending_identity() {
        let (send, mut stop) = oneshot::channel();
        send.send(()).unwrap();
        assert_eq!(
            startup_identity_or_stop(async { Some(ProcessStartIdentity(1, 0)) }, &mut stop).await,
            Err(())
        );
        let (send, mut stop) = oneshot::channel();
        drop(send);
        assert_eq!(
            startup_identity_or_stop(std::future::pending(), &mut stop).await,
            Err(())
        );
        let (_send, mut stop) = oneshot::channel();
        assert_eq!(
            startup_identity_or_stop(async { None }, &mut stop).await,
            Ok(None)
        );
    }

    #[switchy_async::test]
    async fn failed_startup_cleanup_reports_wait_error_and_timeout() {
        let wait = poll_failed_startup_child(
            || Err(std::io::Error::other("injected wait failure")),
            Some(std::io::Error::other("injected kill failure")),
            Duration::ZERO,
        )
        .await
        .unwrap_err();
        assert!(wait.contains("injected wait failure"));
        assert!(wait.contains("injected kill failure"));
        let timeout = poll_failed_startup_child(|| Ok(false), None, Duration::ZERO)
            .await
            .unwrap_err();
        assert!(timeout.contains("timed out"));
        assert_eq!(
            poll_failed_startup_child(
                || Ok(true),
                Some(std::io::Error::other("already exited")),
                Duration::ZERO
            )
            .await,
            Ok(())
        );
    }

    #[cfg(unix)]
    #[switchy_async::test]
    async fn failed_startup_cleanup_reaps_spawned_child() {
        let host = crate::runtime::PaneRuntimeHost::detached();
        let host = &host;
        let pair = bmux_pty::PtyPair::open(PtySize { rows: 24, cols: 80 }).unwrap();
        let mut command = SpawnCommand::new("/bin/sh");
        command.args(["-c", "exec sleep 60"]);
        let child = pty::spawn(&pair.slave, &command).unwrap();
        let pid = child.process_id().unwrap();
        let reason =
            cleanup_failed_startup_child(host, child, "injected startup failure".into()).await;
        assert_eq!(reason, "injected startup failure");
        assert!(process_start_identity_for_pid(pid).is_none());
    }

    #[cfg(unix)]
    #[switchy_async::test]
    async fn failed_startup_cleanup_survives_dropped_waiter() {
        let host = crate::runtime::PaneRuntimeHost::detached();
        let host = &host;
        let pair = bmux_pty::PtyPair::open(PtySize { rows: 24, cols: 80 }).unwrap();
        let mut command = SpawnCommand::new("/bin/sh");
        command.args(["-c", "exec sleep 60"]);
        let child = pty::spawn(&pair.slave, &command).unwrap();
        let pid = child.process_id().unwrap();
        let identity = process_start_identity_for_pid(pid).unwrap();
        let (acknowledgement, mut cleanup) = watch::channel(None);
        drop(cleanup_failed_startup_child_acknowledged(
            host,
            child,
            "cancelled request".into(),
            &acknowledgement,
        ));
        switchy_async::time::timeout(Duration::from_secs(3), cleanup.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*cleanup.borrow(), Some(Ok(())));
        let deadline = switchy_async::time::Instant::now() + Duration::from_secs(3);
        while process_start_identity_for_pid(pid) == Some(identity) {
            assert!(
                switchy_async::time::Instant::now() < deadline,
                "child was not reaped"
            );
            switchy_async::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[cfg(unix)]
    #[switchy_async::test]
    async fn aborting_startup_owner_reaps_child() {
        let host = crate::runtime::PaneRuntimeHost::detached();
        let host = &host;
        let pair = bmux_pty::PtyPair::open(PtySize { rows: 24, cols: 80 }).unwrap();
        let mut command = SpawnCommand::new("/bin/sh");
        command.args(["-c", "exec sleep 60"]);
        let child = pty::spawn(&pair.slave, &command).unwrap();
        let pid = child.process_id().unwrap();
        let identity = process_start_identity_for_pid(pid).unwrap();
        let guard = StartupChild::new(host, child);
        let (started, ready) = oneshot::channel();
        let task = switchy_async::task::spawn(async move {
            let _guard = guard;
            started.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        ready.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let deadline = switchy_async::time::Instant::now() + Duration::from_secs(3);
        while process_start_identity_for_pid(pid) == Some(identity) {
            assert!(
                switchy_async::time::Instant::now() < deadline,
                "aborted child was not reaped"
            );
            switchy_async::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[cfg(unix)]
    #[switchy_async::test]
    async fn startup_child_drop_on_plain_thread_uses_owning_runtime() {
        let host = crate::runtime::PaneRuntimeHost::detached();
        let host = &host;
        let pair = bmux_pty::PtyPair::open(PtySize { rows: 24, cols: 80 }).unwrap();
        let mut command = SpawnCommand::new("/bin/sh");
        command.args(["-c", "exec sleep 60"]);
        let child = pty::spawn(&pair.slave, &command).unwrap();
        let pid = child.process_id().unwrap();
        let identity = process_start_identity_for_pid(pid).unwrap();
        let guard = StartupChild::new(host, child);
        switchy_thread::thread::spawn(move || drop(guard))
            .join()
            .unwrap();
        let deadline = switchy_async::time::Instant::now() + Duration::from_secs(3);
        while process_start_identity_for_pid(pid) == Some(identity) {
            assert!(
                switchy_async::time::Instant::now() < deadline,
                "child was not reaped"
            );
            switchy_async::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[switchy_async::test(flavor = "current_thread")]
    async fn process_identity_probe_does_not_block_async_runtime() {
        let (release, blocked) = switchy_thread::sync::mpsc::channel();
        let (started, ready) = oneshot::channel();
        let query = switchy_async::task::spawn(bounded_start_identity_probe(move || {
            started.send(()).unwrap();
            blocked.recv().unwrap();
            Some(ProcessStartIdentity(42, 0))
        }));
        // The only runtime thread must remain available while native work blocks.
        ready.await.unwrap();
        release.send(()).unwrap();
        assert_eq!(query.await.unwrap(), Some(ProcessStartIdentity(42, 0)));
    }

    // Process start identities are only read on Unix.
    #[cfg(unix)]
    #[test]
    fn native_directory_observation_rejects_wrong_process_start_identity() {
        let pid = std::process::id();
        let identity = process_start_identity_for_pid(pid).unwrap();
        assert!(observe_process_directory_for_identity(pid, identity).is_some());
        assert!(
            observe_process_directory_for_identity(
                pid,
                ProcessStartIdentity(identity.0.wrapping_add(1), identity.1)
            )
            .is_none()
        );
    }

    #[test]
    fn directory_probe_deadline_retains_admission_until_worker_finishes() {
        use bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SpawnError;
        static ACTIVE: AtomicUsize = AtomicUsize::new(0);
        let (release, blocked) = switchy_thread::sync::mpsc::channel();
        assert_eq!(
            bounded_directory_probe::<String, _>(&ACTIVE, Duration::ZERO, move || {
                blocked.recv().unwrap();
                Some("/late".into())
            }),
            Err(SpawnError::ObservationTimeout)
        );
        assert_eq!(ACTIVE.load(Ordering::SeqCst), 1);
        release.send(()).unwrap();
        let deadline = switchy_time::Instant::now() + Duration::from_secs(2);
        while ACTIVE.load(Ordering::SeqCst) != 0 {
            assert!(
                switchy_time::Instant::now() < deadline,
                "worker leaked admission"
            );
            switchy_thread::thread::yield_now();
        }
        assert_eq!(
            bounded_directory_probe::<String, _>(&ACTIVE, Duration::from_secs(2), || Some(
                "/ready".into()
            )),
            Ok(Some("/ready".into()))
        );
    }

    #[test]
    fn directory_probe_rejects_saturated_admission_without_running_query() {
        use bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SpawnError;
        static ACTIVE: AtomicUsize = AtomicUsize::new(MAX_DIRECTORY_PROBES);
        assert_eq!(
            bounded_directory_probe::<String, _>(&ACTIVE, Duration::from_secs(1), || panic!(
                "must not run"
            )),
            Err(SpawnError::Busy)
        );
        assert_eq!(ACTIVE.load(Ordering::SeqCst), MAX_DIRECTORY_PROBES);
    }

    #[switchy_async::test]
    async fn native_directory_probe_releases_locks_and_rejects_replacement() {
        use bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SpawnError;
        let session_id = SessionId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let manager = switchy_thread::sync::Mutex::new(manager_with_runtime(
            session_id,
            runtime_with_panes(&[pane_id]),
        ));
        let source = {
            let guard = manager.lock().unwrap();
            *guard.runtimes[&session_id].panes[&pane_id]
                .process_id
                .lock()
                .unwrap() = Some(123);
            *guard.runtimes[&session_id].panes[&pane_id]
                .process_start_identity
                .lock()
                .unwrap() = Some(ProcessStartIdentity(1, 0));
            guard.register_pane_input(session_id, &guard.runtimes[&session_id].panes[&pane_id]);
            indexed_spawn_source(&guard.pane_input_index, session_id, pane_id).unwrap()
        };
        let result = observe_spawn_directory_with(&manager, &source, |pid, _identity| {
            assert_eq!(pid, 123);
            let mut guard = manager
                .try_lock()
                .expect("native query must not hold manager");
            {
                let pane = &guard.runtimes[&session_id].panes[&pane_id];
                let _metadata = pane.resurrection_state.try_lock().unwrap();
                let _pid = pane.process_id.try_lock().unwrap();
            }
            guard
                .runtimes
                .get_mut(&session_id)
                .unwrap()
                .panes
                .insert(pane_id, dummy_pane(pane_id));
            Ok(Some("/old-execution".into()))
        });
        assert_eq!(result, Err(SpawnError::StaleExecution));
    }

    #[test]
    fn frozen_resurrection_ignores_late_shell_boundaries_and_replay() {
        let mut state = PaneResurrectionRuntime::from_snapshot(&PaneResurrectionSnapshot {
            execution_token: Some("saved-invocation".into()),
            active_command: Some("application".into()),
            active_command_source: Some(PaneCommandSource::Verbatim),
            last_known_cwd: Some("/saved".into()),
        });
        state.freeze().unwrap();
        let mut pending = Some("must-not-replay".into());
        let replay = apply_shell_metadata_events_and_take_prompt_replay(
            &mut state,
            &mut pending,
            [
                PaneShellMetadataEvent::Prompt {
                    cwd: "/late".into(),
                },
                PaneShellMetadataEvent::CommandStart {
                    command: "other".into(),
                    cwd: "/late".into(),
                },
            ],
        );
        assert!(replay.is_none());
        assert_eq!(pending.as_deref(), Some("must-not-replay"));
        assert_eq!(state.execution_token.as_deref(), Some("saved-invocation"));
        assert_eq!(state.active_command.as_deref(), Some("application"));
        assert_eq!(state.last_known_cwd.as_deref(), Some("/saved"));
    }

    fn manager_with_runtime(
        session_id: SessionId,
        runtime: SessionRuntimeHandle,
    ) -> SessionRuntimeManager {
        let (pane_exit_tx, _pane_exit_rx) = mpsc::unbounded_channel();
        SessionRuntimeManager {
            host: crate::runtime::PaneRuntimeHost::detached(),
            padding_config: PanePaddingConfig::default(),
            restored_padding_overrides: BTreeMap::new(),
            padding_previews: BTreeMap::new(),
            pending_layout_revisions: BTreeMap::new(),
            runtimes: BTreeMap::from([(session_id, runtime)]),
            pane_input_index: Arc::new(RwLock::new(BTreeMap::new())),
            client_write_permissions: Arc::new(RwLock::new(BTreeMap::new())),
            retained_output_bytes: Arc::new(switchy_thread::sync::atomic::AtomicUsize::new(0)),
            retained_pin_bytes: Arc::new(AtomicUsize::new(0)),
            shell: "sh".to_string(),
            pane_term: "xterm-256color".to_string(),
            protocol_profile: ProtocolProfile::Bmux,
            bracketed_paste_supported: true,
            shell_integration_root: None,
            pane_exit_tx,
        }
    }

    fn set_pane_grid(pane: &PaneRuntimeHandle, width: u16, height: u16) {
        *pane
            .terminal_grid
            .lock()
            .expect("grid lock should be available") =
            TerminalGridStream::new(width, height, GridLimits::default())
                .expect("test grid dimensions are valid");
    }

    fn adapter_for_manager(manager: SessionRuntimeManager) -> ServerSessionRuntimeAdapter {
        ServerSessionRuntimeAdapter::new(Arc::new(Mutex::new(manager)))
    }

    fn floating_surface(pane_id: Uuid, scope: FloatingPaneScope) -> FloatingSurfaceRuntime {
        FloatingSurfaceRuntime {
            id: crate::ids::new_uuid(),
            pane_id,
            anchor_pane_id: None,
            context_id: None,
            client_id: None,
            rect: LayoutRect {
                x: 1,
                y: 1,
                w: 10,
                h: 5,
            },
            scope,
            layer: FloatingPaneLayer::FloatingPane,
            z: 0,
            visible: true,
            opaque: true,
            accepts_input: true,
            cursor_owner: false,
        }
    }

    #[switchy_async::test]
    async fn pane_direct_input_resolves_visible_cross_session_owner() {
        let host = crate::runtime::PaneRuntimeHost::detached();
        let host = &host;
        let attach_session_id = SessionId(crate::ids::new_uuid());
        let owner_session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let attach_pane_id = crate::ids::new_uuid();
        let owner_pane_id = crate::ids::new_uuid();

        let mut attach_runtime = runtime_with_panes(&[attach_pane_id]);
        attach_runtime.attached_clients.insert(client_id);
        let mut owner_runtime = runtime_with_panes(&[owner_pane_id]);
        owner_runtime.floating_surfaces.push(floating_surface(
            owner_pane_id,
            FloatingPaneScope::ServerGlobal,
        ));

        let (input_tx, mut input_rx) = mpsc::unbounded_channel();
        let pane_input_index = Arc::new(RwLock::new(BTreeMap::from([(
            owner_pane_id,
            PaneInputHandle {
                execution_token: None,
                host: crate::runtime::PaneRuntimeHost::detached(),
                session_id: owner_session_id,
                input_tx,
                exited: Arc::new(AtomicBool::new(false)),
            },
        )])));
        let (pane_exit_tx, _pane_exit_rx) = mpsc::unbounded_channel();
        let manager = SessionRuntimeManager {
            host: crate::runtime::PaneRuntimeHost::detached(),
            padding_config: PanePaddingConfig::default(),
            restored_padding_overrides: BTreeMap::new(),
            padding_previews: BTreeMap::new(),
            pending_layout_revisions: BTreeMap::new(),
            runtimes: BTreeMap::from([
                (attach_session_id, attach_runtime),
                (owner_session_id, owner_runtime),
            ]),
            pane_input_index,
            client_write_permissions: Arc::new(RwLock::new(BTreeMap::new())),
            retained_output_bytes: Arc::new(switchy_thread::sync::atomic::AtomicUsize::new(0)),
            retained_pin_bytes: Arc::new(AtomicUsize::new(0)),
            shell: "sh".to_string(),
            pane_term: "xterm-256color".to_string(),
            protocol_profile: ProtocolProfile::Bmux,
            bracketed_paste_supported: true,
            shell_integration_root: None,
            pane_exit_tx,
        };
        let adapter = adapter_for_manager(manager);
        // This fixture inserts attached_clients directly rather than using the
        // attach path, so establish the same authenticated producer membership
        // before exercising its cross-session routing behavior.
        assert!(crate::private_input::register_producer(
            host,
            owner_pane_id,
            client_id.0
        ));

        let written = bmux_pane_runtime_state::SessionRuntimeManagerApi::write_input_to_pane(
            &adapter,
            attach_session_id,
            client_id,
            owner_pane_id,
            b"click".to_vec(),
        )
        .expect("visible cross-session pane direct input should resolve owner");

        assert_eq!(written, 5);
        match input_rx.try_recv() {
            Ok(PaneRuntimeCommand::Input(bytes)) => assert_eq!(bytes, b"click"),
            Ok(_) => panic!("expected direct pane input command, got another command"),
            Err(error) => panic!("expected direct pane input command: {error}"),
        }
    }

    /// A selection provider whose only cleared client is `self.0`.
    struct ClearedSelection(ClientId);

    impl bmux_context_state::ContextStateReader for ClearedSelection {
        fn list(&self) -> Vec<bmux_context_state::ContextSummary> {
            Vec::new()
        }
        fn current_for_client(
            &self,
            _client_id: ClientId,
        ) -> Option<bmux_context_state::ContextSummary> {
            None
        }
        fn current_session_for_client(&self, _client_id: ClientId) -> Option<SessionId> {
            None
        }
        fn context_for_session(&self, _session_id: SessionId) -> Option<Uuid> {
            None
        }
        fn resolve_id(
            &self,
            _selector: &bmux_context_state::ContextSelector,
        ) -> Result<Uuid, &'static str> {
            Err("not under test")
        }
        fn selection_cleared_for_client(&self, client_id: ClientId) -> bool {
            client_id == self.0
        }
    }

    impl bmux_context_state::ContextStateWriter for ClearedSelection {
        fn create(
            &self,
            _client_id: ClientId,
            _name: Option<String>,
            _attributes: BTreeMap<String, String>,
        ) -> bmux_context_state::ContextSummary {
            unreachable!("not under test")
        }
        fn select_for_client(
            &self,
            _client_id: ClientId,
            _selector: &bmux_context_state::ContextSelector,
        ) -> Result<bmux_context_state::ContextSummary, &'static str> {
            Err("not under test")
        }
        fn rename(
            &self,
            _selector: &bmux_context_state::ContextSelector,
            _name: String,
        ) -> Result<bmux_context_state::ContextSummary, &'static str> {
            Err("not under test")
        }
        fn set_attributes(
            &self,
            _selector: &bmux_context_state::ContextSelector,
            _attributes: BTreeMap<String, String>,
        ) -> Result<bmux_context_state::ContextSummary, &'static str> {
            Err("not under test")
        }
        fn close(
            &self,
            _client_id: ClientId,
            _selector: &bmux_context_state::ContextSelector,
            _force: bool,
        ) -> Result<(Uuid, Option<SessionId>), &'static str> {
            Err("not under test")
        }
        fn remove_contexts_for_session(&self, _session_id: SessionId) -> Vec<Uuid> {
            Vec::new()
        }
        fn bind_session(
            &self,
            _context_id: Uuid,
            _session_id: SessionId,
        ) -> Result<(), &'static str> {
            Err("not under test")
        }
        fn disconnect_client(&self, _client_id: ClientId) {}
        fn remove_context_by_id(
            &self,
            _context_id: Uuid,
            _preferred_client: Option<ClientId>,
        ) -> Option<(Uuid, Option<SessionId>)> {
            None
        }
        fn snapshot(&self) -> bmux_context_state::ContextStateSnapshot {
            bmux_context_state::ContextStateSnapshot::default()
        }
        fn restore_snapshot(&self, _snapshot: bmux_context_state::ContextStateSnapshot) {}
    }

    #[test]
    fn only_a_client_whose_selection_was_cleared_has_no_input_target() {
        let cleared = ClientId(crate::ids::new_uuid());
        let other = ClientId(crate::ids::new_uuid());

        // No selection provider (baseline, missing contexts plugin): no gate.
        let host = PaneRuntimeHost::detached();
        assert_eq!(host.ensure_client_has_input_target(cleared), Ok(()));

        let handle = Arc::new(switchy_thread::sync::RwLock::new(ContextStateHandle::new(
            ClearedSelection(cleared),
        )));
        host.states().register(&handle);
        assert_eq!(
            host.ensure_client_has_input_target(cleared),
            Err(NO_INPUT_TARGET)
        );
        assert_eq!(host.ensure_client_has_input_target(other), Ok(()));

        // A selection provider with no notion of clearing never gates.
        *handle.write().unwrap() = ContextStateHandle::noop();
        assert_eq!(host.ensure_client_has_input_target(cleared), Ok(()));
    }

    #[switchy_async::test]
    async fn attach_snapshot_watermark_matches_concurrent_output() {
        let session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime.attached_clients.insert(client_id);
        let pane = runtime.panes.get(&pane_id).unwrap();
        pane.terminal_grid.lock().unwrap().resize(2000, 2).unwrap();
        let grid = Arc::clone(&pane.terminal_grid);
        let output = Arc::clone(&pane.output_buffer);
        let adapter = adapter_for_manager(manager_with_runtime(session_id, runtime));
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let writer_barrier = Arc::clone(&barrier);
        let writer = switchy_thread::thread::spawn(move || {
            writer_barrier.wait();
            for _ in 0..1000 {
                process_pane_output(&grid, &output, b"x").unwrap();
                switchy_thread::thread::yield_now();
            }
        });
        barrier.wait();
        for _ in 0..100 {
            let state =
                bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_grid_snapshot_state(
                    &adapter,
                    session_id,
                    client_id,
                    &[pane_id],
                    2,
                )
                .unwrap();
            let captured = &state.snapshots[0];
            let snapshot: bmux_terminal_grid::GridSnapshot =
                serde_json::from_slice(&captured.encoded).unwrap();
            let bytes = snapshot
                .rows
                .iter()
                .flat_map(|row| &row.runs)
                .map(|run| run.text.len())
                .sum::<usize>();
            assert_eq!(u64::try_from(bytes).unwrap(), captured.stream_end);
            let pin = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_scrollback_pin(
                &adapter, session_id, client_id, pane_id,
            )
            .unwrap();
            for pin_id in [None, Some(pin.pin_id)] {
                let windows =
                    bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_grid_window_state(
                        &adapter,
                        session_id,
                        client_id,
                        &[bmux_pane_runtime_state::AttachPaneGridWindowRequest {
                            pane_id,
                            scrollback_offset: 0,
                            rows: 2,
                            anchor_total_scrolled_rows: None,
                            pin_id,
                        }],
                    )
                    .unwrap();
                let window = &windows.windows[0];
                let snapshot: bmux_terminal_grid::GridSnapshot =
                    serde_json::from_slice(&window.encoded).unwrap();
                let bytes = snapshot
                    .rows
                    .iter()
                    .flat_map(|row| &row.runs)
                    .map(|run| run.text.len())
                    .sum::<usize>();
                assert_eq!(u64::try_from(bytes).unwrap(), window.stream_end);
                if pin_id.is_some() {
                    assert_eq!(window.stream_end, pin.stream_end);
                }
            }
        }
        writer.join().unwrap();
    }

    #[switchy_async::test]
    async fn attach_grid_snapshot_budget_rejection_preserves_replay_cursors() {
        let session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let mut grid = TerminalGridStream::new(1000, 100, GridLimits::default()).unwrap();
        grid.process(&vec![b'x'; 100_000]);
        let size = serde_json::to_vec(&grid.snapshot(0, 100)).unwrap().len();
        assert!(size < RESPONSE_OUTPUT_BUDGET);
        let pane_ids = (0..=RESPONSE_OUTPUT_BUDGET / size)
            .map(|_| crate::ids::new_uuid())
            .collect::<Vec<_>>();
        let mut runtime = runtime_with_panes(&pane_ids);
        runtime.attached_clients.insert(client_id);
        // Share immutable terminal content to keep this aggregate-budget fixture
        // small; each pane still owns an independent raw replay buffer.
        let grid = Arc::new(switchy_thread::sync::Mutex::new(grid));
        let mut buffers = Vec::new();
        for pane in runtime.panes.values_mut() {
            pane.terminal_grid = Arc::clone(&grid);
            let mut output = pane.output_buffer.lock().unwrap();
            output.push_chunk(b"hello");
            output.set_client_cursor(client_id, 1);
            buffers.push(Arc::clone(&pane.output_buffer));
        }
        let adapter = adapter_for_manager(manager_with_runtime(session_id, runtime));
        // The aggregate overflows: panes that fit are returned and acknowledged;
        // omitted panes keep their replay cursors for a later retry.
        let partial =
            bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_grid_snapshot_state(
                &adapter, session_id, client_id, &pane_ids, 100,
            )
            .unwrap();
        let returned = partial.snapshots.len();
        assert!(returned >= 1 && returned < pane_ids.len());
        assert!(
            partial
                .snapshots
                .iter()
                .all(|snapshot| snapshot.stream_end == 5)
        );
        let cursor_count = |cursor: u64| {
            buffers
                .iter()
                .filter(|buffer| buffer.lock().unwrap().cursors.get(&client_id) == Some(&cursor))
                .count()
        };
        assert_eq!(cursor_count(5), returned);
        assert_eq!(cursor_count(1), pane_ids.len() - returned);
    }

    #[switchy_async::test]
    async fn oversized_screen_cell_snapshot_rejection_preserves_state() {
        let session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime.attached_clients.insert(client_id);
        let pane = runtime.panes.get(&pane_id).unwrap();
        set_pane_grid(pane, 10, 2);
        let grid = Arc::clone(&pane.terminal_grid);
        let output = Arc::clone(&pane.output_buffer);
        output.lock().unwrap().register_client_at_tail(client_id);
        let mut bytes = b"a".to_vec();
        for _ in 0..=RESPONSE_OUTPUT_BUDGET / 2 {
            bytes.extend_from_slice("\u{0301}".as_bytes());
        }
        process_pane_output(&grid, &output, &bytes).unwrap();
        let revision = grid.lock().unwrap().grid().revision();
        let cursor = output.lock().unwrap().cursors[&client_id];
        let adapter = adapter_for_manager(manager_with_runtime(session_id, runtime));
        let result = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_grid_snapshot_state(
            &adapter,
            session_id,
            client_id,
            &[pane_id],
            2,
        );
        assert!(matches!(
            result,
            Err(SessionRuntimeError::ResponseBudgetExceeded)
        ));
        assert_eq!(grid.lock().unwrap().grid().revision(), revision);
        assert!(
            grid.lock()
                .unwrap()
                .grid()
                .screen_cell_text_exceeds(RESPONSE_OUTPUT_BUDGET)
        );
        assert_eq!(output.lock().unwrap().cursors[&client_id], cursor);
        grid.lock().unwrap().process(b"\r\x1b[2Kok");
        assert!(
            bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_grid_snapshot_state(
                &adapter,
                session_id,
                client_id,
                &[pane_id],
                2,
            )
            .is_ok()
        );
    }

    #[switchy_async::test]
    async fn oversized_pin_capture_preserves_previous_pin() {
        let session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime.attached_clients.insert(client_id);
        let pane = runtime.panes.get(&pane_id).unwrap();
        set_pane_grid(pane, 10, 2);
        let grid = Arc::clone(&pane.terminal_grid);
        let adapter = adapter_for_manager(manager_with_runtime(session_id, runtime));
        let pin = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_scrollback_pin(
            &adapter, session_id, client_id, pane_id,
        )
        .unwrap();
        let mut bytes = b"a".to_vec();
        for _ in 0..=RESPONSE_OUTPUT_BUDGET / 2 {
            bytes.extend_from_slice("\u{0301}".as_bytes());
        }
        grid.lock().unwrap().process(&bytes);
        let revision = grid.lock().unwrap().grid().revision();
        assert!(matches!(
            bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_scrollback_pin(
                &adapter, session_id, client_id, pane_id,
            ),
            Err(SessionRuntimeError::ResponseBudgetExceeded)
        ));
        assert_eq!(grid.lock().unwrap().grid().revision(), revision);
        let windows = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_grid_window_state(
            &adapter,
            session_id,
            client_id,
            &[bmux_pane_runtime_state::AttachPaneGridWindowRequest {
                pane_id,
                scrollback_offset: 0,
                rows: 2,
                anchor_total_scrolled_rows: None,
                pin_id: Some(pin.pin_id),
            }],
        )
        .unwrap();
        assert_eq!(windows.windows.len(), 1);
        // A retained clone can fit while projection plus wire text cannot.
        // Exercise the post-clone admission stage, not just clone rejection.
        grid.lock().unwrap().process(b"\r\x1b[2K");
        let mut text = b"a".to_vec();
        for _ in 0..RESPONSE_OUTPUT_BUDGET / 3 {
            text.extend_from_slice("\u{0301}".as_bytes());
        }
        grid.lock().unwrap().process(&text);
        assert!(
            grid.lock()
                .unwrap()
                .grid()
                .try_clone_with_budget(RESPONSE_OUTPUT_BUDGET)
                .is_some()
        );
        assert!(matches!(
            bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_scrollback_pin(
                &adapter, session_id, client_id, pane_id,
            ),
            Err(SessionRuntimeError::ResponseBudgetExceeded)
        ));
        let request = bmux_pane_runtime_state::AttachPaneGridWindowRequest {
            pane_id,
            scrollback_offset: 0,
            rows: 2,
            anchor_total_scrolled_rows: None,
            pin_id: Some(pin.pin_id),
        };
        assert!(
            bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_grid_window_state(
                &adapter,
                session_id,
                client_id,
                &[request],
            )
            .is_ok()
        );
    }

    #[switchy_async::test]
    async fn sparse_wide_attach_snapshot_does_not_charge_unused_columns() {
        let session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime.attached_clients.insert(client_id);
        let pane = runtime.panes.get(&pane_id).unwrap();
        set_pane_grid(pane, 1000, 1000);
        pane.terminal_grid.lock().unwrap().process(b"sparse");
        let adapter = adapter_for_manager(manager_with_runtime(session_id, runtime));
        let result = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_grid_snapshot_state(
            &adapter,
            session_id,
            client_id,
            &[pane_id],
            1000,
        )
        .unwrap();
        let snapshot: bmux_terminal_grid::GridSnapshot =
            serde_json::from_slice(&result.snapshots[0].encoded).unwrap();
        assert_eq!(snapshot.width, 1000);
        assert_eq!(snapshot.rows[0].runs[0].text, "sparse");
    }

    #[switchy_async::test]
    async fn adapters_share_pin_reservations_and_shrink_releases_only_unused_capacity() {
        let runtime = runtime_with_panes(&[crate::ids::new_uuid()]);
        let manager = Arc::new(Mutex::new(manager_with_runtime(
            SessionId(crate::ids::new_uuid()),
            runtime,
        )));
        let first = ServerSessionRuntimeAdapter::new(Arc::clone(&manager));
        let second = ServerSessionRuntimeAdapter::new(manager);
        assert!(Arc::ptr_eq(
            &first.retained_pin_bytes,
            &second.retained_pin_bytes
        ));
        let mut reservation =
            PinReservation::acquire(&first.retained_pin_bytes, RETAINED_PIN_BUDGET).unwrap();
        assert!(PinReservation::acquire(&second.retained_pin_bytes, 1).is_err());
        reservation.shrink(100).unwrap();
        assert_eq!(second.retained_pin_bytes.load(Ordering::Acquire), 100);
        let reader = Arc::new(reservation);
        let retired = Arc::clone(&reader);
        drop(reader);
        let rest =
            PinReservation::acquire(&second.retained_pin_bytes, RETAINED_PIN_BUDGET - 100).unwrap();
        assert!(PinReservation::acquire(&first.retained_pin_bytes, 1).is_err());
        drop(retired);
        drop(rest);
        assert_eq!(first.retained_pin_bytes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn pin_reservation_survives_retirement_until_final_reader_drops() {
        let used = Arc::new(AtomicUsize::new(0));
        let capture = Arc::new(PinReservation::acquire(&used, RETAINED_PIN_BUDGET).unwrap());
        assert!(PinReservation::acquire(&used, 1).is_err());
        let reader = Arc::clone(&capture);
        drop(capture);
        assert!(PinReservation::acquire(&used, 1).is_err());
        drop(reader);
        assert_eq!(used.load(Ordering::Acquire), 0);
        let retry = PinReservation::acquire(&used, RETAINED_PIN_BUDGET).unwrap();
        drop(retry);
        assert_eq!(used.load(Ordering::Acquire), 0);
    }

    #[switchy_async::test]
    async fn retained_pin_admission_counts_other_sessions_and_releases_removed_pins() {
        let session = SessionId(crate::ids::new_uuid());
        let other_session = SessionId(crate::ids::new_uuid());
        let client = ClientId(crate::ids::new_uuid());
        let pane = crate::ids::new_uuid();
        let terminal_grid =
            bmux_terminal_grid::TerminalGrid::new(1, 1, GridLimits::default()).unwrap();
        let bytes = terminal_grid.retained_capacity_bytes().unwrap();
        let grid = Arc::new(CapturedPinGrid {
            grid: terminal_grid,
            #[cfg(feature = "image-registry")]
            images: None,
            reservation: PinReservation::acquire(&Arc::new(AtomicUsize::new(0)), bytes).unwrap(),
        });
        let mut other = runtime_with_panes(&[pane]);
        // Shared test backing keeps the fixture cheap; admission deliberately
        // charges each registered pin conservatively even when backing aliases.
        for id in 0..RETAINED_PIN_BUDGET / bytes {
            other.scrollback_pins.insert(
                (client, Uuid::from_u128(id as u128)),
                ScrollbackPin {
                    capture_id: crate::ids::new_uuid(),
                    id: 1,
                    grid: Arc::clone(&grid),
                    total_scrolled_rows: 0,
                    stream_end: 0,
                    encoded_bytes: 0,
                },
            );
        }
        let mut manager = manager_with_runtime(other_session, other);
        assert!(matches!(
            admit_retained_pin(&manager, session, client, pane, &grid.grid),
            Err(SessionRuntimeError::ResponseBudgetExceeded)
        ));
        let key = *manager.runtimes[&other_session]
            .scrollback_pins
            .keys()
            .next()
            .unwrap();
        assert!(admit_retained_pin(&manager, other_session, key.0, key.1, &grid.grid).is_ok());
        manager
            .runtimes
            .get_mut(&other_session)
            .unwrap()
            .scrollback_pins
            .remove(&key);
        assert!(admit_retained_pin(&manager, session, client, pane, &grid.grid).is_ok());
    }

    #[switchy_async::test]
    async fn attach_grid_windows_reject_aggregate_overflow_and_allow_retry() {
        let session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime.attached_clients.insert(client_id);
        let pane = runtime.panes.get(&pane_id).unwrap();
        set_pane_grid(pane, 1000, 2);
        pane.terminal_grid
            .lock()
            .unwrap()
            .process(&vec![b'x'; 1000]);
        let expected = pane.terminal_grid.lock().unwrap().snapshot(0, 2);
        let encoded_size = serde_json::to_vec(&expected).unwrap().len();
        let request = bmux_pane_runtime_state::AttachPaneGridWindowRequest {
            pane_id,
            scrollback_offset: 0,
            rows: 2,
            anchor_total_scrolled_rows: None,
            pin_id: None,
        };
        let requests = vec![request; RESPONSE_OUTPUT_BUDGET / encoded_size + 1];
        let adapter = adapter_for_manager(manager_with_runtime(session_id, runtime));
        let result = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_grid_window_state(
            &adapter, session_id, client_id, &requests,
        );
        assert!(matches!(
            result,
            Err(SessionRuntimeError::ResponseBudgetExceeded)
        ));
        let retry = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_grid_window_state(
            &adapter,
            session_id,
            client_id,
            &requests[..1],
        )
        .unwrap();
        assert_eq!(retry.windows.len(), 1);
        let snapshot: bmux_terminal_grid::GridSnapshot =
            serde_json::from_slice(&retry.windows[0].encoded).unwrap();
        assert_eq!(snapshot, expected);
    }

    #[test]
    fn alternate_screen_encoding_budget_includes_hidden_main_content() {
        let mut grid = TerminalGridStream::new(80, 10, GridLimits::default()).unwrap();
        grid.process(&vec![b'x'; 800]);
        grid.process(b"\x1b[?1049hsmall");
        let snapshot = grid.snapshot(0, 10);
        assert_eq!(snapshot.mode, "alternate");
        let expected = serde_json::to_vec(&snapshot).unwrap();
        let mut visible_only = snapshot.clone();
        visible_only.main_rows = None;
        let mut remaining = serde_json::to_vec(&visible_only).unwrap().len();
        assert!(remaining < expected.len());
        let original_budget = remaining;
        assert_eq!(
            encode_grid_window(&snapshot, &mut remaining),
            Err(SessionRuntimeError::ResponseBudgetExceeded)
        );
        assert_eq!(remaining, original_budget);
        remaining = expected.len();
        assert_eq!(
            encode_grid_window(&snapshot, &mut remaining).unwrap(),
            expected
        );
        assert_eq!(remaining, 0);
        assert_eq!(grid.snapshot(0, 10), snapshot);
    }

    #[test]
    fn grid_window_encoding_enforces_shared_budget_without_truncation() {
        let mut grid = TerminalGridStream::new(10, 2, GridLimits::default()).unwrap();
        grid.process(b"hello");
        let snapshot = grid.snapshot(0, 2);
        let expected = serde_json::to_vec(&snapshot).unwrap();
        let mut remaining = expected.len() * 2;
        for _ in 0..2 {
            assert_eq!(
                encode_grid_window(&snapshot, &mut remaining).unwrap(),
                expected
            );
        }
        assert_eq!(remaining, 0);
        assert_eq!(
            encode_grid_window(&snapshot, &mut remaining),
            Err(SessionRuntimeError::ResponseBudgetExceeded)
        );
        remaining = expected.len() - 1;
        assert_eq!(
            encode_grid_window(&snapshot, &mut remaining),
            Err(SessionRuntimeError::ResponseBudgetExceeded)
        );
        assert_eq!(remaining, expected.len() - 1);
        assert_eq!(grid.snapshot(0, 2), snapshot);
    }

    /// A filter registered in plugin state reaches readers made before it: the sequences it
    /// declines neither change the grid nor get answered, and the rest apply as usual.
    #[test]
    fn registered_output_filters_apply_to_pane_readers() {
        use bmux_terminal_grid::{FilterInterest, SequenceFilter};
        struct NoQueriesOrClears;
        impl SequenceFilter for NoQueriesOrClears {
            fn interest(&self) -> FilterInterest {
                FilterInterest::none().csi(b'n').csi(b'J')
            }
            fn filter_csi(&self, _sequence: &SequenceHeader<'_>, _: &mut Vec<u8>) -> FilterVerdict {
                FilterVerdict::Drop
            }
        }
        let states = bmux_plugin::PluginStateRegistry::new();
        let mut reader =
            PaneReader::new(ProtocolProfile::Bmux, None).with_filters(pane_output_filters(&states));
        let mut grid = TerminalGridStream::new(10, 2, GridLimits::default()).unwrap();
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
        let mut replies = Vec::new();
        let feed = |reader: &mut PaneReader,
                    grid: &mut TerminalGridStream,
                    engine: &mut TerminalProtocolEngine,
                    bytes: &[u8],
                    replies: &mut Vec<u8>| {
            let shared = reader.filters.clone();
            let guard = read_filters(shared.as_ref());
            reader.feed(
                grid,
                engine,
                bytes,
                replies,
                active_filters(guard.as_deref()),
                &mut NoImages,
            );
        };
        feed(
            &mut reader,
            &mut grid,
            &mut engine,
            b"a\x1b[6n",
            &mut replies,
        );
        assert!(
            !replies.is_empty(),
            "no filter yet: the cursor report is answered"
        );
        pane_output_filters(&states)
            .write()
            .unwrap()
            .register(Arc::new(NoQueriesOrClears));
        replies.clear();
        feed(
            &mut reader,
            &mut grid,
            &mut engine,
            b"\x1b[6n\x1b[2Jb",
            &mut replies,
        );
        assert!(replies.is_empty(), "the filtered query is not answered");
        assert_eq!(
            bmux_terminal_grid::visible_text(grid.grid(), 0, 1).trim_end(),
            "ab",
            "the filtered clear did not apply"
        );
    }

    /// Calls `pane-runtime-output-filters-v1` as the plugin `caller` (a local invocation), through
    /// the pane runtime's own service routing.
    struct OutputFilterCaller(PaneRuntimeHost, &'static str);
    impl bmux_plugin_sdk::TypedDispatchClient for OutputFilterCaller {
        fn invoke_service_raw(
            &mut self,
            capability: &str,
            kind: bmux_ipc::InvokeServiceKind,
            interface_id: &str,
            operation: &str,
            payload: Vec<u8>,
        ) -> impl std::future::Future<Output = bmux_plugin_sdk::TypedDispatchClientResult<Vec<u8>>>
        {
            use bmux_plugin_sdk::{
                ApiVersion, HostConnectionInfo, HostMetadata, HostScope, NativeServiceContext,
                ProviderId, RegisteredService, ServiceKind, ServiceRequest,
            };
            assert_eq!(kind, bmux_ipc::InvokeServiceKind::Command);
            let response = crate::handlers::route(
                &self.0,
                &NativeServiceContext {
                    host_service_port: Some(history_fixture_origin()),
                    plugin_id: "bmux.pane_runtime".into(),
                    request: ServiceRequest {
                        caller_plugin_id: self.1.into(),
                        service: RegisteredService {
                            capability: HostScope::new(capability).unwrap(),
                            kind: ServiceKind::Command,
                            interface_id: interface_id.into(),
                            provider: ProviderId::Plugin("bmux.pane_runtime".into()),
                        },
                        operation: operation.into(),
                        payload,
                    },
                    required_capabilities: vec![],
                    provided_capabilities: vec![],
                    services: vec![],
                    available_capabilities: vec![],
                    enabled_plugins: vec![],
                    plugin_search_roots: vec![],
                    host: HostMetadata {
                        product_name: "bmux".into(),
                        product_version: env!("CARGO_PKG_VERSION").into(),
                        plugin_api_version: ApiVersion::new(1, 0),
                        plugin_abi_version: ApiVersion::new(1, 0),
                    },
                    connection: HostConnectionInfo {
                        config_dir: String::new(),
                        config_dir_candidates: vec![],
                        runtime_dir: String::new(),
                        data_dir: String::new(),
                        state_dir: String::new(),
                    },
                    settings: None,
                    plugin_settings_map: std::collections::BTreeMap::new(),
                    caller_client_id: None,
                    cancellation: bmux_plugin_sdk::CancellationToken::default(),
                    host_kernel_bridge: None,
                },
            );
            assert_eq!(response.error, None);
            std::future::ready(Ok(response.payload))
        }
    }

    /// Follow `output` through a reader with `host`'s output filters, and apply the grid's frame
    /// to a client replica: the replica's first row and the style of its third cell, with the raw
    /// output the reader publishes and the signals it saw.
    fn follow_as_a_replica(
        host: &PaneRuntimeHost,
        output: &[u8],
    ) -> (String, bmux_terminal_grid::Style, Vec<u8>, Vec<PaneSignal>) {
        use bmux_terminal_grid::ReplicaCursor;
        let mut reader = PaneReader::new(ProtocolProfile::Bmux, None)
            .with_filters(pane_output_filters(host.states()));
        let mut grid = TerminalGridStream::new(10, 2, GridLimits::default()).unwrap();
        let shared = reader.filters.clone();
        let guard = read_filters(shared.as_ref());
        reader.feed(
            &mut grid,
            &mut TerminalProtocolEngine::new(ProtocolProfile::Bmux),
            output,
            &mut Vec::new(),
            active_filters(guard.as_deref()),
            &mut NoImages,
        );
        drop(guard);
        let mut frame = Vec::new();
        grid.grid_mut()
            .write_frame_since(ReplicaCursor::default(), 0, &mut frame);
        let mut replica = TerminalGrid::new(10, 2, GridLimits::default()).unwrap();
        replica
            .apply_frame(&frame, &mut ReplicaCursor::default())
            .unwrap();
        let text = bmux_terminal_grid::visible_text(&replica, 0, 1)
            .trim_end()
            .to_owned();
        let style = replica.viewport_row_ref(0).unwrap().cells()[2].style();
        (
            text,
            replica.palette().get(style),
            reader.visible,
            reader.signals,
        )
    }

    /// A plugin's output rules, set through `pane-runtime-output-filters-v1`, rewrite what a
    /// pane's frames show (a title replaced, colours turned to bold, a full clear dropped) while
    /// the raw output the reader publishes stays as the program wrote it; another plugin's rules
    /// are its own, invalid rules are refused, and clearing the rules restores the output.
    #[switchy_async::test]
    async fn output_filter_rules_rewrite_what_pane_frames_show() {
        use bmux_pane_runtime_plugin_api::pane_runtime_output_filters_v1::{
            OutputAction, OutputFilterError, OutputMatch, OutputRule, client,
        };
        let host = PaneRuntimeHost::detached();
        let mut plugin = OutputFilterCaller(host.clone(), "example.rewrites");
        let rules = vec![
            OutputRule {
                sequence: OutputMatch::Csi {
                    final_byte: b'm',
                    intermediates: Vec::new(),
                    params: None,
                },
                action: OutputAction::Replace {
                    bytes: b"\x1b[1m".to_vec(),
                },
            },
            OutputRule {
                sequence: OutputMatch::Csi {
                    final_byte: b'J',
                    intermediates: Vec::new(),
                    params: Some(vec![2]),
                },
                action: OutputAction::Drop,
            },
            OutputRule {
                sequence: OutputMatch::Osc { number: 2 },
                action: OutputAction::Replace {
                    bytes: b"\x1b]2;[pane]\x07".to_vec(),
                },
            },
        ];
        client::set_rules(&mut plugin, rules)
            .await
            .unwrap()
            .unwrap();
        let invalid = OutputRule {
            sequence: OutputMatch::Csi {
                final_byte: b'!',
                intermediates: Vec::new(),
                params: None,
            },
            action: OutputAction::Drop,
        };
        assert!(matches!(
            client::set_rules(
                &mut OutputFilterCaller(host.clone(), "example.other"),
                vec![invalid]
            )
            .await
            .unwrap(),
            Err(OutputFilterError::InvalidRule { index: 0, .. })
        ));
        let output = b"ab\x1b[2J\x1b]2;secret\x07\x1b[31mred";
        let (text, style, raw, signals) = follow_as_a_replica(&host, output);
        assert_eq!(text, "abred", "the full clear was dropped");
        assert!(
            style.bold && style.fg.is_none(),
            "the colour became bold: {style:?}"
        );
        assert_eq!(signals, [PaneSignal::Title("[pane]".into())]);
        assert_eq!(raw, output, "raw output is as the program wrote it");
        client::set_rules(&mut plugin, Vec::new())
            .await
            .unwrap()
            .unwrap();
        let (text, style, _, signals) = follow_as_a_replica(&host, output);
        assert_eq!(text, "  red", "the clear applied, the cursor where it was");
        assert!(!style.bold && style.fg.is_some());
        assert_eq!(signals, [PaneSignal::Title("secret".into())]);
    }

    /// OSC 52 writes are recognized (selection kept, text decoded); queries and malformed or
    /// non-UTF-8 payloads are not writes.
    #[test]
    fn clipboard_writes_are_decoded_and_queries_ignored() {
        assert_eq!(
            clipboard_write(b"c;aGVsbG8gd29ybGQ="),
            Some(PaneSignal::ClipboardWrite {
                selection: "c".into(),
                text: "hello world".into(),
            })
        );
        assert_eq!(clipboard_write(b"c;?"), None);
        assert_eq!(clipboard_write(b"c;not base64!"), None);
        assert_eq!(clipboard_write(b"c;/w=="), None, "not UTF-8");
        assert_eq!(clipboard_write(b"no separator"), None);
    }

    /// A clipboard write queued for a client reaches it in a frame, even when the grid did not
    /// otherwise change, and only once.
    #[switchy_async::test]
    async fn queued_clipboard_writes_reach_the_client_in_its_next_frame() {
        use bmux_pane_runtime_state::SessionRuntimeManagerApi as Api;
        let session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime.attached_clients.insert(client_id);
        let pane = runtime.panes.get(&pane_id).expect("pane should exist");
        set_pane_grid(pane, 20, 4);
        let replicas = Arc::clone(&pane.frame_replicas);
        let adapter = adapter_for_manager(manager_with_runtime(session_id, runtime));
        let frames = || {
            Api::attach_pane_frames(&adapter, session_id, client_id, &[pane_id], 0)
                .expect("attached client gets frames")
                .0
        };
        assert_eq!(frames().len(), 1);
        assert!(frames().is_empty());
        replicas
            .lock()
            .unwrap()
            .get_mut(&client_id)
            .unwrap()
            .clipboard_writes
            .push(("c".into(), "copied".into()));
        let sent = frames();
        assert_eq!(
            sent[0].clipboard_writes,
            [bmux_pane_runtime_state::PaneClipboardWrite {
                selection: "c".into(),
                text: "copied".into(),
            }]
        );
        assert!(frames().is_empty(), "sent once");
    }

    /// What a program signals beyond the screen reaches the reader in order: titles (even with
    /// `;` in them), working directories, notifications, progress reports, the bell, and
    /// prompt marks with the line they mark and a finished command's status.
    #[test]
    fn the_reader_collects_what_programs_signal_beyond_the_screen() {
        let mut tracker = PaneCursorTracker::new(4, 40);
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
        let _ = protocol_reply_for_chunk(
            &mut engine,
            &mut tracker.reader,
            &mut tracker.terminal_grid,
            b"\x1b]2;build; test\x07\x1b]7;file://host/tmp\x1b\\\x1b]777;notify;Build;done; ok\x07\
              \x1b]9;4;1;50\x07ding\x07\r\n\r\n\x1b]133;D;2\x07",
        );
        assert_eq!(
            tracker.reader.signals,
            [
                PaneSignal::Title("build; test".into()),
                PaneSignal::WorkingDirectory("file://host/tmp".into()),
                PaneSignal::Notification {
                    title: Some("Build".into()),
                    body: "done; ok".into(),
                },
                PaneSignal::Progress("4;1;50".into()),
                PaneSignal::Bell,
                PaneSignal::PromptMark {
                    kind: PromptMarkKind::CommandFinished,
                    line: 2,
                    exit_status: Some(2),
                },
            ]
        );
    }

    /// What a pane signals is retained as state: a reader that missed every
    /// event still sees the latest title and progress, how many notifications
    /// and bells there were, and a revision that moves only when they change.
    #[test]
    fn signals_are_retained_as_current_state() {
        let mut tracker = PaneCursorTracker::new(4, 40);
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
        let mut retained = RetainedPaneSignals::default();
        let mut feed = |bytes: &[u8], retained: &mut RetainedPaneSignals| {
            let _ = protocol_reply_for_chunk(
                &mut engine,
                &mut tracker.reader,
                &mut tracker.terminal_grid,
                bytes,
            );
            for signal in tracker.reader.signals.drain(..) {
                retained.apply(&signal);
            }
        };
        // Claude Code's titles as it starts, works and then waits.
        feed("\x1b]0;✳ Claude Code\x07".as_bytes(), &mut retained);
        feed("\x1b]0;◐ Claude Code\x07".as_bytes(), &mut retained);
        feed(
            "\x1b]0;◑ Touch probe marker file\x07".as_bytes(),
            &mut retained,
        );
        feed(
            "\x1b]0;✳ Touch probe marker file\x07".as_bytes(),
            &mut retained,
        );
        assert_eq!(retained.title.as_deref(), Some("✳ Touch probe marker file"));
        assert_eq!(retained.revision, 4);
        // The same title again changes nothing.
        feed(
            "\x1b]0;✳ Touch probe marker file\x07".as_bytes(),
            &mut retained,
        );
        assert_eq!(retained.revision, 4);
        // Notifications and bells count, so a repeated one is still seen.
        feed(
            b"\x1b]9;4;3\x07\x1b]777;notify;Claude;needs you\x07\x07\x07",
            &mut retained,
        );
        assert_eq!(retained.progress.as_deref(), Some("4;3"));
        assert_eq!(retained.notifications, 1);
        assert_eq!(
            retained.last_notification.as_deref(),
            Some("Claude: needs you")
        );
        assert_eq!(retained.bells, 2);
        assert_eq!(retained.revision, 8);
        // The directory is kept; a prompt starting changes nothing.
        feed(b"\x1b]7;file://host/tmp\x1b\\\x1b]133;A\x07", &mut retained);
        assert_eq!(
            retained.working_directory.as_deref(),
            Some("file://host/tmp")
        );
        assert_eq!(retained.revision, 9);
        // A finished command's status is kept until the next one starts.
        feed(b"\x1b]133;C\x07out\r\n\x1b]133;D;2\x07", &mut retained);
        assert_eq!(retained.exit_status, Some(2));
        assert_eq!(retained.revision, 10);
        feed(b"\x1b]133;C\x07", &mut retained);
        assert_eq!(retained.exit_status, None);
        assert_eq!(retained.revision, 11);
    }

    /// An attached client's frames keep a replica of the pane's grid level with it: a reset frame
    /// first, nothing while the grid is unchanged, then what changed; a reset starts it over, and
    /// detaching forgets it.
    /// An observer, not attached to the pane's session, gets frames that keep its replica level
    /// with the grid as an attached client does; a reset starts it over, and a target that does
    /// not exist is skipped.
    #[switchy_async::test]
    async fn observed_pane_frames_keep_an_unattached_replica_level_with_the_grid() {
        use bmux_pane_runtime_state::SessionRuntimeManagerApi as Api;
        let session_id = SessionId(crate::ids::new_uuid());
        let observer = ClientId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let runtime = runtime_with_panes(&[pane_id]);
        let pane = runtime.panes.get(&pane_id).expect("pane should exist");
        set_pane_grid(pane, 20, 4);
        let grid = Arc::clone(&pane.terminal_grid);
        grid.lock().unwrap().process(b"one\r\ntwo\r\n");
        let adapter = adapter_for_manager(manager_with_runtime(session_id, runtime));
        let targets = [(session_id, pane_id), (session_id, crate::ids::new_uuid())];
        let mut replica = TerminalGrid::new(1, 1, GridLimits::default()).unwrap();
        let mut cursor = bmux_terminal_grid::ReplicaCursor::default();
        let mut follow = |adapter: &ServerSessionRuntimeAdapter| {
            let (frames, pending) = Api::observe_pane_frames(adapter, observer, &targets, 0)
                .expect("an observer gets frames");
            assert!(!pending);
            let mut reset = false;
            for frame in &frames {
                reset |= replica
                    .apply_frame(&frame.encoded, &mut cursor)
                    .expect("frame applies")
                    .reset;
            }
            assert_eq!(
                replica.snapshot(0, 100).rows,
                grid.lock().unwrap().grid().snapshot(0, 100).rows
            );
            (frames.len(), reset)
        };
        assert_eq!(follow(&adapter), (1, true), "first a reset frame");
        assert_eq!(follow(&adapter), (0, false), "nothing while unchanged");
        grid.lock().unwrap().process(b"three");
        assert_eq!(follow(&adapter), (1, false), "then what changed");
        Api::reset_observed_pane_frames(&adapter, observer, &targets).unwrap();
        assert_eq!(follow(&adapter), (1, true), "a reset replica starts over");
    }

    #[switchy_async::test]
    async fn attach_pane_frames_keep_a_client_replica_level_with_the_pane_grid() {
        use bmux_pane_runtime_state::SessionRuntimeManagerApi as Api;
        let session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime.attached_clients.insert(client_id);
        let pane = runtime.panes.get(&pane_id).expect("pane should exist");
        set_pane_grid(pane, 20, 4);
        let grid = Arc::clone(&pane.terminal_grid);
        grid.lock().unwrap().process(b"one\r\ntwo\r\n");
        let adapter = adapter_for_manager(manager_with_runtime(session_id, runtime));
        let mut replica = TerminalGrid::new(1, 1, GridLimits::default()).unwrap();
        let mut cursor = bmux_terminal_grid::ReplicaCursor::default();
        let mut reset_seen = false;
        let mut apply = |frames: &[bmux_pane_runtime_state::AttachPaneFrame]| {
            for frame in frames {
                reset_seen |= replica
                    .apply_frame(&frame.encoded, &mut cursor)
                    .expect("frame applies")
                    .reset;
            }
            (
                replica.snapshot(0, 100).rows,
                std::mem::take(&mut reset_seen),
            )
        };
        let frames = |adapter: &ServerSessionRuntimeAdapter| {
            let (frames, pending) =
                Api::attach_pane_frames(adapter, session_id, client_id, &[pane_id], usize::MAX)
                    .expect("attached client gets frames");
            assert!(!pending);
            frames
        };
        let level = |(rows, reset): (Vec<bmux_terminal_grid::RowSnapshot>, bool)| {
            assert_eq!(rows, grid.lock().unwrap().grid().snapshot(0, 100).rows);
            reset
        };

        let first = frames(&adapter);
        assert_eq!(first.len(), 1);
        assert!(
            level(apply(&first)),
            "a new replica's first frame is a reset frame"
        );
        assert!(
            frames(&adapter).is_empty(),
            "an unchanged grid needs no frame"
        );
        grid.lock().unwrap().process(b"three\r\nfour");
        assert!(!level(apply(&frames(&adapter))));

        Api::reset_attach_pane_frames(&adapter, session_id, client_id, &[pane_id]).unwrap();
        assert!(
            level(apply(&frames(&adapter))),
            "a reset replica starts over"
        );

        let other = ClientId(crate::ids::new_uuid());
        assert_eq!(
            Api::attach_pane_frames(&adapter, session_id, other, &[pane_id], usize::MAX).err(),
            Some(SessionRuntimeError::NotAttached)
        );
    }

    #[switchy_async::test]
    async fn attach_grid_snapshot_state_reports_retained_scrollback_without_encoding_it() {
        let session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime.attached_clients.insert(client_id);
        let pane = runtime.panes.get(&pane_id).expect("pane should exist");
        set_pane_grid(pane, 5, 2);
        pane.terminal_grid
            .lock()
            .expect("grid lock should be available")
            .process(b"line1\r\nline2\r\nline3");
        let adapter = adapter_for_manager(manager_with_runtime(session_id, runtime));

        let state = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_grid_snapshot_state(
            &adapter,
            session_id,
            client_id,
            &[pane_id],
            usize::MAX,
        )
        .expect("attached client should receive grid snapshot");

        assert_eq!(state.snapshots.len(), 1);
        let snapshot =
            serde_json::from_slice::<bmux_terminal_grid::GridSnapshot>(&state.snapshots[0].encoded)
                .expect("snapshot payload should decode");
        let retained = snapshot
            .rows
            .iter()
            .map(|row| {
                row.runs
                    .iter()
                    .map(|run| run.text.as_str())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>();

        assert!(!retained.iter().any(|row| row == "line1"));
        assert!(retained.iter().any(|row| row == "line3"));
        assert_eq!(retained.len(), usize::from(snapshot.height));
        assert!(snapshot.scrollback_rows > 0);
    }

    #[switchy_async::test]
    async fn attach_grid_snapshot_state_omits_panes_beyond_reply_budget() {
        fn fill(pane: &PaneRuntimeHandle) {
            set_pane_grid(pane, 200, 60);
            // Alternating styles force one run per cell, inflating the encoding.
            let mut bytes = Vec::new();
            for row in 0..60 {
                for col in 0..200 {
                    let color = 31 + (row + col) % 6;
                    bytes.extend_from_slice(format!("\x1b[{color}mx").as_bytes());
                }
                bytes.extend_from_slice(b"\r\n");
            }
            pane.terminal_grid
                .lock()
                .expect("grid lock should be available")
                .process(&bytes);
        }
        let session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());

        // Measure one pane, then request enough identical panes to overflow.
        let probe = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[probe]);
        runtime.attached_clients.insert(client_id);
        fill(runtime.panes.get(&probe).expect("pane should exist"));
        let adapter = adapter_for_manager(manager_with_runtime(session_id, runtime));
        let one = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_grid_snapshot_state(
            &adapter,
            session_id,
            client_id,
            &[probe],
            usize::MAX,
        )
        .expect("one pane fits the reply budget");
        let pane_bytes = one.snapshots[0].encoded.len();
        assert!(pane_bytes * 2 < REPLY_WIRE_BUDGET);

        let requested = REPLY_WIRE_BUDGET / pane_bytes + 2;
        let pane_ids = (0..requested)
            .map(|_| crate::ids::new_uuid())
            .collect::<Vec<_>>();
        let mut runtime = runtime_with_panes(&pane_ids);
        runtime.attached_clients.insert(client_id);
        for pane_id in &pane_ids {
            fill(runtime.panes.get(pane_id).expect("pane should exist"));
        }
        let adapter = adapter_for_manager(manager_with_runtime(session_id, runtime));
        let state = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_grid_snapshot_state(
            &adapter,
            session_id,
            client_id,
            &pane_ids,
            usize::MAX,
        )
        .expect("oversized aggregate degrades to the panes that fit");

        assert!(!state.snapshots.is_empty());
        assert!(state.snapshots.len() < requested);
        let total = state
            .snapshots
            .iter()
            .map(|snapshot| snapshot.encoded.len())
            .sum::<usize>();
        assert!(total <= REPLY_WIRE_BUDGET);
        const { assert!(REPLY_WIRE_BUDGET < bmux_plugin_sdk::abi::MAX_BUFFER_BYTES) };
    }

    #[switchy_async::test]
    async fn attach_grid_window_state_returns_bounded_scrollback_window() {
        let session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime.attached_clients.insert(client_id);
        let pane = runtime.panes.get(&pane_id).expect("pane should exist");
        set_pane_grid(pane, 5, 2);
        pane.terminal_grid
            .lock()
            .expect("grid lock should be available")
            .process(b"line1\r\nline2\r\nline3");
        let adapter = adapter_for_manager(manager_with_runtime(session_id, runtime));

        let state = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_grid_window_state(
            &adapter,
            session_id,
            client_id,
            &[bmux_pane_runtime_state::AttachPaneGridWindowRequest {
                pane_id,
                scrollback_offset: 1,
                rows: 2,
                anchor_total_scrolled_rows: None,
                pin_id: None,
            }],
        )
        .expect("attached client should receive grid window");

        assert_eq!(state.windows.len(), 1);
        assert_eq!(state.windows[0].scrollback_offset, 1);
        assert!(state.windows[0].max_scrollback_offset >= 1);
        let snapshot =
            serde_json::from_slice::<bmux_terminal_grid::GridSnapshot>(&state.windows[0].encoded)
                .expect("window payload should decode");
        assert_eq!(snapshot.rows.len(), 2);
        let retained = snapshot
            .rows
            .iter()
            .map(|row| {
                row.runs
                    .iter()
                    .map(|run| run.text.as_str())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>();
        assert!(retained.iter().any(|row| row == "line1"));
        assert!(retained.iter().any(|row| row == "line2"));
        assert!(!retained.iter().any(|row| row == "line3"));
    }

    #[switchy_async::test]
    async fn attach_grid_window_state_anchors_offset_when_output_appends() {
        let session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime.attached_clients.insert(client_id);
        let pane = runtime.panes.get(&pane_id).expect("pane should exist");
        set_pane_grid(pane, 20, 2);
        pane.terminal_grid
            .lock()
            .expect("grid lock should be available")
            .process(b"line1\r\nline2\r\nline3");
        let adapter = adapter_for_manager(manager_with_runtime(session_id, runtime));

        let first = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_grid_window_state(
            &adapter,
            session_id,
            client_id,
            &[bmux_pane_runtime_state::AttachPaneGridWindowRequest {
                pane_id,
                scrollback_offset: 1,
                rows: 2,
                anchor_total_scrolled_rows: None,
                pin_id: None,
            }],
        )
        .expect("attached client should receive first window")
        .windows
        .into_iter()
        .next()
        .expect("first window should exist");

        {
            let manager = adapter
                .inner
                .lock()
                .expect("manager lock should be available");
            let pane = manager
                .runtimes
                .get(&session_id)
                .and_then(|runtime| runtime.panes.get(&pane_id))
                .expect("pane should still exist");
            pane.terminal_grid
                .lock()
                .expect("grid lock should be available")
                .process(b"\r\nline4");
        }

        let second = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_grid_window_state(
            &adapter,
            session_id,
            client_id,
            &[bmux_pane_runtime_state::AttachPaneGridWindowRequest {
                pane_id,
                scrollback_offset: first.scrollback_offset,
                rows: 2,
                anchor_total_scrolled_rows: Some(first.total_scrolled_rows),
                pin_id: None,
            }],
        )
        .expect("attached client should receive anchored window")
        .windows
        .into_iter()
        .next()
        .expect("anchored window should exist");

        assert!(second.total_scrolled_rows > first.total_scrolled_rows);
        assert_eq!(
            second.scrollback_offset,
            first.scrollback_offset
                + usize::try_from(second.total_scrolled_rows - first.total_scrolled_rows)
                    .expect("test delta fits usize")
        );
        assert_eq!(
            second.anchor_delta_rows,
            second.scrollback_offset - first.scrollback_offset
        );
        assert!(!second.anchor_clamped);
    }

    #[test]
    fn terminal_grid_clone_preserves_frozen_history_after_live_eviction_and_clear() {
        let mut live = bmux_terminal_grid::TerminalGrid::new(
            8,
            2,
            bmux_terminal_grid::GridLimits { scrollback_rows: 2 },
        )
        .expect("grid dimensions are valid");
        live.process(b"line-1\r\nline-2\r\nline-3");
        let frozen = live.clone();
        let frozen_before = frozen.snapshot(1, 2);

        live.process(b"\r\nline-4\r\nline-5\r\nline-6\x1b[3J");

        assert_eq!(frozen.snapshot(1, 2), frozen_before);
        assert_ne!(live.snapshot(1, 2), frozen_before);
    }

    #[switchy_async::test]
    async fn frozen_scrollback_pin_serves_immutable_windows_while_live_grid_advances() {
        let session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime.attached_clients.insert(client_id);
        let pane = runtime.panes.get(&pane_id).expect("pane should exist");
        set_pane_grid(pane, 8, 2);
        pane.terminal_grid
            .lock()
            .expect("grid lock should be available")
            .process(b"line-1\r\nline-2\r\nline-3");
        let adapter = adapter_for_manager(manager_with_runtime(session_id, runtime));

        let pin = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_scrollback_pin(
            &adapter, session_id, client_id, pane_id,
        )
        .expect("attached client should pin pane history");
        let request = bmux_pane_runtime_state::AttachPaneGridWindowRequest {
            pane_id,
            scrollback_offset: 1,
            rows: 2,
            anchor_total_scrolled_rows: None,
            pin_id: Some(pin.pin_id),
        };
        let before = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_grid_window_state(
            &adapter,
            session_id,
            client_id,
            &[request],
        )
        .expect("pinned window should be available")
        .windows
        .into_iter()
        .next()
        .expect("pinned window should exist");

        {
            let manager = adapter
                .inner
                .lock()
                .expect("manager lock should be available");
            let pane = manager
                .runtimes
                .get(&session_id)
                .and_then(|runtime| runtime.panes.get(&pane_id))
                .expect("pane should still exist");
            pane.terminal_grid
                .lock()
                .expect("grid lock should be available")
                .process(b"\r\nline-4\r\nline-5\r\nline-6");
        }

        let after = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_grid_window_state(
            &adapter,
            session_id,
            client_id,
            &[request],
        )
        .expect("pinned window should remain available")
        .windows
        .into_iter()
        .next()
        .expect("pinned window should exist");

        assert_eq!(after.encoded, before.encoded);
        assert_eq!(after.total_scrolled_rows, pin.total_scrolled_rows);
        assert_eq!(after.max_scrollback_offset, pin.max_scrollback_offset);
        assert_eq!(after.stream_end, pin.stream_end);
        assert_eq!(after.scrollback_offset, 1);
        assert_eq!(after.anchor_delta_rows, 0);
        assert!(!after.anchor_clamped);
    }

    #[test]
    fn encoded_byte_count_matches_json_and_rejects_overflow() {
        let value = serde_json::json!({"text": "雪\n\"\\", "rows": [1, 2, 3]});
        let mut count = EncodedByteCount::default();
        serde_json::to_writer(&mut count, &value).unwrap();
        assert_eq!(count.0, serde_json::to_vec(&value).unwrap().len());
        let mut bounded = EncodedByteCount(0, 2);
        std::io::Write::write_all(&mut bounded, b"ab").unwrap();
        assert!(std::io::Write::write_all(&mut bounded, b"c").is_err());
        assert_eq!(bounded.0, 2);
        let mut exhausted = EncodedByteCount(usize::MAX, usize::MAX);
        assert!(std::io::Write::write(&mut exhausted, b"x").is_err());
        assert_eq!(exhausted.0, usize::MAX);
    }

    #[switchy_async::test]
    async fn removed_pane_cannot_store_an_in_flight_scrollback_capture() {
        assert_retired_pane_capture_rejected(false);
    }

    #[switchy_async::test]
    async fn replaced_pane_cannot_store_an_in_flight_scrollback_capture() {
        assert_retired_pane_capture_rejected(true);
    }

    fn assert_retired_pane_capture_rejected(replace: bool) {
        let session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime.attached_clients.insert(client_id);
        let grid = Arc::clone(&runtime.panes[&pane_id].terminal_grid);
        let adapter = adapter_for_manager(manager_with_runtime(session_id, runtime));
        let grid_guard = grid.lock().expect("grid lock");
        let initial_handles = Arc::strong_count(&grid);
        std::thread::scope(|scope| {
            let capture = scope.spawn(|| {
                bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_scrollback_pin(
                    &adapter, session_id, client_id, pane_id,
                )
            });
            // Resolving the pane clones its grid handle before attempting the
            // grid lock held above, establishing the race without a sleep.
            let deadline = Instant::now() + Duration::from_secs(5);
            while Arc::strong_count(&grid) == initial_handles && Instant::now() < deadline {
                switchy_thread::thread::yield_now();
            }
            let resolved = Arc::strong_count(&grid) > initial_handles;
            {
                let mut manager = adapter.inner.lock().expect("manager lock");
                let runtime = manager.runtimes.get_mut(&session_id).unwrap();
                runtime.panes.remove(&pane_id);
                if replace {
                    let replacement = runtime_with_panes(&[pane_id])
                        .panes
                        .remove(&pane_id)
                        .expect("replacement pane");
                    runtime.panes.insert(pane_id, replacement);
                }
            }
            drop(grid_guard);
            let result = capture.join().expect("capture thread");
            assert!(resolved, "capture must resolve the pane before removal");
            assert!(matches!(result, Err(SessionRuntimeError::NotFound)));
        });
        let manager = adapter.inner.lock().expect("manager lock");
        assert!(manager.runtimes[&session_id].scrollback_pins.is_empty());
    }

    #[switchy_async::test]
    async fn grid_builders_share_capture_admission_and_release_working_capacity() {
        let session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime.attached_clients.insert(client_id);
        let adapter = adapter_for_manager(manager_with_runtime(session_id, runtime));
        let other = ServerSessionRuntimeAdapter::new(Arc::clone(&adapter.inner));
        let occupied = PinReservation::acquire(
            &adapter.retained_pin_bytes,
            RETAINED_PIN_BUDGET - 2 * RESPONSE_OUTPUT_BUDGET + 1,
        )
        .unwrap();
        assert!(matches!(
            bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_grid_snapshot_state(
                &other,
                session_id,
                client_id,
                &[pane_id],
                1,
            ),
            Err(SessionRuntimeError::ResponseBudgetExceeded)
        ));
        assert!(matches!(
            bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_grid_window_state(
                &other,
                session_id,
                client_id,
                &[],
            ),
            Err(SessionRuntimeError::ResponseBudgetExceeded)
        ));
        drop(occupied);
        let response =
            bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_grid_snapshot_state(
                &other,
                session_id,
                client_id,
                &[pane_id],
                1,
            )
            .unwrap();
        assert_eq!(response.snapshots.len(), 1);
        // Only construction is charged; the returned response is caller-owned.
        assert_eq!(adapter.retained_pin_bytes.load(Ordering::Acquire), 0);
        bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_grid_window_state(
            &other,
            session_id,
            client_id,
            &[],
        )
        .unwrap();
        assert_eq!(adapter.retained_pin_bytes.load(Ordering::Acquire), 0);
        assert!(
            bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_grid_snapshot_state(
                &other,
                SessionId(crate::ids::new_uuid()),
                client_id,
                &[pane_id],
                1,
            )
            .is_err()
        );
        assert_eq!(adapter.retained_pin_bytes.load(Ordering::Acquire), 0);
    }

    fn history_fixture_origin() -> bmux_plugin_sdk::PluginHostServices {
        bmux_plugin_sdk::PluginHostServices::new(
            // The SDK callback contract returns PluginError by value.
            #[allow(clippy::result_large_err)]
            |request| {
                assert_eq!(
                    request.capability,
                    bmux_plugin_sdk::CORE_INVOCATION_CAPABILITY
                );
                assert_eq!(request.kind, bmux_plugin_sdk::ServiceKind::Query);
                assert_eq!(
                    request.interface,
                    bmux_plugin_sdk::CORE_INVOCATION_INTERFACE_V1
                );
                assert_eq!(
                    request.operation,
                    bmux_plugin_sdk::CORE_INVOCATION_STREAM_ORIGIN_OPERATION_V1
                );
                bmux_plugin_sdk::encode_service_message(&bmux_plugin::StreamOrigin::Local)
            },
        )
    }

    /// Routes typed calls through this test's own host handles.
    struct HistoryDispatch(Uuid, PaneRuntimeHost);
    impl bmux_plugin_sdk::TypedDispatchClient for HistoryDispatch {
        fn invoke_service_raw(
            &mut self,
            capability: &str,
            kind: bmux_ipc::InvokeServiceKind,
            interface_id: &str,
            operation: &str,
            payload: Vec<u8>,
        ) -> impl std::future::Future<Output = bmux_plugin_sdk::TypedDispatchClientResult<Vec<u8>>>
        {
            let host = self.1.clone();
            let mut payload = Some(payload);
            std::future::poll_fn(move |_| {
                let host = &host;
                let payload = payload.take().expect("transport polled after completion");
                std::task::Poll::Ready({
                    use bmux_plugin_sdk::{
                        ApiVersion, HostConnectionInfo, HostMetadata, HostScope,
                        NativeServiceContext, ProviderId, RegisteredService, ServiceKind,
                        ServiceRequest,
                    };
                    assert_eq!(kind, bmux_ipc::InvokeServiceKind::Query);
                    let response = crate::handlers::route(
                        host,
                        &NativeServiceContext {
                            host_service_port: Some(history_fixture_origin()),
                            plugin_id: "bmux.pane-runtime".into(),
                            request: ServiceRequest {
                                caller_plugin_id: String::new(),
                                service: RegisteredService {
                                    capability: HostScope::new(capability).unwrap(),
                                    kind: ServiceKind::Query,
                                    interface_id: interface_id.into(),
                                    provider: ProviderId::Plugin("bmux.pane-runtime".into()),
                                },
                                operation: operation.into(),
                                payload,
                            },
                            required_capabilities: vec![],
                            provided_capabilities: vec![],
                            services: vec![],
                            available_capabilities: vec![],
                            enabled_plugins: vec![],
                            plugin_search_roots: vec![],
                            host: HostMetadata {
                                product_name: "bmux".into(),
                                product_version: env!("CARGO_PKG_VERSION").into(),
                                plugin_api_version: ApiVersion::new(1, 0),
                                plugin_abi_version: ApiVersion::new(1, 0),
                            },
                            connection: HostConnectionInfo {
                                config_dir: String::new(),
                                config_dir_candidates: vec![],
                                runtime_dir: String::new(),
                                data_dir: String::new(),
                                state_dir: String::new(),
                            },
                            settings: None,
                            plugin_settings_map: std::collections::BTreeMap::new(),
                            caller_client_id: Some(self.0),
                            cancellation: bmux_plugin_sdk::CancellationToken::default(),
                            host_kernel_bridge: None,
                        },
                    );
                    assert_eq!(response.error, None);
                    Ok(response.payload)
                })
            })
        }
    }

    #[cfg(feature = "image-registry")]
    #[switchy_async::test]
    async fn generated_history_images_survive_live_reset_and_reject_stale_capture() {
        use bmux_pane_runtime_plugin_api::attach_runtime_state::client;

        let session = SessionId(crate::ids::new_uuid());
        let caller = ClientId(crate::ids::new_uuid());
        let pane = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane]);
        runtime.attached_clients.insert(caller);
        let images = Arc::clone(&runtime.panes[&pane].image_registry);
        let mut tracker = PaneCursorTracker::new(3, 80);
        let mut protocol = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
        let mut collector = image_collector();
        follow_image_chunk(
            &mut tracker.reader,
            &mut tracker.terminal_grid,
            &mut protocol,
            &mut images.lock().unwrap(),
            &mut collector,
            b"\x1bPq\"1;1;1;6#1;2;100;0;0~\x1b\\\r\n\r\n\r\n",
            &mut Vec::new(),
        )
        .unwrap();
        *runtime.panes[&pane].terminal_grid.lock().unwrap() = tracker.terminal_grid;
        let manager = Arc::new(Mutex::new(manager_with_runtime(session, runtime)));
        let padding = Arc::new(switchy_thread::sync::RwLock::new(PanePaddingRuntimeHandle(
            Arc::clone(&manager),
        )));
        let host = PaneRuntimeHost::detached();
        host.states().register(&padding);
        let adapter = ServerSessionRuntimeAdapter::new(manager);
        let handle = Arc::new(switchy_thread::sync::RwLock::new(
            bmux_pane_runtime_state::SessionRuntimeManagerHandle::new(adapter),
        ));
        host.states().register(&handle);
        let mut dispatch = HistoryDispatch(caller.0, host.clone());
        let capture = client::attach_history_capture_v1(&mut dispatch, session.0, pane)
            .await
            .unwrap()
            .unwrap();
        images.lock().unwrap().reset();
        let response = client::attach_history_images_v2(
            &mut dispatch,
            session.0,
            pane,
            capture.pin.pin_id,
            capture.capture_id,
            80,
            1,
            3,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.capture_id, capture.capture_id);
        let recovered: Vec<bmux_attach_image_protocol::AttachPaneImage> =
            serde_json::from_slice(&response.encoded).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].position_row, 0);
        assert!(!recovered[0].raw_data.is_empty());
        assert_anchored_history_image_recovery(&mut dispatch, session, pane, &capture).await;
        assert_history_image_window_replacement(
            &mut dispatch,
            session,
            pane,
            &capture,
            &response.encoded,
        )
        .await;
        assert!(
            client::attach_history_images_v2(
                &mut dispatch,
                session.0,
                pane,
                capture.pin.pin_id,
                crate::ids::new_uuid(),
                80,
                1,
                3,
            )
            .await
            .unwrap()
            .is_err()
        );
    }

    #[cfg(feature = "image-registry")]
    async fn assert_anchored_history_image_recovery(
        dispatch: &mut HistoryDispatch,
        session: SessionId,
        pane: Uuid,
        capture: &bmux_pane_runtime_plugin_api::attach_runtime_state::HistoryCaptureV1,
    ) {
        use bmux_pane_runtime_plugin_api::attach_runtime_state::client;
        for width in [80, 40, 120, 80] {
            for line in [0, 1, 0] {
                let response = client::attach_history_images_v3(
                    dispatch,
                    session.0,
                    pane,
                    capture.pin.pin_id,
                    capture.capture_id,
                    width,
                    line,
                    0,
                    3,
                )
                .await
                .unwrap()
                .unwrap();
                assert_eq!(response.capture_id, capture.capture_id);
                let images: Vec<bmux_attach_image_protocol::AttachPaneImage> =
                    serde_json::from_slice(&response.encoded).unwrap();
                if line == 0 {
                    assert_eq!(images.len(), 1);
                    assert_eq!((images[0].position_row, images[0].position_col), (0, 0));
                    assert!(!images[0].raw_data.is_empty());
                } else {
                    assert!(
                        images.is_empty(),
                        "image-free window must replace old graphics"
                    );
                }
            }
        }
        assert!(
            client::attach_history_images_v3(
                dispatch,
                session.0,
                pane,
                capture.pin.pin_id,
                crate::ids::new_uuid(),
                80,
                0,
                0,
                3,
            )
            .await
            .unwrap()
            .is_err()
        );
    }

    #[cfg(feature = "image-registry")]
    async fn assert_history_image_window_replacement(
        dispatch: &mut HistoryDispatch,
        session: SessionId,
        pane: Uuid,
        capture: &bmux_pane_runtime_plugin_api::attach_runtime_state::HistoryCaptureV1,
        expected: &[u8],
    ) {
        // Each query is a complete window replacement. Visiting the live edge
        // must clear the overlay without destroying the immutable capture.
        for offset in [0, 1, 0, 1] {
            let window = bmux_pane_runtime_plugin_api::attach_runtime_state::client::attach_history_images_v2(
                dispatch,
                session.0,
                pane,
                capture.pin.pin_id,
                capture.capture_id,
                80,
                offset,
                3,
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(window.capture_id, capture.capture_id);
            let projected: Vec<bmux_attach_image_protocol::AttachPaneImage> =
                serde_json::from_slice(&window.encoded).unwrap();
            if offset == 0 {
                assert_eq!(
                    projected,
                    [] as [bmux_attach_image_protocol::AttachPaneImage; 0]
                );
            } else {
                assert_eq!(window.encoded, expected);
            }
        }
    }

    async fn assert_history_fetch_errors(
        dispatch: &mut HistoryDispatch,
        session: SessionId,
        pane: Uuid,
        capture: &bmux_pane_runtime_plugin_api::attach_runtime_state::HistoryCaptureV1,
    ) {
        use bmux_pane_runtime_plugin_api::attach_runtime_state::{HistoryFetchError, client};
        for (identity, line, offset, columns, expected) in [
            (
                crate::ids::new_uuid(),
                0,
                0,
                2,
                HistoryFetchError::StaleCapture,
            ),
            (
                capture.capture_id,
                u32::try_from(capture.history_line_count).unwrap(),
                0,
                2,
                HistoryFetchError::Unavailable,
            ),
            (
                capture.capture_id,
                0,
                u32::MAX,
                2,
                HistoryFetchError::InvalidOffset,
            ),
            (
                capture.capture_id,
                0,
                0,
                0,
                HistoryFetchError::BudgetExhausted,
            ),
        ] {
            assert_eq!(
                client::attach_history_slice_v1(
                    dispatch,
                    session.0,
                    pane,
                    capture.pin.pin_id,
                    identity,
                    line,
                    offset,
                    columns,
                    100,
                )
                .await
                .unwrap(),
                Err(expected)
            );
        }
    }

    #[switchy_async::test]
    #[allow(
        clippy::too_many_lines,
        reason = "capture, fetch, replacement and stale-identity assertions share one registered runtime lifecycle"
    )]
    async fn generated_history_capture_and_fetch_share_logical_bounds() {
        use bmux_pane_runtime_plugin_api::attach_runtime_state::{
            HistoryEnd, HistoryFetchError, client,
        };

        let session = SessionId(crate::ids::new_uuid());
        let caller = ClientId(crate::ids::new_uuid());
        let pane = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane]);
        runtime.attached_clients.insert(caller);
        for _ in 0..100 {
            runtime.panes[&pane]
                .terminal_grid
                .lock()
                .unwrap()
                .process(b"first\r\n");
        }
        let adapter = adapter_for_manager(manager_with_runtime(session, runtime));
        let handle = Arc::new(switchy_thread::sync::RwLock::new(
            bmux_pane_runtime_state::SessionRuntimeManagerHandle::new(adapter),
        ));
        let host = PaneRuntimeHost::detached();
        host.states().register(&handle);
        let mut dispatch = HistoryDispatch(caller.0, host);
        let capture = client::attach_history_capture_v1(&mut dispatch, session.0, pane)
            .await
            .unwrap()
            .unwrap();
        assert!(capture.history_line_count > 0);
        let row = client::attach_main_row_slice_v1(
            &mut dispatch,
            session.0,
            pane,
            capture.pin.pin_id,
            capture.capture_id,
            0,
            0,
            4096,
            65536,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(row.next_cell_offset <= u64::from(capture.width));
        let stale = client::attach_main_row_slice_v1(
            &mut dispatch,
            session.0,
            pane,
            capture.pin.pin_id,
            crate::ids::new_uuid(),
            0,
            0,
            4096,
            65536,
        )
        .await
        .unwrap();
        assert!(matches!(
            stale,
            Err(
                bmux_pane_runtime_plugin_api::attach_runtime_state::HistoryFetchError::StaleCapture
            )
        ));
        let slice = client::attach_history_slice_v1(
            &mut dispatch,
            session.0,
            pane,
            capture.pin.pin_id,
            capture.capture_id,
            0,
            0,
            2,
            100,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(slice.next_cell_offset, 2);
        assert_eq!(slice.end_kind, HistoryEnd::Continue);
        let slice = client::attach_history_slice_v1(
            &mut dispatch,
            session.0,
            pane,
            capture.pin.pin_id,
            capture.capture_id,
            0,
            2,
            100,
            100,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(slice.end_kind, HistoryEnd::HardBreak);
        let cells: Vec<(String, u8, bmux_terminal_grid::Style)> =
            serde_json::from_slice(&slice.encoded).unwrap();
        assert_eq!(
            cells.iter().map(|cell| cell.0.as_str()).collect::<String>(),
            "rst"
        );
        assert_history_fetch_errors(&mut dispatch, session, pane, &capture).await;
        // Replacement makes the previous capture identity unusable even though
        // the same session and pane remain attached.
        let replacement = client::attach_history_capture_v1(&mut dispatch, session.0, pane)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(replacement.capture_id, capture.capture_id);
        assert_eq!(
            client::attach_history_slice_v1(
                &mut dispatch,
                session.0,
                pane,
                capture.pin.pin_id,
                capture.capture_id,
                0,
                0,
                2,
                100,
            )
            .await
            .unwrap(),
            Err(HistoryFetchError::StaleCapture)
        );
    }

    #[switchy_async::test]
    async fn history_fetch_uses_frozen_pin_and_rejects_released_token() {
        use bmux_pane_runtime_state::SessionRuntimeManagerApi;
        let session = SessionId(crate::ids::new_uuid());
        let client = ClientId(crate::ids::new_uuid());
        let pane = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane]);
        runtime.attached_clients.insert(client);
        for _ in 0..100 {
            runtime.panes[&pane]
                .terminal_grid
                .lock()
                .unwrap()
                .process(b"first\r\n");
        }
        let adapter = adapter_for_manager(manager_with_runtime(session, runtime));
        let pin = adapter
            .attach_scrollback_pin(session, client, pane)
            .unwrap();
        let (encoded, next, end) = adapter
            .attach_history_slice(
                session,
                client,
                pane,
                (pin.pin_id, pin.capture_id),
                (0, 0),
                (2, 100),
            )
            .unwrap();
        assert_ne!(encoded, [] as [u8; 0]);
        assert_eq!(next, 2);
        assert_eq!(end, bmux_pane_runtime_state::HistoryEnd::Continue);
        assert!(
            adapter
                .attach_history_slice(
                    session,
                    client,
                    pane,
                    (pin.pin_id, crate::ids::new_uuid()),
                    (0, 0),
                    (10, 100)
                )
                .is_err()
        );
        // An unavailable line must not manufacture content from the live grid.
        assert!(
            adapter
                .attach_history_slice(
                    session,
                    client,
                    pane,
                    (pin.pin_id, pin.capture_id),
                    (usize::MAX, 0),
                    (10, 100)
                )
                .is_err()
        );
        adapter
            .attach_scrollback_unpin(session, client, pane, pin.pin_id)
            .unwrap();
        assert!(
            adapter
                .attach_history_slice(
                    session,
                    client,
                    pane,
                    (pin.pin_id, pin.capture_id),
                    (0, 0),
                    (10, 100)
                )
                .is_err()
        );
    }

    #[switchy_async::test]
    async fn failed_pin_publication_releases_capture_and_allows_retry() {
        let session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime.attached_clients.insert(client_id);
        // Exhaust tokens to fail after capture and admission, before insertion.
        runtime.next_scrollback_pin_id = u64::MAX;
        let adapter = adapter_for_manager(manager_with_runtime(session_id, runtime));
        let result = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_scrollback_pin(
            &adapter, session_id, client_id, pane_id,
        );
        assert!(matches!(result, Err(SessionRuntimeError::Closed)));
        assert_eq!(adapter.retained_pin_bytes.load(Ordering::Acquire), 0);
        {
            let mut manager = adapter.inner.lock().unwrap();
            let runtime = manager.runtimes.get_mut(&session_id).unwrap();
            assert!(runtime.scrollback_pins.is_empty());
            runtime.next_scrollback_pin_id = 1;
        }
        let pin = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_scrollback_pin(
            &adapter, session_id, client_id, pane_id,
        )
        .unwrap();
        assert!(adapter.retained_pin_bytes.load(Ordering::Acquire) > 0);
        bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_scrollback_unpin(
            &adapter, session_id, client_id, pane_id, pin.pin_id,
        )
        .unwrap();
        assert_eq!(adapter.retained_pin_bytes.load(Ordering::Acquire), 0);
    }

    #[switchy_async::test]
    async fn unpinned_capture_lives_only_until_last_window_reader_drops() {
        let session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime.attached_clients.insert(client_id);
        let adapter = adapter_for_manager(manager_with_runtime(session_id, runtime));
        let captured = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_scrollback_pin(
            &adapter, session_id, client_id, pane_id,
        )
        .expect("capture");
        let reader = {
            let manager = adapter.inner.lock().expect("manager lock");
            manager.runtimes[&session_id].scrollback_pins[&(client_id, pane_id)].clone()
        };
        let weak_grid = Arc::downgrade(&reader.grid);
        let backing_reader = Arc::clone(&reader.grid);
        let bytes = backing_reader.reservation.bytes;
        assert_eq!(adapter.retained_pin_bytes.load(Ordering::Acquire), bytes);
        let before = reader.grid.grid.snapshot(0, 1);
        let ack = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_scrollback_unpin(
            &adapter,
            session_id,
            client_id,
            pane_id,
            captured.pin_id,
        )
        .expect("unpin");
        assert!(ack.released);
        assert_eq!(reader.grid.grid.snapshot(0, 1), before);
        assert!(weak_grid.upgrade().is_some());
        drop(reader);
        assert_eq!(adapter.retained_pin_bytes.load(Ordering::Acquire), bytes);
        drop(backing_reader);
        assert!(weak_grid.upgrade().is_none());
        assert_eq!(adapter.retained_pin_bytes.load(Ordering::Acquire), 0);
    }

    #[switchy_async::test]
    async fn frozen_scrollback_pin_releases_explicitly_and_on_detach() {
        let session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime.attached_clients.insert(client_id);
        let adapter = adapter_for_manager(manager_with_runtime(session_id, runtime));

        let first = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_scrollback_pin(
            &adapter, session_id, client_id, pane_id,
        )
        .expect("first pin should succeed");
        let ack = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_scrollback_unpin(
            &adapter,
            session_id,
            client_id,
            pane_id,
            first.pin_id,
        )
        .expect("unpin should succeed");
        assert!(ack.released);

        let second = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_scrollback_pin(
            &adapter, session_id, client_id, pane_id,
        )
        .expect("second pin should succeed");
        bmux_pane_runtime_state::SessionRuntimeManagerApi::end_attach(
            &adapter, session_id, client_id,
        );
        let manager = adapter
            .inner
            .lock()
            .expect("manager lock should be available");
        let runtime = manager
            .runtimes
            .get(&session_id)
            .expect("runtime should exist");
        assert!(!runtime.scrollback_pins.contains_key(&(client_id, pane_id)));
        assert!(second.pin_id > first.pin_id);
    }

    #[switchy_async::test]
    async fn exhausted_scrollback_pin_ids_preserve_existing_capture() {
        let session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime.attached_clients.insert(client_id);
        runtime.next_scrollback_pin_id = u64::MAX - 1;
        let adapter = adapter_for_manager(manager_with_runtime(session_id, runtime));
        let first = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_scrollback_pin(
            &adapter, session_id, client_id, pane_id,
        )
        .expect("last allocatable capture");
        assert_eq!(first.pin_id, u64::MAX - 1);
        for _ in 0..2 {
            assert!(matches!(
                bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_scrollback_pin(
                    &adapter, session_id, client_id, pane_id,
                ),
                Err(SessionRuntimeError::Closed)
            ));
        }
        let ack = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_scrollback_unpin(
            &adapter,
            session_id,
            client_id,
            pane_id,
            first.pin_id,
        )
        .expect("existing capture remains releasable");
        assert!(ack.released);
    }

    #[switchy_async::test]
    async fn replacing_scrollback_pin_rejects_stale_and_cross_client_release() {
        let session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let other_client_id = ClientId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime.attached_clients.insert(client_id);
        runtime.attached_clients.insert(other_client_id);
        let adapter = adapter_for_manager(manager_with_runtime(session_id, runtime));

        let first = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_scrollback_pin(
            &adapter, session_id, client_id, pane_id,
        )
        .expect("first capture");
        let replacement = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_scrollback_pin(
            &adapter, session_id, client_id, pane_id,
        )
        .expect("replacement capture");
        assert_ne!(first.pin_id, replacement.pin_id);

        for (caller, pin_id) in [
            (client_id, first.pin_id),
            (other_client_id, replacement.pin_id),
        ] {
            let ack = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_scrollback_unpin(
                &adapter, session_id, caller, pane_id, pin_id,
            )
            .expect("unpin acknowledgment");
            assert!(!ack.released);
        }
        let ack = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_scrollback_unpin(
            &adapter,
            session_id,
            client_id,
            pane_id,
            replacement.pin_id,
        )
        .expect("owner releases replacement");
        assert!(ack.released);
    }

    #[switchy_async::test]
    async fn frozen_scrollback_pins_are_removed_with_panes_and_sessions() {
        let session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let other_pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane_id, other_pane_id]);
        runtime.attached_clients.insert(client_id);
        let adapter = adapter_for_manager(manager_with_runtime(session_id, runtime));

        let pin = bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_scrollback_pin(
            &adapter, session_id, client_id, pane_id,
        )
        .expect("pin should succeed");
        bmux_pane_runtime_state::SessionRuntimeManagerApi::close_pane(
            &adapter,
            session_id,
            Some(PaneSelector::ById(pane_id)),
        )
        .expect("pane close should succeed");
        {
            let manager = adapter
                .inner
                .lock()
                .expect("manager lock should be available");
            let runtime = manager
                .runtimes
                .get(&session_id)
                .expect("runtime should remain");
            assert!(!runtime.scrollback_pins.contains_key(&(client_id, pane_id)));
        }

        let remaining_pin =
            bmux_pane_runtime_state::SessionRuntimeManagerApi::attach_scrollback_pin(
                &adapter,
                session_id,
                client_id,
                other_pane_id,
            )
            .expect("remaining pane pin should succeed");
        assert!(remaining_pin.pin_id > pin.pin_id);
        let removed =
            bmux_pane_runtime_state::SessionRuntimeManagerApi::remove_runtime(&adapter, session_id);
        assert!(removed.is_some());
        let manager = adapter
            .inner
            .lock()
            .expect("manager lock should be available");
        assert!(!manager.runtimes.contains_key(&session_id));
    }

    #[switchy_async::test]
    async fn alternate_resize_guard_is_atomic_and_preserves_last_writer_policy() {
        let pane = dummy_pane(crate::ids::new_uuid());
        let pty = bmux_pty::PtyPair::open(PtySize { rows: 1, cols: 1 }).unwrap();
        *pane.execution.master.lock().unwrap() = Some(pty.master);
        let original = pane.terminal_grid.lock().unwrap().grid().revision();
        let mut ticket = None;
        // Even a no-op size cannot authorize execution resizing in normal mode.
        assert!(
            pane.resize_pty_under_order(1, 1, None, None, &mut ticket, Some(original))
                .is_err()
        );
        assert_eq!(*pane.last_requested_size.lock().unwrap(), (1, 1));
        pane.terminal_grid.lock().unwrap().process(b"\x1b[?1049h");
        let observed = pane.terminal_grid.lock().unwrap().grid().revision();
        assert!(
            pane.resize_pty_under_order(3, 8, None, None, &mut ticket, Some(original))
                .is_err()
        );
        assert_eq!(*pane.last_requested_size.lock().unwrap(), (1, 1));
        pane.resize_pty_under_order(3, 8, None, None, &mut ticket, Some(observed))
            .unwrap();
        assert_eq!(*pane.last_requested_size.lock().unwrap(), (3, 8));
        // A later writer is allowed with a fresh source, without owner election.
        let next = pane.terminal_grid.lock().unwrap().grid().revision();
        pane.resize_pty_under_order(4, 9, None, None, &mut ticket, Some(next))
            .unwrap();
        assert_eq!(*pane.last_requested_size.lock().unwrap(), (4, 9));
        assert!(
            pane.resize_pty_under_order(2, 6, None, None, &mut ticket, Some(observed))
                .is_err()
        );
        assert_eq!(*pane.last_requested_size.lock().unwrap(), (4, 9));
    }

    #[switchy_async::test]
    async fn uncaptured_alternate_resize_acknowledges_master_without_queueing() {
        let mut pane = dummy_pane(crate::ids::new_uuid());
        let (input, mut queued) = mpsc::unbounded_channel();
        pane.input_tx = input;
        let pty = bmux_pty::PtyPair::open(PtySize { rows: 1, cols: 1 }).unwrap();
        *pane.execution.master.lock().unwrap() = Some(pty.master);
        pane.terminal_grid.lock().unwrap().process(b"\x1b[?1049h");
        let revision = pane.terminal_grid.lock().unwrap().grid().revision();
        let mut ticket = None;
        pane.resize_pty_under_order(7, 19, None, None, &mut ticket, Some(revision))
            .unwrap();
        let actual = pane
            .execution
            .master
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .size()
            .unwrap();
        assert_eq!((actual.rows, actual.cols), (7, 19));
        assert_eq!(*pane.last_requested_size.lock().unwrap(), (7, 19));
        assert!(ticket.is_none());
        assert!(matches!(
            queued.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        // A legacy request queued before this acknowledgement cannot restore
        // its superseded PTY dimensions. A current request remains permitted.
        assert!(
            !pane
                .execution
                .resize_if_current(&pane.last_requested_size, 1, 1)
                .unwrap()
        );
        assert!(
            pane.execution
                .resize_if_current(&pane.last_requested_size, 7, 19)
                .unwrap()
        );
        let actual = pane
            .execution
            .master
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .size()
            .unwrap();
        assert_eq!((actual.rows, actual.cols), (7, 19));
    }

    #[switchy_async::test]
    async fn uncaptured_alternate_resize_never_acknowledges_missing_master() {
        let pane = dummy_pane(crate::ids::new_uuid());
        pane.terminal_grid.lock().unwrap().process(b"\x1b[?1049h");
        let revision = pane.terminal_grid.lock().unwrap().grid().revision();
        let mut ticket = None;
        for (rows, cols) in [(1, 1), (7, 19)] {
            assert!(
                pane.resize_pty_under_order(rows, cols, None, None, &mut ticket, Some(revision))
                    .is_err()
            );
            assert_eq!(*pane.last_requested_size.lock().unwrap(), (1, 1));
            assert_eq!(
                pane.terminal_grid.lock().unwrap().grid().revision(),
                revision
            );
        }
    }

    #[switchy_async::test]
    async fn alternate_resize_commits_geometry_in_ordered_capture() {
        let pane = dummy_pane(crate::ids::new_uuid());
        let pty = bmux_pty::PtyPair::open(PtySize { rows: 1, cols: 1 }).unwrap();
        *pane.execution.master.lock().unwrap() = Some(pty.master);
        let root = tempfile::tempdir().unwrap();
        let capture = durable_capture::DurableCapture::create(root.path()).unwrap();
        let (writer, mut owner) = capture_writer::CaptureWriter::start(capture).unwrap();
        *pane.execution.capture_writer.lock().unwrap() = Some(writer.clone());
        let output = writer
            .reserve_output()
            .unwrap()
            .output(b"\x1b[?1049h".to_vec())
            .unwrap();
        pane.terminal_grid.lock().unwrap().process(b"\x1b[?1049h");
        writer.projected(output).unwrap();
        let revision = pane.terminal_grid.lock().unwrap().grid().revision();
        let reservation = writer.reserve_resize().unwrap();
        let order = pane.execution.capture_order.lock().unwrap();
        writer
            .wait_projected_raw(output.raw_end, Duration::from_secs(2))
            .unwrap();
        let mut ticket = None;
        pane.resize_pty_under_order(3, 8, None, Some(reservation), &mut ticket, Some(revision))
            .unwrap();
        drop(order);
        let ticket = ticket.expect("resize must publish an ordered event");
        writer.wait_committed(ticket).unwrap();
        assert!(ticket.sequence > output.sequence);
        assert_eq!(ticket.raw_end, output.raw_end);
        assert_eq!(writer.progress().unwrap().projected, ticket);
        assert_eq!(*pane.last_requested_size.lock().unwrap(), (3, 8));
        assert_eq!(pane.terminal_grid.lock().unwrap().grid().width(), 8);
        assert_eq!(
            pane.execution
                .master
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .size()
                .unwrap()
                .cols,
            8
        );
        let end = writer.reserve_end().unwrap().end(None).unwrap();
        writer.wait_committed(end).unwrap();
        owner.take().join().unwrap();
    }

    #[switchy_async::test]
    async fn rejected_alternate_resize_does_not_publish_reserved_capture_event() {
        let pane = dummy_pane(crate::ids::new_uuid());
        let root = tempfile::tempdir().unwrap();
        let capture = durable_capture::DurableCapture::create(root.path()).unwrap();
        let (writer, mut owner) = capture_writer::CaptureWriter::start(capture).unwrap();
        *pane.execution.capture_writer.lock().unwrap() = Some(writer.clone());
        let before = writer.progress().unwrap().admitted;
        let reservation = writer.reserve_resize().unwrap();
        let revision = pane.terminal_grid.lock().unwrap().grid().revision();
        let mut ticket = None;
        assert!(
            pane.resize_pty_under_order(3, 8, None, Some(reservation), &mut ticket, Some(revision))
                .is_err()
        );
        assert!(ticket.is_none());
        assert_eq!(writer.progress().unwrap().admitted, before);
        assert_eq!(*pane.last_requested_size.lock().unwrap(), (1, 1));
        let end = writer.reserve_end().unwrap().end(None).unwrap();
        writer.wait_committed(end).unwrap();
        owner.take().join().unwrap();
    }

    #[switchy_async::test]
    async fn resize_waits_for_output_geometry_guard() {
        let pane_id = crate::ids::new_uuid();
        let runtime = runtime_with_panes(&[pane_id]);
        let pane = runtime.panes.get(&pane_id).expect("pane should exist");
        pane.resize_pty(2, 10);
        let size = pane.last_requested_size.lock().unwrap();
        std::thread::scope(|scope| {
            let (started_tx, started_rx) = switchy_thread::sync::mpsc::channel();
            let (finished_tx, finished_rx) = switchy_thread::sync::mpsc::channel();
            scope.spawn(move || {
                started_tx.send(()).unwrap();
                pane.resize_pty(4, 20);
                finished_tx.send(()).unwrap();
            });
            started_rx.recv().unwrap();
            let blocked = finished_rx.recv_timeout(Duration::from_millis(20));
            let width_during_output = pane.terminal_grid.lock().unwrap().grid().width();
            drop(size);
            assert!(matches!(
                blocked,
                Err(switchy_thread::sync::mpsc::RecvTimeoutError::Timeout)
            ));
            assert_eq!(width_during_output, 10);
            finished_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        assert_eq!(*pane.last_requested_size.lock().unwrap(), (4, 20));
        let grid = pane.terminal_grid.lock().unwrap();
        assert_eq!(grid.grid().width(), 20);
        assert_eq!(grid.grid().height(), 4);
    }

    #[cfg(feature = "image-registry")]
    #[switchy_async::test]
    async fn resize_waits_for_image_capture_guard() {
        let pane_id = crate::ids::new_uuid();
        let runtime = runtime_with_panes(&[pane_id]);
        let pane = runtime.panes.get(&pane_id).expect("pane should exist");
        pane.resize_pty(2, 10);
        let images = pane.image_registry.lock().unwrap();
        std::thread::scope(|scope| {
            let (started_tx, started_rx) = switchy_thread::sync::mpsc::channel();
            let (finished_tx, finished_rx) = switchy_thread::sync::mpsc::channel();
            scope.spawn(move || {
                started_tx.send(()).unwrap();
                pane.resize_pty(4, 20);
                finished_tx.send(()).unwrap();
            });
            started_rx.recv().unwrap();
            let blocked = finished_rx.recv_timeout(Duration::from_millis(20));
            let width_during_capture = pane.terminal_grid.lock().unwrap().grid().width();
            drop(images);
            assert!(matches!(
                blocked,
                Err(switchy_thread::sync::mpsc::RecvTimeoutError::Timeout)
            ));
            assert_eq!(width_during_capture, 10);
            finished_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        assert_eq!(*pane.last_requested_size.lock().unwrap(), (4, 20));
    }

    #[switchy_async::test]
    async fn resize_preserves_partial_cursor_escape_state() {
        for prefix in [
            b"\x1b".as_slice(),
            b"\x1b[".as_slice(),
            b"\x1b[4;".as_slice(),
        ] {
            let pane_id = crate::ids::new_uuid();
            let runtime = runtime_with_panes(&[pane_id]);
            let pane = runtime.panes.get(&pane_id).expect("pane should exist");
            pane.resize_pty(2, 10);
            pane.terminal_grid.lock().unwrap().process(prefix);
            pane.resize_pty(5, 20);
            let suffix = &b"\x1b[4;15H"[prefix.len()..];
            let mut grid = pane.terminal_grid.lock().unwrap();
            grid.process(suffix);
            assert_eq!(grid_cursor_position(grid.grid()), (3, 14));
            assert_eq!(grid.grid().cursor().row, 3);
            assert_eq!(grid.grid().cursor().col, 14);
        }
    }

    #[cfg(feature = "image-registry")]
    #[switchy_async::test]
    async fn height_growth_preserves_retained_image_anchor() {
        let pane_id = crate::ids::new_uuid();
        let runtime = runtime_with_panes(&[pane_id]);
        let pane = runtime.panes.get(&pane_id).unwrap();
        pane.resize_pty(3, 80);
        let mut collector = image_collector();
        follow_image_chunk(
            &mut PaneReader::new(ProtocolProfile::Bmux, None),
            &mut pane.terminal_grid.lock().unwrap(),
            &mut TerminalProtocolEngine::new(ProtocolProfile::Bmux),
            &mut pane.image_registry.lock().unwrap(),
            &mut collector,
            b"\x1bPq\"1;1;1;6#1;2;100;0;0~\x1b\\\r\n\r\n\r\n",
            &mut Vec::new(),
        )
        .unwrap();
        let before = pane
            .image_registry
            .lock()
            .unwrap()
            .project_viewport(1, 3)
            .unwrap();
        pane.resize_pty(5, 80);
        let images = pane.image_registry.lock().unwrap();
        assert_eq!(images.project_viewport(1, 3).unwrap(), before);
        assert!(pane.image_dirty.load(Ordering::SeqCst));
        assert_eq!(pane.last_requested_size.lock().unwrap().0, 5);
    }

    #[cfg(feature = "image-registry")]
    #[switchy_async::test]
    async fn failed_image_height_growth_preserves_both_grids() {
        let pane_id = crate::ids::new_uuid();
        let runtime = runtime_with_panes(&[pane_id]);
        let pane = runtime.panes.get(&pane_id).unwrap();
        pane.resize_pty(3, 80);
        // A placement crossing the new bottom requires a decode to crop it.
        // An absent payload makes that projection fail deterministically.
        let mut images = pane.image_registry.lock().unwrap();
        images.handle_event(
            bmux_image::ImageEvent::SixelImage {
                data: Vec::new(),
                position: bmux_image::ImagePosition { row: 4, col: 0 },
                pixel_size: bmux_image::ImagePixelSize {
                    width: 8,
                    height: 64,
                },
                filtered_byte_offset: 0,
            },
            8,
            16,
        );
        let revision = images.sequence();
        let original = images.images().to_vec();
        drop(images);
        pane.resize_pty(5, 80);
        assert_eq!(*pane.last_requested_size.lock().unwrap(), (3, 80));
        assert_eq!(pane.terminal_grid.lock().unwrap().grid().height(), 3);
        assert_eq!(pane.last_requested_size.lock().unwrap().0, 3);
        let images = pane.image_registry.lock().unwrap();
        assert_eq!(images.sequence(), revision);
        assert_eq!(images.images(), original);
    }

    #[cfg(feature = "image-registry")]
    #[switchy_async::test]
    async fn height_shrink_moves_image_into_history() {
        let pane_id = crate::ids::new_uuid();
        let runtime = runtime_with_panes(&[pane_id]);
        let pane = runtime.panes.get(&pane_id).unwrap();
        pane.resize_pty(5, 80);
        let mut collector = image_collector();
        follow_image_chunk(
            &mut PaneReader::new(ProtocolProfile::Bmux, None),
            &mut pane.terminal_grid.lock().unwrap(),
            &mut TerminalProtocolEngine::new(ProtocolProfile::Bmux),
            &mut pane.image_registry.lock().unwrap(),
            &mut collector,
            b"\x1bPq\"1;1;1;6#1;2;100;0;0~\x1b\\a\r\nb\r\nc\r\nd\r\ne",
            &mut Vec::new(),
        )
        .unwrap();
        let original = pane.image_registry.lock().unwrap().images()[0].clone();
        pane.resize_pty(3, 80);
        let images = pane.image_registry.lock().unwrap();
        assert_eq!(images.images(), []);
        assert_eq!(images.project_viewport(2, 3).unwrap(), vec![original]);
    }

    #[cfg(feature = "image-registry")]
    #[switchy_async::test]
    async fn height_shrink_evicts_images_with_text_history() {
        let pane_id = crate::ids::new_uuid();
        let runtime = runtime_with_panes(&[pane_id]);
        let pane = runtime.panes.get(&pane_id).unwrap();
        pane.resize_pty(5, 80);
        *pane.terminal_grid.lock().unwrap() =
            TerminalGridStream::new(80, 5, GridLimits { scrollback_rows: 0 }).unwrap();
        let mut collector = image_collector();
        follow_image_chunk(
            &mut PaneReader::new(ProtocolProfile::Bmux, None),
            &mut pane.terminal_grid.lock().unwrap(),
            &mut TerminalProtocolEngine::new(ProtocolProfile::Bmux),
            &mut pane.image_registry.lock().unwrap(),
            &mut collector,
            b"\x1bPq\"1;1;1;6#1;2;100;0;0~\x1b\\a\r\nb\r\nc\r\nd\r\ne",
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(pane.image_registry.lock().unwrap().images().len(), 1);
        pane.resize_pty(3, 80);
        assert_eq!(
            pane.terminal_grid
                .lock()
                .unwrap()
                .grid()
                .scrollback_rows_hint(),
            0
        );
        assert_eq!(
            pane.image_registry
                .lock()
                .unwrap()
                .project_viewport(2, 3)
                .unwrap(),
            [] as [bmux_image::PaneImage; 0]
        );
    }

    #[cfg(feature = "image-registry")]
    #[switchy_async::test]
    async fn width_resize_restores_image_from_pending_wrapped_line() {
        let pane_id = crate::ids::new_uuid();
        let runtime = runtime_with_panes(&[pane_id]);
        let pane = runtime.panes.get(&pane_id).unwrap();
        pane.resize_pty(2, 4);
        let mut collector = image_collector();
        follow_image_chunk(
            &mut PaneReader::new(ProtocolProfile::Bmux, None),
            &mut pane.terminal_grid.lock().unwrap(),
            &mut TerminalProtocolEngine::new(ProtocolProfile::Bmux),
            &mut pane.image_registry.lock().unwrap(),
            &mut collector,
            b"\x1bPq\"1;1;1;6#1;2;100;0;0~\x1b\\abcdefghijkl",
            &mut Vec::new(),
        )
        .unwrap();
        let original = pane
            .image_registry
            .lock()
            .unwrap()
            .project_viewport(1, 2)
            .unwrap();
        assert_eq!(original.len(), 1);
        pane.resize_pty(2, 8);
        assert_eq!(*pane.last_requested_size.lock().unwrap(), (2, 8));
        assert_eq!(pane.image_registry.lock().unwrap().images(), original);
        pane.resize_pty(2, 4);
        let images = pane.image_registry.lock().unwrap();
        assert_eq!(images.images(), []);
        assert_eq!(images.project_viewport(1, 2).unwrap(), original);
    }

    #[cfg(feature = "image-registry")]
    #[switchy_async::test]
    async fn width_resize_remaps_hidden_history_without_exposing_images() {
        let pane_id = crate::ids::new_uuid();
        let runtime = runtime_with_panes(&[pane_id]);
        let pane = runtime.panes.get(&pane_id).unwrap();
        pane.resize_pty(2, 4);
        let mut collector = image_collector();
        let mut protocol = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
        follow_image_chunk(
            &mut PaneReader::new(ProtocolProfile::Bmux, None),
            &mut pane.terminal_grid.lock().unwrap(),
            &mut protocol,
            &mut pane.image_registry.lock().unwrap(),
            &mut collector,
            b"\x1bPq\"1;1;1;6#1;2;100;0;0~\x1b\\abcdefghijkl\x1b[?1049h",
            &mut Vec::new(),
        )
        .unwrap();
        pane.resize_pty(2, 8);
        assert_eq!(*pane.last_requested_size.lock().unwrap(), (2, 8));
        assert_eq!(pane.image_registry.lock().unwrap().images(), []);
        follow_image_chunk(
            &mut PaneReader::new(ProtocolProfile::Bmux, None),
            &mut pane.terminal_grid.lock().unwrap(),
            &mut protocol,
            &mut pane.image_registry.lock().unwrap(),
            &mut collector,
            b"\x1b[?1049l",
            &mut Vec::new(),
        )
        .unwrap();
        let images = pane.image_registry.lock().unwrap();
        assert_eq!(images.images().len(), 1);
        assert_eq!(images.images()[0].position.row, 0);
        assert_eq!(images.images()[0].position.col, 0);
    }

    #[cfg(feature = "image-registry")]
    #[switchy_async::test]
    async fn height_resize_restores_image_from_pending_wrapped_line() {
        let pane_id = crate::ids::new_uuid();
        let runtime = runtime_with_panes(&[pane_id]);
        let pane = runtime.panes.get(&pane_id).unwrap();
        pane.resize_pty(2, 5);
        let mut collector = image_collector();
        follow_image_chunk(
            &mut PaneReader::new(ProtocolProfile::Bmux, None),
            &mut pane.terminal_grid.lock().unwrap(),
            &mut TerminalProtocolEngine::new(ProtocolProfile::Bmux),
            &mut pane.image_registry.lock().unwrap(),
            &mut collector,
            b"\x1bPq\"1;1;1;6#1;2;100;0;0~\x1b\\abcdefghijklmnop",
            &mut Vec::new(),
        )
        .unwrap();
        let original = pane
            .image_registry
            .lock()
            .unwrap()
            .project_viewport(2, 2)
            .unwrap();
        assert_eq!(original.len(), 1);
        assert_eq!(pane.image_registry.lock().unwrap().images(), []);
        pane.resize_pty(4, 5);
        assert_eq!(pane.image_registry.lock().unwrap().images(), original);
        pane.resize_pty(2, 5);
        let images = pane.image_registry.lock().unwrap();
        assert_eq!(images.images(), []);
        assert_eq!(images.project_viewport(2, 2).unwrap(), original);
    }

    #[cfg(feature = "image-registry")]
    #[switchy_async::test]
    async fn text_only_resize_does_not_require_image_capture_budget() {
        let pane_id = crate::ids::new_uuid();
        let runtime = runtime_with_panes(&[pane_id]);
        let pane = runtime.panes.get(&pane_id).unwrap();
        pane.resize_pty(3, 80);
        let text =
            b"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\r\n"
                .repeat(8000);
        {
            let mut grid = pane.terminal_grid.lock().unwrap();
            grid.process(&text);
            let mut budget = RESPONSE_OUTPUT_BUDGET;
            assert!(grid.grid().try_clone_charged(&mut budget).is_none());
        }
        pane.resize_pty(5, 80);
        assert_eq!(*pane.last_requested_size.lock().unwrap(), (5, 80));
        assert_eq!(pane.last_requested_size.lock().unwrap().0, 5);
        assert_eq!(pane.terminal_grid.lock().unwrap().grid().height(), 5);
    }

    #[cfg(feature = "image-registry")]
    #[switchy_async::test]
    async fn height_resize_remaps_hidden_main_images_only() {
        let pane_id = crate::ids::new_uuid();
        let runtime = runtime_with_panes(&[pane_id]);
        let pane = runtime.panes.get(&pane_id).unwrap();
        pane.resize_pty(2, 5);
        let mut collector = image_collector();
        let mut protocol = TerminalProtocolEngine::new(ProtocolProfile::Bmux);
        let image = b"\x1bPq\"1;1;1;6#1;2;100;0;0~\x1b\\";
        let mut bytes = image.to_vec();
        bytes.extend_from_slice(b"abcdefghijklmnop\x1b[?1049h");
        bytes.extend_from_slice(image);
        follow_image_chunk(
            &mut PaneReader::new(ProtocolProfile::Bmux, None),
            &mut pane.terminal_grid.lock().unwrap(),
            &mut protocol,
            &mut pane.image_registry.lock().unwrap(),
            &mut collector,
            &bytes,
            &mut Vec::new(),
        )
        .unwrap();
        let alternate = pane.image_registry.lock().unwrap().images().to_vec();
        assert_eq!(alternate.len(), 1);
        pane.resize_pty(4, 5);
        assert_eq!(pane.image_registry.lock().unwrap().images(), alternate);
        follow_image_chunk(
            &mut PaneReader::new(ProtocolProfile::Bmux, None),
            &mut pane.terminal_grid.lock().unwrap(),
            &mut protocol,
            &mut pane.image_registry.lock().unwrap(),
            &mut collector,
            b"\x1b[?1049l",
            &mut Vec::new(),
        )
        .unwrap();
        let images = pane.image_registry.lock().unwrap();
        assert_eq!(images.images().len(), 1);
        assert_eq!(images.images()[0].id, 1);
        assert_eq!(images.images()[0].position.row, 0);
        assert_ne!(images.images()[0].id, alternate[0].id);
    }

    #[cfg(feature = "image-registry")]
    #[switchy_async::test]
    async fn alternate_only_images_reproject_on_height_resize() {
        let pane_id = crate::ids::new_uuid();
        let runtime = runtime_with_panes(&[pane_id]);
        let pane = runtime.panes.get(&pane_id).unwrap();
        pane.resize_pty(5, 80);
        let mut collector = image_collector();
        follow_image_chunk(
            &mut PaneReader::new(ProtocolProfile::Bmux, None),
            &mut pane.terminal_grid.lock().unwrap(),
            &mut TerminalProtocolEngine::new(ProtocolProfile::Bmux),
            &mut pane.image_registry.lock().unwrap(),
            &mut collector,
            b"\x1b[?1049h\x1b[5;1H\x1bPq\"1;1;1;6#1;2;100;0;0~\x1b\\",
            &mut Vec::new(),
        )
        .unwrap();
        let original = pane.image_registry.lock().unwrap().images().to_vec();
        assert_eq!(original.len(), 1);
        assert!(!pane.image_registry.lock().unwrap().has_main_images());
        pane.resize_pty(3, 80);
        assert_eq!(pane.image_registry.lock().unwrap().images(), []);
        pane.resize_pty(5, 80);
        assert_eq!(pane.image_registry.lock().unwrap().images(), original);
    }

    #[cfg(feature = "image-registry")]
    #[switchy_async::test]
    async fn blank_cell_width_resize_tracks_cursor_without_creating_text() {
        let pane_id = crate::ids::new_uuid();
        let runtime = runtime_with_panes(&[pane_id]);
        let pane = runtime.panes.get(&pane_id).unwrap();
        pane.resize_pty(2, 8);
        let mut collector = image_collector();
        // Placement in trailing blank cells has no retained text anchor.
        follow_image_chunk(
            &mut PaneReader::new(ProtocolProfile::Bmux, None),
            &mut pane.terminal_grid.lock().unwrap(),
            &mut TerminalProtocolEngine::new(ProtocolProfile::Bmux),
            &mut pane.image_registry.lock().unwrap(),
            &mut collector,
            b"\x1b[1;7H\x1bPq\"1;1;1;6#1;2;100;0;0~\x1b\\",
            &mut Vec::new(),
        )
        .unwrap();
        let id = pane.image_registry.lock().unwrap().images()[0].id;
        pane.resize_pty(2, 4);
        assert_eq!(*pane.last_requested_size.lock().unwrap(), (2, 4));
        let cursor = pane.terminal_grid.lock().unwrap().grid().cursor();
        let images = pane.image_registry.lock().unwrap();
        assert_eq!(images.images().len(), 1);
        assert_eq!(images.images()[0].id, id);
        assert_eq!(usize::from(images.images()[0].position.row), cursor.row);
        assert_eq!(usize::from(images.images()[0].position.col), cursor.col);
    }

    #[switchy_async::test]
    async fn rejected_resize_preserves_published_geometry() {
        let pane_id = crate::ids::new_uuid();
        let runtime = runtime_with_panes(&[pane_id]);
        let pane = runtime.panes.get(&pane_id).expect("pane should exist");
        pane.resize_pty(2, 10);
        let revision = pane.terminal_grid.lock().unwrap().grid().revision();

        for (rows, cols) in [(0, 10), (2, 0), (0, 0)] {
            pane.resize_pty(rows, cols);
            assert_eq!(*pane.last_requested_size.lock().unwrap(), (2, 10));
            let grid = pane.terminal_grid.lock().unwrap();
            assert_eq!(grid.grid().width(), 10);
            assert_eq!(grid.grid().height(), 2);
            assert_eq!(grid.grid().revision(), revision);
        }
    }

    #[switchy_async::test]
    async fn attach_scene_projects_client_and_server_global_floating_panes() {
        let attach_session_id = SessionId(crate::ids::new_uuid());
        let other_session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let attach_pane_id = crate::ids::new_uuid();
        let client_global_pane_id = crate::ids::new_uuid();
        let server_global_pane_id = crate::ids::new_uuid();
        let other_client_pane_id = crate::ids::new_uuid();

        let mut attach_runtime = runtime_with_panes(&[attach_pane_id]);
        attach_runtime.attached_clients.insert(client_id);
        let mut other_runtime = runtime_with_panes(&[
            client_global_pane_id,
            server_global_pane_id,
            other_client_pane_id,
        ]);
        let mut client_global =
            floating_surface(client_global_pane_id, FloatingPaneScope::ClientGlobal);
        client_global.client_id = Some(client_id);
        let server_global =
            floating_surface(server_global_pane_id, FloatingPaneScope::ServerGlobal);
        let mut other_client =
            floating_surface(other_client_pane_id, FloatingPaneScope::ClientGlobal);
        other_client.client_id = Some(ClientId(crate::ids::new_uuid()));
        other_runtime.floating_surfaces = vec![client_global, server_global, other_client];

        let (pane_exit_tx, _pane_exit_rx) = mpsc::unbounded_channel();
        let manager = SessionRuntimeManager {
            host: crate::runtime::PaneRuntimeHost::detached(),
            padding_config: PanePaddingConfig::default(),
            restored_padding_overrides: BTreeMap::new(),
            padding_previews: BTreeMap::new(),
            pending_layout_revisions: BTreeMap::new(),
            runtimes: BTreeMap::from([
                (attach_session_id, attach_runtime),
                (other_session_id, other_runtime),
            ]),
            pane_input_index: Arc::new(RwLock::new(BTreeMap::new())),
            client_write_permissions: Arc::new(RwLock::new(BTreeMap::new())),
            retained_output_bytes: Arc::new(switchy_thread::sync::atomic::AtomicUsize::new(0)),
            retained_pin_bytes: Arc::new(AtomicUsize::new(0)),
            shell: "sh".to_string(),
            pane_term: "xterm-256color".to_string(),
            protocol_profile: ProtocolProfile::Bmux,
            bracketed_paste_supported: true,
            shell_integration_root: None,
            pane_exit_tx,
        };

        let scene = manager
            .build_attach_scene_for_client(attach_session_id, client_id)
            .expect("scene should build");
        let visible_panes = scene
            .surfaces
            .iter()
            .filter(|surface| surface.visible)
            .filter_map(|surface| surface.pane_id)
            .collect::<BTreeSet<_>>();
        assert!(visible_panes.contains(&attach_pane_id));
        assert!(visible_panes.contains(&client_global_pane_id));
        assert!(visible_panes.contains(&server_global_pane_id));
        assert!(!visible_panes.contains(&other_client_pane_id));
    }

    #[switchy_async::test]
    async fn per_window_and_per_pane_scopes_stay_in_owner_session() {
        let attach_session_id = SessionId(crate::ids::new_uuid());
        let other_session_id = SessionId(crate::ids::new_uuid());
        let client_id = ClientId(crate::ids::new_uuid());
        let attach_pane_id = crate::ids::new_uuid();
        let other_pane_id = crate::ids::new_uuid();

        let mut attach_runtime = runtime_with_panes(&[attach_pane_id]);
        attach_runtime.attached_clients.insert(client_id);
        let mut other_runtime = runtime_with_panes(&[other_pane_id]);
        let mut per_window = floating_surface(other_pane_id, FloatingPaneScope::PerWindow);
        per_window.context_id = Some(crate::ids::new_uuid());
        let mut per_pane = floating_surface(other_pane_id, FloatingPaneScope::PerPane);
        per_pane.anchor_pane_id = Some(other_pane_id);
        other_runtime.floating_surfaces = vec![per_window, per_pane];

        let (pane_exit_tx, _pane_exit_rx) = mpsc::unbounded_channel();
        let manager = SessionRuntimeManager {
            host: crate::runtime::PaneRuntimeHost::detached(),
            padding_config: PanePaddingConfig::default(),
            restored_padding_overrides: BTreeMap::new(),
            padding_previews: BTreeMap::new(),
            pending_layout_revisions: BTreeMap::new(),
            runtimes: BTreeMap::from([
                (attach_session_id, attach_runtime),
                (other_session_id, other_runtime),
            ]),
            pane_input_index: Arc::new(RwLock::new(BTreeMap::new())),
            client_write_permissions: Arc::new(RwLock::new(BTreeMap::new())),
            retained_output_bytes: Arc::new(switchy_thread::sync::atomic::AtomicUsize::new(0)),
            retained_pin_bytes: Arc::new(AtomicUsize::new(0)),
            shell: "sh".to_string(),
            pane_term: "xterm-256color".to_string(),
            protocol_profile: ProtocolProfile::Bmux,
            bracketed_paste_supported: true,
            shell_integration_root: None,
            pane_exit_tx,
        };

        let scene = manager
            .build_attach_scene_for_client(attach_session_id, client_id)
            .expect("scene should build");
        assert!(
            scene
                .surfaces
                .iter()
                .all(|surface| surface.pane_id != Some(other_pane_id))
        );
    }

    #[switchy_async::test]
    async fn closing_last_tiled_pane_leaves_unanchored_floating_pane_visible() {
        let session_id = SessionId(crate::ids::new_uuid());
        let tiled_pane_id = crate::ids::new_uuid();
        let floating_pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[tiled_pane_id, floating_pane_id]);
        runtime.layout_root = Some(PaneLayoutNode::Leaf {
            pane_id: tiled_pane_id,
        });
        runtime.floating_surfaces = vec![floating_surface(
            floating_pane_id,
            FloatingPaneScope::PerSession,
        )];
        let (pane_exit_tx, _pane_exit_rx) = mpsc::unbounded_channel();
        let mut manager = SessionRuntimeManager {
            host: crate::runtime::PaneRuntimeHost::detached(),
            padding_config: PanePaddingConfig::default(),
            restored_padding_overrides: BTreeMap::new(),
            padding_previews: BTreeMap::new(),
            pending_layout_revisions: BTreeMap::new(),
            runtimes: BTreeMap::from([(session_id, runtime)]),
            pane_input_index: Arc::new(RwLock::new(BTreeMap::new())),
            client_write_permissions: Arc::new(RwLock::new(BTreeMap::new())),
            retained_output_bytes: Arc::new(switchy_thread::sync::atomic::AtomicUsize::new(0)),
            retained_pin_bytes: Arc::new(AtomicUsize::new(0)),
            shell: "sh".to_string(),
            pane_term: "xterm-256color".to_string(),
            protocol_profile: ProtocolProfile::Bmux,
            bracketed_paste_supported: true,
            shell_integration_root: None,
            pane_exit_tx,
        };

        let (_closed_pane_id, removed) = manager
            .close_pane(session_id, Some(PaneSelector::ById(tiled_pane_id)))
            .expect("last tiled pane should close");

        assert!(removed.is_none());
        let runtime = manager.runtimes.get(&session_id).expect("runtime remains");
        assert!(runtime.layout_root.is_none());
        assert!(runtime.panes.contains_key(&floating_pane_id));
        assert_eq!(runtime.focused_pane_id, floating_pane_id);
        assert_eq!(runtime.floating_surfaces.len(), 1);
    }

    #[switchy_async::test]
    async fn exiting_focused_floating_pane_restores_live_focus() {
        let session_id = SessionId(crate::ids::new_uuid());
        let tiled_pane_id = crate::ids::new_uuid();
        let floating_pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[tiled_pane_id, floating_pane_id]);
        runtime.layout_root = Some(PaneLayoutNode::Leaf {
            pane_id: tiled_pane_id,
        });
        runtime.focused_pane_id = floating_pane_id;
        let mut surface = floating_surface(floating_pane_id, FloatingPaneScope::PerSession);
        surface.cursor_owner = true;
        runtime.floating_surfaces = vec![surface];
        runtime
            .panes
            .get(&floating_pane_id)
            .expect("floating pane exists")
            .exited
            .store(true, Ordering::SeqCst);
        let (pane_exit_tx, _pane_exit_rx) = mpsc::unbounded_channel();
        let mut manager = SessionRuntimeManager {
            host: crate::runtime::PaneRuntimeHost::detached(),
            padding_config: PanePaddingConfig::default(),
            restored_padding_overrides: BTreeMap::new(),
            padding_previews: BTreeMap::new(),
            pending_layout_revisions: BTreeMap::new(),
            runtimes: BTreeMap::from([(session_id, runtime)]),
            pane_input_index: Arc::new(RwLock::new(BTreeMap::new())),
            client_write_permissions: Arc::new(RwLock::new(BTreeMap::new())),
            retained_output_bytes: Arc::new(switchy_thread::sync::atomic::AtomicUsize::new(0)),
            retained_pin_bytes: Arc::new(AtomicUsize::new(0)),
            shell: "sh".to_string(),
            pane_term: "xterm-256color".to_string(),
            protocol_profile: ProtocolProfile::Bmux,
            bracketed_paste_supported: true,
            shell_integration_root: None,
            pane_exit_tx,
        };

        let publish_all = manager.reconcile_exited_pane_focus(session_id, floating_pane_id);

        assert!(!publish_all);
        let runtime = manager.runtimes.get(&session_id).expect("runtime remains");
        assert_eq!(runtime.focused_pane_id, tiled_pane_id);
        assert!(
            runtime
                .floating_surfaces
                .iter()
                .all(|surface| !surface.cursor_owner)
        );
    }

    #[switchy_async::test]
    async fn closing_final_floating_pane_requires_explicit_session_action() {
        let session_id = SessionId(crate::ids::new_uuid());
        let floating_pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[floating_pane_id]);
        runtime.layout_root = None;
        runtime.floating_surfaces = vec![floating_surface(
            floating_pane_id,
            FloatingPaneScope::PerSession,
        )];
        let (pane_exit_tx, _pane_exit_rx) = mpsc::unbounded_channel();
        let mut manager = SessionRuntimeManager {
            host: crate::runtime::PaneRuntimeHost::detached(),
            padding_config: PanePaddingConfig::default(),
            restored_padding_overrides: BTreeMap::new(),
            padding_previews: BTreeMap::new(),
            pending_layout_revisions: BTreeMap::new(),
            runtimes: BTreeMap::from([(session_id, runtime)]),
            pane_input_index: Arc::new(RwLock::new(BTreeMap::new())),
            client_write_permissions: Arc::new(RwLock::new(BTreeMap::new())),
            retained_output_bytes: Arc::new(switchy_thread::sync::atomic::AtomicUsize::new(0)),
            retained_pin_bytes: Arc::new(AtomicUsize::new(0)),
            shell: "sh".to_string(),
            pane_term: "xterm-256color".to_string(),
            protocol_profile: ProtocolProfile::Bmux,
            bracketed_paste_supported: true,
            shell_integration_root: None,
            pane_exit_tx,
        };

        let result = manager.close_floating_pane(session_id, floating_pane_id);

        assert!(result.is_err());
        assert!(manager.runtimes.contains_key(&session_id));
    }

    /// A pane's grid's modes, as input routing sees them.
    fn pane_modes(bytes: &[u8]) -> (AttachMouseProtocolState, AttachInputModeState) {
        let mut grid = TerminalGridStream::new(80, 24, GridLimits::default()).unwrap();
        grid.process(bytes);
        let modes = grid.grid().protocol_state();
        (attach_mouse_protocol(modes), attach_input_modes(modes))
    }

    #[test]
    fn pane_mouse_protocol_follows_dec_private_modes() {
        let protocol = |bytes: &[u8]| pane_modes(bytes).0;
        assert_eq!(protocol(b""), AttachMouseProtocolState::default());
        assert_eq!(
            protocol(b"\x1b[?1000h\x1b[?1006h"),
            AttachMouseProtocolState {
                mode: AttachMouseProtocolMode::PressRelease,
                encoding: AttachMouseProtocolEncoding::Sgr,
            }
        );
        assert_eq!(
            protocol(b"\x1b[?1000h\x1b[?1003h").mode,
            AttachMouseProtocolMode::AnyMotion
        );
        assert_eq!(
            protocol(b"\x1b[?1000h\x1b[?1003h\x1b[?1003l").mode,
            AttachMouseProtocolMode::PressRelease
        );
        // UTF-8 encoding counts only without SGR; urxvt alone leaves the default encoding.
        assert_eq!(
            protocol(b"\x1b[?1005h").encoding,
            AttachMouseProtocolEncoding::Utf8
        );
        assert_eq!(
            protocol(b"\x1b[?1005h\x1b[?1006h").encoding,
            AttachMouseProtocolEncoding::Sgr
        );
        assert_eq!(
            protocol(b"\x1b[?1015h").encoding,
            AttachMouseProtocolEncoding::Default
        );
        assert_eq!(
            protocol(b"\x1b[?1000h\x1b[?1006h\x1b[?1000l\x1b[?1006l"),
            AttachMouseProtocolState::default()
        );
    }

    #[test]
    fn pane_input_modes_follow_dec_modes_and_resets() {
        let modes = |bytes: &[u8]| pane_modes(bytes).1;
        assert_eq!(modes(b""), AttachInputModeState::default());
        let enabled = AttachInputModeState {
            application_cursor: true,
            application_keypad: true,
            bracketed_paste: true,
        };
        assert_eq!(modes(b"\x1b[?1;2004h\x1b="), enabled);
        assert_eq!(
            modes(b"\x1b[?1;2004h\x1b=\x1b[?1;2004l\x1b>"),
            AttachInputModeState::default()
        );
        // A full reset and a soft reset (DECSTR) both return every mode to its default.
        for reset in [&b"\x1bc"[..], b"\x1b[!p"] {
            let mut bytes = b"\x1b[?1;2004h\x1b=\x1b[?1003h".to_vec();
            bytes.extend_from_slice(reset);
            assert_eq!(pane_modes(&bytes), Default::default(), "{reset:?}");
        }
    }

    #[test]
    fn protocol_reply_tracks_cursor_position_for_cpr_queries() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Xterm);
        let mut cursor_tracker = PaneCursorTracker::new(24, 80);

        let _ = protocol_reply_for_chunk(
            &mut engine,
            &mut cursor_tracker.reader,
            &mut cursor_tracker.terminal_grid,
            b"\x1b[12;34H",
        );

        let cpr_reply = protocol_reply_for_chunk(
            &mut engine,
            &mut cursor_tracker.reader,
            &mut cursor_tracker.terminal_grid,
            b"\x1b[6n",
        );
        assert_eq!(cpr_reply, b"\x1b[12;34R");

        let dec_cpr_reply = protocol_reply_for_chunk(
            &mut engine,
            &mut cursor_tracker.reader,
            &mut cursor_tracker.terminal_grid,
            b"\x1b[?6n",
        );
        assert_eq!(dec_cpr_reply, b"\x1b[?12;34R");
    }

    #[test]
    fn protocol_reply_reports_saved_cursor_after_alt_screen_exit() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Xterm);
        let mut cursor_tracker = PaneCursorTracker::new(24, 80);

        let _ = protocol_reply_for_chunk(
            &mut engine,
            &mut cursor_tracker.reader,
            &mut cursor_tracker.terminal_grid,
            b"\x1b[12;34H",
        );
        assert_eq!(
            protocol_reply_for_chunk(
                &mut engine,
                &mut cursor_tracker.reader,
                &mut cursor_tracker.terminal_grid,
                b"\x1b[6n"
            ),
            b"\x1b[12;34R"
        );

        let _ = protocol_reply_for_chunk(
            &mut engine,
            &mut cursor_tracker.reader,
            &mut cursor_tracker.terminal_grid,
            b"\x1b[?1049h",
        );
        let _ = protocol_reply_for_chunk(
            &mut engine,
            &mut cursor_tracker.reader,
            &mut cursor_tracker.terminal_grid,
            b"\x1b[4;7H",
        );
        assert_eq!(
            protocol_reply_for_chunk(
                &mut engine,
                &mut cursor_tracker.reader,
                &mut cursor_tracker.terminal_grid,
                b"\x1b[6n"
            ),
            b"\x1b[4;7R"
        );

        let _ = protocol_reply_for_chunk(
            &mut engine,
            &mut cursor_tracker.reader,
            &mut cursor_tracker.terminal_grid,
            b"\x1b[?1049l",
        );
        assert_eq!(
            protocol_reply_for_chunk(
                &mut engine,
                &mut cursor_tracker.reader,
                &mut cursor_tracker.terminal_grid,
                b"\x1b[6n"
            ),
            b"\x1b[12;34R"
        );
    }

    #[test]
    fn protocol_reply_does_not_confuse_kitty_query_u_with_restore_cursor() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Xterm);
        let mut cursor_tracker = PaneCursorTracker::new(30, 120);

        let _ = protocol_reply_for_chunk(
            &mut engine,
            &mut cursor_tracker.reader,
            &mut cursor_tracker.terminal_grid,
            b"\x1b[9;17H",
        );
        let _ = protocol_reply_for_chunk(
            &mut engine,
            &mut cursor_tracker.reader,
            &mut cursor_tracker.terminal_grid,
            b"\x1b[s",
        );
        let _ = protocol_reply_for_chunk(
            &mut engine,
            &mut cursor_tracker.reader,
            &mut cursor_tracker.terminal_grid,
            b"\x1b[?u",
        );
        let _ = protocol_reply_for_chunk(
            &mut engine,
            &mut cursor_tracker.reader,
            &mut cursor_tracker.terminal_grid,
            b"\x1b[H",
        );
        let _ = protocol_reply_for_chunk(
            &mut engine,
            &mut cursor_tracker.reader,
            &mut cursor_tracker.terminal_grid,
            b"\x1b[u",
        );

        assert_eq!(
            protocol_reply_for_chunk(
                &mut engine,
                &mut cursor_tracker.reader,
                &mut cursor_tracker.terminal_grid,
                b"\x1b[6n"
            ),
            b"\x1b[9;17R"
        );
    }

    #[test]
    fn cursor_tracker_resize_updates_cursor_bounds() {
        let mut engine = TerminalProtocolEngine::new(ProtocolProfile::Xterm);
        let mut cursor_tracker = PaneCursorTracker::new(5, 5);

        let _ = protocol_reply_for_chunk(
            &mut engine,
            &mut cursor_tracker.reader,
            &mut cursor_tracker.terminal_grid,
            b"\x1b[24;80H",
        );
        let clamped_reply = protocol_reply_for_chunk(
            &mut engine,
            &mut cursor_tracker.reader,
            &mut cursor_tracker.terminal_grid,
            b"\x1b[6n",
        );
        assert_eq!(clamped_reply, b"\x1b[5;5R");

        cursor_tracker.resize(40, 120);
        let _ = protocol_reply_for_chunk(
            &mut engine,
            &mut cursor_tracker.reader,
            &mut cursor_tracker.terminal_grid,
            b"\x1b[24;80H",
        );
        let resized_reply = protocol_reply_for_chunk(
            &mut engine,
            &mut cursor_tracker.reader,
            &mut cursor_tracker.terminal_grid,
            b"\x1b[6n",
        );
        assert_eq!(resized_reply, b"\x1b[24;80R");
    }

    #[test]
    fn esc_seq_phase_csi_sgr_round_trip() {
        let seq = b"\x1b[38;2;10;10;10m";
        let mut phase = EscSeqPhase::Ground;

        phase = phase.advance(seq[0]);
        assert_eq!(phase, EscSeqPhase::Escape);

        phase = phase.advance(seq[1]);
        assert_eq!(phase, EscSeqPhase::Csi);

        for &byte in &seq[2..seq.len() - 1] {
            phase = phase.advance(byte);
            assert_eq!(phase, EscSeqPhase::Csi);
        }

        phase = phase.advance(seq[seq.len() - 1]);
        assert_eq!(phase, EscSeqPhase::Ground);
    }

    #[test]
    fn esc_seq_phase_can_aborts_from_any_state() {
        for initial in [
            EscSeqPhase::Escape,
            EscSeqPhase::Csi,
            EscSeqPhase::Osc,
            EscSeqPhase::Dcs,
            EscSeqPhase::Sos,
        ] {
            assert_eq!(initial.advance(0x18), EscSeqPhase::Ground);
            assert_eq!(initial.advance(0x1A), EscSeqPhase::Ground);
        }
    }

    #[cfg(unix)]
    #[test]
    fn parse_ps_process_entry_handles_command_with_spaces() {
        let entry = parse_ps_process_entry(" 101  99 Ss opencode --model gpt-5")
            .expect("ps line should parse");
        assert_eq!(entry.pid, 101);
        assert_eq!(entry.pgid, 99);
        assert_eq!(entry.state, "Ss");
        assert_eq!(entry.command, "opencode --model gpt-5");
    }

    #[cfg(unix)]
    #[test]
    fn inspection_fallback_prefers_non_shell_process_for_command() {
        let entries = vec![
            PsProcessEntry {
                pid: 120,
                pgid: 88,
                state: "Ss".to_string(),
                command: "nu".to_string(),
            },
            PsProcessEntry {
                pid: 121,
                pgid: 88,
                state: "R+".to_string(),
                command: "opencode run".to_string(),
            },
        ];

        let inspection =
            inspect_process_group_entries_with_resolver(&entries, 88, Some(120), "nu", |pid| {
                match pid {
                    120 => Some("/home/user".to_string()),
                    121 => Some("/work/project".to_string()),
                    _ => None,
                }
            })
            .expect("inspection should include active command");

        assert_eq!(inspection.command.as_deref(), Some("opencode run"));
        assert_eq!(inspection.cwd.as_deref(), Some("/work/project"));
    }

    // Runs fish and sh from PATH.
    #[cfg(unix)]
    #[test]
    fn fish_integration_restores_config_home_for_config_and_children() {
        let directory = tempfile::tempdir().unwrap();
        let fish_dir = directory.path().join("fish");
        std::fs::create_dir(&fish_dir).unwrap();
        std::fs::write(
            fish_dir.join("config.fish"),
            "test \"$XDG_CONFIG_HOME\" = \"$BMUX_ORIG_XDG_CONFIG_HOME\"; or exit 42\n",
        )
        .unwrap();
        for original in [Some(directory.path()), None] {
            let mut child = std::process::Command::new("fish");
            child
                .args(["--no-config", "-c"])
                .arg(format!(
                    "{}\nsh -c 'printf %s \"${{XDG_CONFIG_HOME-unset}}\"'",
                    shell_integration_fish_config()
                ))
                .env("XDG_CONFIG_HOME", "/tmp/bmux-private-xdg")
                .env_remove("BMUX_ORIG_XDG_CONFIG_HOME");
            if let Some(path) = original {
                child.env("BMUX_ORIG_XDG_CONFIG_HOME", path);
            } else {
                child.env("HOME", directory.path().join("no-user-config"));
            }
            let output = child.output().unwrap();
            assert!(output.status.success(), "{:?}", output.stderr);
            let expected = original.map_or_else(
                || "unset".into(),
                |path| path.to_string_lossy().into_owned(),
            );
            assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
        }
    }

    // Runs fish from PATH.
    #[cfg(unix)]
    #[test]
    fn fish_shell_metadata_preserves_long_commands_and_directories() {
        let directory = tempfile::Builder::new()
            .prefix(&"fish-metadata-".repeat(8))
            .tempdir()
            .unwrap();
        let cwd = directory
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let command = format!("printf {}", "resurrection".repeat(12));
        let token = "long-fields-token";
        let config = shell_integration_fish_config().replace(
            BMUX_SHELL_METADATA_PREFIX,
            &format!("{BMUX_SHELL_METADATA_PREFIX}{token};"),
        );
        let output = std::process::Command::new("fish")
            .args(["--no-config", "-c"])
            .arg(format!(
                "{config}\nemit fish_prompt\nemit fish_preexec '{command}'\n"
            ))
            .env("BMUX_ORIG_XDG_CONFIG_HOME", directory.path())
            .env("XDG_CONFIG_HOME", directory.path())
            .current_dir(directory.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output.stderr);
        let mut parser = PaneShellMetadataParser {
            token: Some(token.into()),
            ..Default::default()
        };
        let metadata = parser.process_chunk(&output.stdout);
        assert_eq!(
            metadata.events,
            vec![
                PaneShellMetadataEvent::Prompt { cwd: cwd.clone() },
                PaneShellMetadataEvent::CommandStart { command, cwd },
            ]
        );
        assert!(metadata.filtered.is_empty());
    }

    #[test]
    fn shell_metadata_is_bound_to_launch_token_across_chunk_boundaries() {
        let marker = b"\x1b]633;bmux;launch-token;prompt;L3RtcA==\x07";
        for split in 0..=marker.len() {
            let mut parser = PaneShellMetadataParser {
                token: Some("launch-token".into()),
                ..Default::default()
            };
            let mut events = parser.process_chunk(&marker[..split]).events;
            events.extend(parser.process_chunk(&marker[split..]).events);
            assert_eq!(
                events,
                vec![PaneShellMetadataEvent::Prompt { cwd: "/tmp".into() }]
            );
        }
        for marker in [
            b"\x1b]633;bmux;prompt;L3RtcA==\x07".as_slice(),
            b"\x1b]633;bmux;old-token;prompt;L3RtcA==\x07",
        ] {
            let mut parser = PaneShellMetadataParser {
                token: Some("launch-token".into()),
                ..Default::default()
            };
            let result = parser.process_chunk(marker);
            assert_eq!(
                result.events,
                [] as [crate::runtime::PaneShellMetadataEvent; 0]
            );
            assert_eq!(result.filtered, marker);
        }
    }

    /// Programs holding the pane's marker report their state; nothing else can.
    #[test]
    fn agent_reports_need_the_launch_token() {
        let parse = |marker: &[u8], token: Option<&str>| {
            let mut parser = PaneShellMetadataParser {
                token: token.map(str::to_string),
                ..Default::default()
            };
            let result = parser.process_chunk(marker);
            (result.events, result.filtered.into_owned())
        };
        // "claude" and "approve?" in base64.
        let (events, filtered) = parse(
            b"before\x1b]633;bmux;launch-token;agent;blocked;Y2xhdWRl;YXBwcm92ZT8=\x07after",
            Some("launch-token"),
        );
        assert_eq!(
            events,
            vec![PaneShellMetadataEvent::AgentReport {
                state: "blocked".into(),
                agent: Some("claude".into()),
                message: Some("approve?".into()),
            }]
        );
        assert_eq!(filtered, b"beforeafter");
        let (events, _) = parse(
            b"\x1b]633;bmux;launch-token;agent;working\x07",
            Some("launch-token"),
        );
        assert_eq!(
            events,
            vec![PaneShellMetadataEvent::AgentReport {
                state: "working".into(),
                agent: None,
                message: None,
            }]
        );
        for marker in [
            // Without the marker, or with another pane's, or with no token at all.
            (
                b"\x1b]633;bmux;agent;working\x07".as_slice(),
                Some("launch-token"),
            ),
            (
                b"\x1b]633;bmux;old-token;agent;working\x07",
                Some("launch-token"),
            ),
            (b"\x1b]633;bmux;agent;working\x07", None),
            // Unknown states, extra fields, bad encodings and control characters.
            (
                b"\x1b]633;bmux;launch-token;agent;sleeping\x07",
                Some("launch-token"),
            ),
            (
                b"\x1b]633;bmux;launch-token;agent;idle;;;extra\x07",
                Some("launch-token"),
            ),
            (
                b"\x1b]633;bmux;launch-token;agent;idle;***\x07",
                Some("launch-token"),
            ),
            (
                b"\x1b]633;bmux;launch-token;agent;idle;YQph\x07",
                Some("launch-token"),
            ),
        ] {
            assert_eq!(
                parse(marker.0, marker.1).0,
                [],
                "{:?}",
                String::from_utf8_lossy(marker.0)
            );
        }
    }

    #[test]
    fn foreground_jobs_list_their_leader_first_and_skip_zombies() {
        let entry = |pid, pgid, state: &str, command: &str| PsProcessEntry {
            pid,
            pgid,
            state: state.into(),
            command: command.into(),
        };
        let table = [
            entry(12, 10, "S", "tee log"),
            entry(11, 10, "Z", "dead"),
            entry(10, 10, "S", "node /opt/claude/cli.js"),
            entry(20, 20, "S", "other"),
            entry(13, 10, "S", ""),
        ];
        assert_eq!(
            foreground_group_commands(&table, 10),
            ["node /opt/claude/cli.js", "tee log"]
        );
        assert_eq!(foreground_group_commands(&table, 99).len(), 0);
    }

    /// A chunk carrying no metadata (escapes, but no OSC) passes through without being copied,
    /// and a chunk ending inside an escape is parsed, so an OSC split there is still found.
    #[test]
    fn chunks_without_metadata_pass_through_uncopied() {
        let mut parser = PaneShellMetadataParser::default();
        let plain = b"\x1b[1;31mred\x1b[0m \x1b\x1bx\r\n".as_slice();
        let output = parser.process_chunk(plain);
        assert!(matches!(output.filtered, std::borrow::Cow::Borrowed(_)));
        assert_eq!(&*output.filtered, plain);
        let first = parser.process_chunk(b"text\x1b");
        assert_eq!(&*first.filtered, b"text");
        let second = parser.process_chunk(b"]633;bmux;start;ZWNobyBoaQ==;L3RtcA==\x07after");
        assert_eq!(&*second.filtered, b"after");
        assert_eq!(second.events.len(), 1);
    }

    #[test]
    fn shell_metadata_parser_strips_bell_marker() {
        let mut parser = PaneShellMetadataParser::default();
        let output =
            parser.process_chunk(b"\x1b]633;bmux;start;ZWNobyBoaQ==;L3RtcA==\x07plain-output");

        assert_eq!(&*output.filtered, b"plain-output");
        assert_eq!(
            output.events,
            vec![PaneShellMetadataEvent::CommandStart {
                command: "echo hi".to_string(),
                cwd: "/tmp".to_string(),
            }]
        );
    }

    #[test]
    fn resurrection_runtime_prompt_clears_command_and_preserves_cwd() {
        let mut state = PaneResurrectionRuntime::default();
        state.apply_event(PaneShellMetadataEvent::CommandStart {
            command: "sleep 30".to_string(),
            cwd: "/work".to_string(),
        });
        state.apply_event(PaneShellMetadataEvent::Prompt {
            cwd: "/work".to_string(),
        });

        assert_eq!(state.active_command, None);
        assert_eq!(state.active_command_source, None);
        assert_eq!(state.last_known_cwd.as_deref(), Some("/work"));
    }

    #[test]
    fn restored_directory_is_not_a_current_execution_observation() {
        let snapshot = PaneResurrectionSnapshot {
            last_known_cwd: Some("/old/directory".into()),
            ..PaneResurrectionSnapshot::default()
        };
        let mut first = PaneResurrectionRuntime::from_snapshot(&snapshot);
        let replacement = PaneResurrectionRuntime::from_snapshot(&snapshot);
        assert_ne!(
            first.observation_execution,
            replacement.observation_execution
        );
        assert_eq!(first.observed_cwd, None);
        assert_eq!(replacement.observed_cwd, None);
        assert_eq!(first.to_snapshot(), snapshot);
        first.apply_event(PaneShellMetadataEvent::Prompt {
            cwd: "/current".into(),
        });
        assert_eq!(first.observed_cwd.as_deref(), Some("/current"));
        assert_eq!(replacement.observed_cwd, None);
        let restored = PaneResurrectionRuntime::from_snapshot(&first.to_snapshot());
        assert_eq!(restored.observed_cwd, None);
        assert_ne!(restored.observation_execution, first.observation_execution);
    }

    #[test]
    fn prompt_replay_waits_for_shell_prompt_and_marks_command_active() {
        let mut state = PaneResurrectionRuntime {
            execution_token: None,
            active_command: Some("lazygit".to_string()),
            active_command_source: Some(PaneCommandSource::Verbatim),
            last_known_cwd: Some("/work".to_string()),
            ..PaneResurrectionRuntime::default()
        };
        let mut pending_replay = Some("lazygit".to_string());

        let replay = apply_shell_metadata_events_and_take_prompt_replay(
            &mut state,
            &mut pending_replay,
            [PaneShellMetadataEvent::Prompt {
                cwd: "/work".to_string(),
            }],
        );

        assert_eq!(replay.as_deref(), Some("lazygit"));
        assert_eq!(pending_replay, None);
        assert_eq!(state.active_command.as_deref(), Some("lazygit"));
        assert_eq!(
            state.active_command_source,
            Some(PaneCommandSource::Verbatim)
        );
        assert_eq!(state.last_known_cwd.as_deref(), Some("/work"));
    }

    #[test]
    fn prompt_replay_is_one_shot_then_later_prompt_clears_command() {
        let mut state = PaneResurrectionRuntime::default();
        let mut pending_replay = Some("lazygit".to_string());

        let replay = apply_shell_metadata_events_and_take_prompt_replay(
            &mut state,
            &mut pending_replay,
            [PaneShellMetadataEvent::Prompt {
                cwd: "/work".to_string(),
            }],
        );
        assert_eq!(replay.as_deref(), Some("lazygit"));

        let replay = apply_shell_metadata_events_and_take_prompt_replay(
            &mut state,
            &mut pending_replay,
            [PaneShellMetadataEvent::Prompt {
                cwd: "/work".to_string(),
            }],
        );

        assert_eq!(replay, None);
        assert_eq!(state.active_command, None);
        assert_eq!(state.active_command_source, None);
    }

    // Runs a Unix bash from PATH.
    #[cfg(unix)]
    #[test]
    fn bash_prompt_work_does_not_consume_user_command_boundary() {
        let script = shell_integration_bash_rcfile()
            .replace("if [[ -f \"${HOME}/.bashrc\" ]]; then", "if false; then");
        let output = std::process::Command::new("bash")
            .args(["--noprofile", "--norc", "-c"])
            .arg(format!(
                "PROMPT_COMMAND=:\n{script}\neval \"$PROMPT_COMMAND\"\nprintf actual\n"
            ))
            .output()
            .unwrap();
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        assert_eq!(text.matches("633;bmux;start;").count(), 1, "{text:?}");
        assert!(
            text.contains("633;bmux;start;cHJpbnRmIGFjdHVhbA==;"),
            "{text:?}"
        );
        assert_eq!(text.matches("633;bmux;prompt;").count(), 1);
    }

    #[test]
    fn nu_shell_integration_uses_once_per_command_prompt_hook() {
        let config = shell_integration_nu_config();

        assert!(config.contains("__bmux_pre_prompt_hooks"));
        assert!(config.contains("__bmux_emit_prompt"));
        assert!(config.contains("upsert hooks.pre_prompt"));
        // Prompt rendering can happen repeatedly without a command boundary.
        assert!(!config.contains("$env.PROMPT_COMMAND ="));
    }

    #[test]
    fn pane_data_dirs_drop_other_panes_nested_shares() {
        let share = std::path::PathBuf::from("/state/runtime/shell-integration/new/nested-share");
        let inherited = std::env::join_paths([
            "/outer/runtime/shell-integration/old/nested-share",
            "/usr/share",
            "/home/user/nested-share",
        ])
        .unwrap();
        assert_eq!(
            pane_xdg_data_dirs(share.clone(), Some(inherited)),
            vec![
                share.clone(),
                std::path::PathBuf::from("/usr/share"),
                std::path::PathBuf::from("/home/user/nested-share"),
            ]
        );
        assert_eq!(
            pane_xdg_data_dirs(share.clone(), None),
            vec![
                share,
                std::path::PathBuf::from("/usr/local/share"),
                std::path::PathBuf::from("/usr/share"),
            ]
        );
    }

    #[test]
    fn nested_nu_integration_installs_once_per_process() {
        let config = shell_integration_nested_nu();

        // An inherited XDG_DATA_DIRS (bmux started inside a bmux pane) can
        // autoload this file twice; doubled hooks would report each boundary
        // twice and replace a restored execution with a manual one.
        assert!(
            config.contains("($env.__BMUX_NESTED_PID? | default '') != ($nu.pid | into string)")
        );
        assert!(config.contains("$env.__BMUX_NESTED_PID = ($nu.pid | into string)"));
    }

    #[test]
    fn missing_persisted_shell_falls_back_and_preserves_same_family_replay() {
        let configured_shell = std::env::current_exe()
            .expect("current test executable")
            .with_file_name(if cfg!(windows) { "nu.exe" } else { "nu" });
        std::fs::copy(std::env::current_exe().unwrap(), &configured_shell).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut permissions = configured_shell.metadata().unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&configured_shell, permissions).unwrap();
        }
        let pane_meta = PaneRuntimeMeta {
            id: crate::ids::new_uuid(),
            name: None,
            shell: "/nix/store/missing-nushell/bin/nu".to_string(),
            launch: None,
            resurrection: PaneResurrectionSnapshot {
                execution_token: None,
                active_command: Some("lazygit".to_string()),
                active_command_source: Some(PaneCommandSource::Verbatim),
                last_known_cwd: Some(std::env::temp_dir().to_string_lossy().into_owned()),
            },
        };

        let resolved = resolve_pane_launch(&pane_meta, &configured_shell.to_string_lossy())
            .expect("configured shell path");

        assert_eq!(resolved.shell, configured_shell.to_string_lossy());
        assert_eq!(resolved.replay_command.as_deref(), Some("lazygit"));
        assert!(resolved.cwd.is_some());
        assert!(resolved.metadata_changed);
        std::fs::remove_file(configured_shell).unwrap();
    }

    #[test]
    fn cross_family_shell_fallback_suppresses_replay() {
        let configured_shell = std::env::current_exe()
            .expect("current test executable")
            .with_file_name(if cfg!(windows) { "fish.exe" } else { "fish" });
        std::fs::copy(std::env::current_exe().unwrap(), &configured_shell).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut permissions = configured_shell.metadata().unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&configured_shell, permissions).unwrap();
        }
        let pane_meta = PaneRuntimeMeta {
            id: crate::ids::new_uuid(),
            name: None,
            shell: "/missing/zsh".to_string(),
            launch: None,
            resurrection: PaneResurrectionSnapshot {
                execution_token: None,
                active_command: Some("print -r -- hello".to_string()),
                active_command_source: Some(PaneCommandSource::Verbatim),
                last_known_cwd: None,
            },
        };

        let resolved = resolve_pane_launch(&pane_meta, &configured_shell.to_string_lossy())
            .expect("configured shell path");

        assert_eq!(resolved.shell, configured_shell.to_string_lossy());
        assert_eq!(resolved.replay_command, None);
        assert!(resolved.metadata_changed);
        std::fs::remove_file(configured_shell).unwrap();
    }

    #[test]
    fn missing_resurrection_cwd_is_cleared() {
        let pane_meta = PaneRuntimeMeta {
            id: crate::ids::new_uuid(),
            name: None,
            shell: "sh".to_string(),
            launch: None,
            resurrection: PaneResurrectionSnapshot {
                execution_token: None,
                active_command: None,
                active_command_source: None,
                last_known_cwd: Some(
                    std::env::temp_dir()
                        .join(crate::ids::new_uuid().to_string())
                        .to_string_lossy()
                        .into_owned(),
                ),
            },
        };

        let resolved = resolve_pane_launch(&pane_meta, "sh").expect("bare shell is valid");

        assert_eq!(resolved.cwd, None);
        assert!(resolved.metadata_changed);
    }

    #[test]
    fn shell_resolution_uses_usable_baseline_after_missing_candidates() {
        let baseline = if cfg!(windows) { "cmd.exe" } else { "/bin/sh" };
        let resolved = resolve_shell_with_baseline(
            "/definitely/missing/persisted-shell",
            "/definitely/missing/configured-shell",
            baseline,
        )
        .expect("platform baseline shell should be usable");
        assert_eq!(resolved, baseline);
    }

    #[test]
    fn shell_resolution_reports_all_unusable_candidates() {
        let error = resolve_shell_with_baseline(
            "/definitely/missing/persisted-shell",
            "/definitely/missing/configured-shell",
            "/definitely/missing/baseline-shell",
        )
        .expect_err("all path candidates are missing");
        let message = error.to_string();
        assert!(message.contains("persisted-shell"));
        assert!(message.contains("configured-shell"));
        assert!(message.contains("baseline-shell"));
    }

    #[test]
    fn explicit_launch_does_not_replace_persisted_shell_metadata() {
        let pane_meta = PaneRuntimeMeta {
            id: crate::ids::new_uuid(),
            name: None,
            shell: "/missing/original-shell".to_string(),
            launch: Some(PaneLaunchSpec {
                program: "custom-program".to_string(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
            }),
            resurrection: PaneResurrectionSnapshot::default(),
        };

        let resolved = resolve_pane_launch(&pane_meta, "sh").expect("explicit launch metadata");

        assert_eq!(resolved.shell, pane_meta.shell);
        assert!(!resolved.metadata_changed);
    }

    #[test]
    fn runtime_notice_updates_grid_and_fanout() {
        let pane_id = crate::ids::new_uuid();
        let session_id = SessionId(crate::ids::new_uuid());
        let output = Arc::new(switchy_thread::sync::Mutex::new(OutputFanoutBuffer::new(
            4096,
        )));
        let grid = Arc::new(switchy_thread::sync::Mutex::new(
            TerminalGridStream::new(80, 24, GridLimits::default()).unwrap(),
        ));
        let dirty = Arc::new(AtomicBool::new(false));

        push_pane_runtime_notice(
            &PaneRuntimeHost::detached(),
            session_id,
            pane_id,
            &output,
            &grid,
            &dirty,
            "pane failed visibly",
        );

        assert!(dirty.load(Ordering::SeqCst));
        assert!(
            output
                .lock()
                .unwrap()
                .read_recent_with_offsets(4096)
                .bytes
                .windows(b"pane failed visibly".len())
                .any(|window| window == b"pane failed visibly")
        );
        let snapshot = grid.lock().unwrap().snapshot(0, 24);
        let encoded = serde_json::to_vec(&snapshot).unwrap();
        assert!(
            encoded
                .windows(b"pane failed visibly".len())
                .any(|window| window == b"pane failed visibly")
        );
    }

    #[test]
    fn resolved_launch_heals_pane_metadata() {
        let mut pane_meta = PaneRuntimeMeta {
            id: crate::ids::new_uuid(),
            name: None,
            shell: "/missing/zsh".to_string(),
            launch: None,
            resurrection: PaneResurrectionSnapshot {
                execution_token: None,
                active_command: Some("zsh-only-command".to_string()),
                active_command_source: Some(PaneCommandSource::Verbatim),
                last_known_cwd: Some("/missing/cwd".to_string()),
            },
        };
        let resolved = ResolvedPaneLaunch {
            shell: "/bin/sh".to_string(),
            cwd: None,
            replay_command: None,
            metadata_changed: true,
        };

        apply_resolved_pane_launch(&mut pane_meta, &resolved);

        assert_eq!(pane_meta.shell, "/bin/sh");
        assert_eq!(pane_meta.resurrection.last_known_cwd, None);
        assert_eq!(pane_meta.resurrection.active_command, None);
        assert_eq!(pane_meta.resurrection.active_command_source, None);
    }

    #[test]
    fn explicit_launch_validation_rejects_empty_program_and_missing_cwd() {
        let empty_program = PaneLaunchSpec {
            program: " ".to_string(),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
        };
        assert!(
            validate_explicit_launch(&empty_program)
                .unwrap_err()
                .to_string()
                .contains("program is empty")
        );

        let missing_cwd = PaneLaunchSpec {
            program: "custom-program".to_string(),
            args: Vec::new(),
            cwd: Some(
                std::env::temp_dir()
                    .join(crate::ids::new_uuid().to_string())
                    .to_string_lossy()
                    .into_owned(),
            ),
            env: BTreeMap::new(),
        };
        assert!(
            validate_explicit_launch(&missing_cwd)
                .unwrap_err()
                .to_string()
                .contains("cwd is unavailable")
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn native_process_directory_observation_matches_current_process() {
        let observed = observe_process_directory(std::process::id()).unwrap();
        assert_eq!(
            Path::new(&observed).canonicalize().unwrap(),
            std::env::current_dir().unwrap().canonicalize().unwrap()
        );
        assert_eq!(observe_process_directory(u32::MAX), None);
    }

    #[test]
    fn versioned_split_denial_precedes_directory_validation() {
        use bmux_pane_runtime_plugin_api::{
            pane_runtime_spawn_commands_v1::client::SplitShellRequest,
            pane_runtime_spawn_v1::{InitialShellOptions, SpawnError},
        };
        let request = SplitShellRequest {
            session_id: crate::ids::new_uuid(),
            target: crate::ids::new_uuid(),
            direction: "horizontal".into(),
            ratio_percent: 50,
            options: InitialShellOptions {
                cwd: Some("relative-invalid-path".into()),
            },
        };
        let result = split_shell_v1(&PaneRuntimeHost::detached(), request, || {
            Err(spawn_failure(SpawnError::Denied {
                reason: "not authorized".into(),
            }))
        });
        assert_eq!(
            spawn_wire_error(result.unwrap_err()),
            SpawnError::Denied {
                reason: "not authorized".into(),
            }
        );
    }

    #[test]
    fn spawn_errors_preserve_typed_denial_directory_and_cleanup() {
        use bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::SpawnError;
        for error in [
            SpawnError::Denied {
                reason: "policy denied".into(),
            },
            SpawnError::InvalidDirectory {
                path: "/missing".into(),
                reason: "missing".into(),
            },
            SpawnError::CleanupFailed {
                reason: "task failed".into(),
            },
            SpawnError::SourceUnavailable,
        ] {
            assert_eq!(spawn_wire_error(spawn_failure(error.clone())), error);
        }
        assert!(matches!(
            spawn_wire_error(anyhow::anyhow!("launch failed")),
            SpawnError::LaunchFailed { .. }
        ));
    }

    #[switchy_async::test]
    async fn unpublished_cleanup_waits_for_task_and_reports_panics() {
        let mut pane = dummy_pane(crate::ids::new_uuid());
        stop_unpublished_pane(&mut pane).await.unwrap();
        assert!(pane.task.is_finished());
        assert!(!pane.publication.wait());

        let mut pane = dummy_pane(crate::ids::new_uuid());
        pane.task.abort();
        pane.task = switchy_async::task::spawn(async { panic!("injected cleanup failure") });
        let error = stop_unpublished_pane(&mut pane).await.unwrap_err();
        assert!(error.to_string().contains("task cleanup failed"));
    }

    #[switchy_async::test]
    async fn split_commit_rejects_replaced_target_before_publication() {
        let session_id = SessionId(crate::ids::new_uuid());
        let target = crate::ids::new_uuid();
        let mut manager = manager_with_runtime(session_id, runtime_with_panes(&[target]));
        let mut prepared = manager
            .prepare_split(session_id, None, PaneSplitDirection::Horizontal, None)
            .unwrap();
        manager.runtimes[&session_id].panes[&target]
            .resurrection_state
            .lock()
            .unwrap()
            .observation_execution = crate::ids::new_uuid();
        let error = manager
            .commit_split(
                session_id,
                &mut prepared,
                PaneSplitDirection::Horizontal,
                0.5,
            )
            .unwrap_err();
        assert!(error.to_string().contains("execution changed"));
        let pane = prepared.pane.as_mut().unwrap();
        stop_unpublished_pane(pane).await.unwrap();
        assert!(pane.task.is_finished());
        assert_eq!(manager.runtimes[&session_id].panes.len(), 1);
        assert_eq!(manager.runtimes[&session_id].focused_pane_id, target);
        assert!(manager.pane_input_index.read().unwrap().is_empty());
    }

    #[switchy_async::test]
    async fn split_commit_rejects_exited_target_without_publishing_new_pane() {
        let session_id = SessionId(crate::ids::new_uuid());
        let target = crate::ids::new_uuid();
        let mut manager = manager_with_runtime(session_id, runtime_with_panes(&[target]));
        let mut prepared = manager
            .prepare_split(session_id, None, PaneSplitDirection::Horizontal, None)
            .unwrap();
        manager.runtimes[&session_id].panes[&target]
            .exited
            .store(true, Ordering::SeqCst);
        let error = manager
            .commit_split(
                session_id,
                &mut prepared,
                PaneSplitDirection::Horizontal,
                0.5,
            )
            .unwrap_err();
        assert!(error.to_string().contains("execution exited"));
        stop_unpublished_pane(prepared.pane.as_mut().unwrap())
            .await
            .unwrap();
        assert_eq!(manager.runtimes[&session_id].panes.len(), 1);
        assert_eq!(manager.runtimes[&session_id].focused_pane_id, target);
        assert!(manager.pane_input_index.read().unwrap().is_empty());
    }

    #[test]
    fn startup_publication_abandonment_cannot_be_reactivated() {
        let publication = StartupPublication::default();
        publication.decide(false);
        publication.decide(true);
        assert!(!publication.wait());
    }

    fn assert_managed_idle_rejects_racing_preparation(
        manager: &Arc<Mutex<SessionRuntimeManager>>,
        session: SessionId,
    ) {
        let contender = Arc::clone(manager);
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let competing = Arc::clone(&barrier);
        let runtime = switchy_async::runtime::Handle::current();
        let thread = {
            let guard = manager.lock().unwrap();
            let thread = switchy_thread::thread::spawn(move || {
                let _runtime = runtime.enter();
                competing.wait();
                contender
                    .lock()
                    .unwrap()
                    .prepare_fresh_runtime(session, None)
                    .is_err()
            });
            barrier.wait();
            assert!(managed_idle::fence_for_test(&guard));
            thread
        };
        assert!(
            thread.join().unwrap(),
            "admission racing the shutdown fence must reject before spawn"
        );
    }

    #[switchy_async::test(flavor = "multi_thread", worker_threads = 2)]
    async fn managed_idle_fence_serializes_real_prepare_attach_and_restore() {
        const CHILD: &str = "BMUX_TEST_MANAGED_IDLE_AUTHORITY_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // Exercise the real admission authority in a fresh process without
            // inherited managed-target markers.
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "runtime::tests::managed_idle_fence_serializes_real_prepare_attach_and_restore",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env_remove("BMUX_MANAGED_TARGET_ID")
                .env_remove("BMUX_MANAGED_TARGET_IDENTITY")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        let session = SessionId(crate::ids::new_uuid());
        let client = ClientId(crate::ids::new_uuid());
        let pane = crate::ids::new_uuid();
        let mut manager = manager_with_runtime(session, runtime_with_panes(&[pane]));
        // The idle authority under test is the manager's own activation's.
        let manager_host = manager.host.clone();
        let host = &manager_host;
        manager.begin_attach(session, client).unwrap();
        assert!(!managed_idle::fence_for_test(&manager));
        manager.end_attach(session, client);
        assert!(
            !managed_idle::fence_for_test(&manager),
            "detached quiet shell still owns work"
        );
        let runtime = manager.runtimes.remove(&session).unwrap();
        shutdown_runtime_handle(
            host,
            RemovedRuntime {
                session_id: session,
                handle: runtime,
            },
        )
        .await;
        let prepared_connection = crate::ids::new_uuid();
        let lease = managed_idle::preparation_for_test(host, prepared_connection, None, false);
        assert!(
            !managed_idle::fence_for_test(&manager),
            "prepared connection blocks idle before any runtime exists"
        );
        assert_eq!(
            managed_idle::preparation_for_test(host, prepared_connection, Some(lease), false),
            lease
        );
        managed_idle::preparation_for_test(host, prepared_connection, Some(lease), true);
        let prepared = manager.prepare_fresh_runtime(session, None).unwrap();
        assert!(
            !managed_idle::fence_for_test(&manager),
            "unpublished startup must block retirement"
        );
        // Abandon before launch; the real cancellation cleanup keeps its admission.
        drop(prepared);
        // No spawned child exists in this preparation: the gated startup owner
        // wakes on cancellation. Wait boundedly for actual cleanup ownership.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !managed_idle::admissions_settled_for_test(host) {
            assert!(
                Instant::now() < deadline,
                "startup cleanup failed to settle"
            );
            switchy_async::time::sleep(Duration::from_millis(5)).await;
        }
        let manager = Arc::new(Mutex::new(manager));
        assert_managed_idle_rejects_racing_preparation(&manager, session);
        let mut manager = Arc::try_unwrap(manager).ok().unwrap().into_inner().unwrap();
        assert!(manager.prepare_fresh_runtime(session, None).is_err());
        manager
            .runtimes
            .insert(session, runtime_with_panes(&[pane]));
        assert!(manager.begin_attach(session, client).is_err());
        assert!(manager.begin_observer_attach(session, client).is_err());
        assert!(
            manager
                .restore_runtime(
                    SessionId(crate::ids::new_uuid()),
                    &[],
                    RestoreRuntimeRequest::default()
                )
                .is_err()
        );
        let runtime = manager.runtimes.remove(&session).unwrap();
        shutdown_runtime_handle(
            host,
            RemovedRuntime {
                session_id: session,
                handle: runtime,
            },
        )
        .await;
    }

    #[test]
    fn startup_publication_waits_for_commit() {
        let publication = Arc::new(StartupPublication::default());
        let waiter = Arc::clone(&publication);
        let (result, receive) = switchy_thread::sync::mpsc::channel();
        let thread = switchy_thread::thread::spawn(move || result.send(waiter.wait()).unwrap());
        assert!(receive.try_recv().is_err());
        publication.decide(true);
        assert!(receive.recv_timeout(Duration::from_secs(2)).unwrap());
        thread.join().unwrap();
        publication.decide(false);
        assert!(publication.wait());
    }

    #[switchy_async::test]
    async fn failed_startup_watch_prevents_readiness() {
        let host = crate::runtime::PaneRuntimeHost::detached();
        let pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane_id]);
        let (ready, receiver) = watch::channel(Some(Err("injected startup failure".into())));
        runtime.panes.get_mut(&pane_id).unwrap().startup = receiver;
        let mut prepared = PreparedFreshRuntime {
            cleanup: PreparedCleanup::reserve(&host).unwrap(),
            runtime: Some(runtime),
            launch: None,
        };
        let error = prepared.wait_until_ready().await.unwrap_err();
        assert_eq!(error.to_string(), "injected startup failure");
        assert!(prepared.runtime.is_some());
        drop(ready);
        drop(prepared);
    }

    #[switchy_async::test]
    async fn readiness_wait_retains_ownership_until_commit() {
        let host = crate::runtime::PaneRuntimeHost::detached();
        let host = &host;
        let pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane_id]);
        let (ready, receiver) = watch::channel(None);
        runtime.panes.get_mut(&pane_id).unwrap().startup = receiver;
        let mut prepared = PreparedFreshRuntime {
            cleanup: PreparedCleanup::reserve(host).unwrap(),
            runtime: Some(runtime),
            launch: None,
        };
        assert!(
            switchy_async::time::timeout(Duration::from_millis(10), prepared.wait_until_ready())
                .await
                .is_err()
        );
        assert!(prepared.runtime.is_some());
        ready.send_replace(Some(Ok(())));
        prepared.wait_until_ready().await.unwrap();
        assert!(prepared.runtime.is_some());
        shutdown_runtime_handle(
            host,
            RemovedRuntime {
                session_id: SessionId(crate::ids::new_uuid()),
                handle: prepared.take().unwrap(),
            },
        )
        .await;
    }

    #[derive(Debug, Clone)]
    struct DelayedCleanupChild {
        exited: Arc<(
            switchy_thread::sync::Mutex<bool>,
            switchy_thread::sync::Condvar,
        )>,
        dropped: Arc<AtomicBool>,
    }

    impl Drop for DelayedCleanupChild {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    impl crate::runtime::pty::ChildKiller for DelayedCleanupChild {
        fn kill(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn crate::runtime::pty::ChildKiller + Send + Sync> {
            Box::new(self.clone())
        }
    }

    impl crate::runtime::pty::Child for DelayedCleanupChild {
        fn try_wait(&mut self) -> std::io::Result<Option<bmux_pty::ChildExit>> {
            Ok((*self.exited.0.lock().unwrap()).then(|| bmux_pty::ChildExit::with_exit_code(0)))
        }

        fn wait(&mut self) -> std::io::Result<bmux_pty::ChildExit> {
            let (state, changed) = &*self.exited;
            let mut exited = state.lock().unwrap();
            while !*exited {
                exited = changed.wait(exited).unwrap();
            }
            Ok(bmux_pty::ChildExit::with_exit_code(0))
        }

        fn process_id(&self) -> Option<u32> {
            None
        }
    }

    #[switchy_async::test]
    async fn failed_child_cleanup_retains_owner_until_observed_exit() {
        let host = crate::runtime::PaneRuntimeHost::detached();
        let host = &host;
        let exited = Arc::new((
            switchy_thread::sync::Mutex::new(false),
            switchy_thread::sync::Condvar::new(),
        ));
        let dropped = Arc::new(AtomicBool::new(false));
        let child = Box::new(DelayedCleanupChild {
            exited: Arc::clone(&exited),
            dropped: Arc::clone(&dropped),
        });
        let (ack, mut observation) = watch::channel(None);
        let waiter = cleanup_failed_startup_child_on(
            host,
            &switchy_async::runtime::Handle::current(),
            child,
            "injected startup failure".into(),
            Some(ack),
        );
        assert!(
            switchy_async::time::timeout(Duration::from_millis(20), observation.changed())
                .await
                .is_err()
        );
        let still_owned = !dropped.load(Ordering::SeqCst);
        // Release the fixture before asserting: a failed assertion must not
        // leave a blocking reaper hanging during runtime teardown.
        *exited.0.lock().unwrap() = true;
        exited.1.notify_all();
        let (reason, result) = switchy_async::time::timeout(Duration::from_secs(2), waiter)
            .await
            .unwrap();
        assert!(still_owned);
        assert_eq!(reason, "injected startup failure");
        assert!(result.is_ok(), "elapsed time alone is not cleanup failure");
        assert!(dropped.load(Ordering::SeqCst));
        assert!(observation.borrow().as_ref().unwrap().is_ok());
    }

    #[test]
    fn startup_stop_check_distinguishes_pending_cancelled_and_abandoned() {
        let (stop, mut receiver) = oneshot::channel();
        assert!(!startup_stop_requested(&mut receiver));
        stop.send(()).unwrap();
        assert!(startup_stop_requested(&mut receiver));
        let (stop, mut receiver) = oneshot::channel::<()>();
        drop(stop);
        assert!(startup_stop_requested(&mut receiver));
    }

    #[switchy_async::test]
    async fn prepared_cleanup_reservation_bounds_admission_and_releases_on_drop() {
        let host = PaneRuntimeHost::detached();
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let reservation = PreparedCleanup::reserve_from(&host, Arc::clone(&slots)).unwrap();
        assert!(PreparedCleanup::reserve_from(&host, Arc::clone(&slots)).is_err());
        let cleanup_owner = Arc::clone(&reservation);
        drop(reservation);
        assert_eq!(slots.available_permits(), 0);
        drop(cleanup_owner);
        assert_eq!(slots.available_permits(), 1);
        assert!(PreparedCleanup::reserve_from(&host, slots).is_ok());
    }

    #[switchy_async::test]
    async fn dropped_prepared_runtime_retains_cleanup_acknowledgement() {
        let existing = SessionId(crate::ids::new_uuid());
        let manager = manager_with_runtime(existing, runtime_with_panes(&[crate::ids::new_uuid()]));
        let prepared = manager
            .prepare_fresh_runtime(SessionId(crate::ids::new_uuid()), None)
            .unwrap();
        let mut cleanup = prepared
            .runtime
            .as_ref()
            .unwrap()
            .panes
            .values()
            .next()
            .unwrap()
            .cleanup
            .clone();
        drop(prepared);
        switchy_async::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(result) = cleanup.borrow_and_update().clone() {
                    result.expect("unlaunched resource cleanup must be acknowledged");
                    break;
                }
                cleanup.changed().await.expect("cleanup owner retained");
            }
        })
        .await
        .expect("cleanup must complete after request owner is dropped");
    }

    #[switchy_async::test]
    async fn commit_rejects_unready_runtime_without_publication() {
        let existing = SessionId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let mut manager = manager_with_runtime(existing, runtime_with_panes(&[pane_id]));
        let created = SessionId(crate::ids::new_uuid());
        let mut prepared = manager.prepare_fresh_runtime(created, None).unwrap();
        assert!(
            manager
                .commit_fresh_runtime(created, &mut prepared, || {
                    panic!("unready runtime must not publish its roster")
                })
                .is_err()
        );
        let pane = prepared
            .runtime
            .as_mut()
            .unwrap()
            .panes
            .values_mut()
            .next()
            .unwrap();
        stop_unpublished_pane(pane).await.unwrap();
        assert!(pane.task.is_finished());
        assert!(!manager.runtimes.contains_key(&created));
        assert!(manager.pane_input_index.read().unwrap().is_empty());
    }

    #[switchy_async::test]
    async fn prepared_runtime_rejects_lost_launch_receiver() {
        let host = crate::runtime::PaneRuntimeHost::detached();
        let pane_id = crate::ids::new_uuid();
        let runtime = runtime_with_panes(&[pane_id]);
        let task = runtime.panes[&pane_id].task.abort_handle();
        let (launch, receiver) = oneshot::channel();
        drop(receiver);
        let prepared = PreparedFreshRuntime {
            cleanup: PreparedCleanup::reserve(&host).unwrap(),
            runtime: Some(runtime),
            launch: Some(launch),
        };
        let Err(error) = prepared.take() else {
            panic!("dead startup must not transfer ownership");
        };
        assert!(error.to_string().contains("exited before launch"));
        switchy_async::time::timeout(Duration::from_secs(2), async {
            while !task.is_finished() {
                switchy_async::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[switchy_async::test]
    async fn abandoned_prepared_runtime_stops_all_owned_tasks() {
        let host = crate::runtime::PaneRuntimeHost::detached();
        let first = crate::ids::new_uuid();
        let second = crate::ids::new_uuid();
        let runtime = runtime_with_panes(&[first, second]);
        let finished = runtime
            .panes
            .values()
            .map(|pane| pane.task.abort_handle())
            .collect::<Vec<_>>();
        drop(PreparedFreshRuntime {
            cleanup: PreparedCleanup::reserve(&host).unwrap(),
            runtime: Some(runtime),
            launch: None,
        });
        switchy_async::time::timeout(Duration::from_secs(2), async {
            while finished.iter().any(|task| !task.is_finished()) {
                switchy_async::task::yield_now().await;
            }
        })
        .await
        .expect("all prepared tasks must stop when ownership is abandoned");
    }

    #[cfg(unix)]
    #[switchy_async::test]
    async fn abandoning_preparation_never_launches_shell() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("launched");
        let shell = directory.path().join("test-shell");
        std::fs::write(&shell, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o700)).unwrap();
        let existing = SessionId(crate::ids::new_uuid());
        let mut manager =
            manager_with_runtime(existing, runtime_with_panes(&[crate::ids::new_uuid()]));
        manager.shell = shell.to_string_lossy().into_owned();
        let prepared = manager
            .prepare_fresh_runtime(SessionId(crate::ids::new_uuid()), None)
            .unwrap();
        let tasks = prepared
            .runtime
            .as_ref()
            .unwrap()
            .panes
            .values()
            .map(|pane| pane.task.abort_handle())
            .collect::<Vec<_>>();
        switchy_async::task::yield_now().await;
        assert!(!marker.exists());
        drop(prepared);
        switchy_async::time::timeout(Duration::from_secs(2), async {
            while tasks.iter().any(|task| !task.is_finished()) {
                switchy_async::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            !marker.exists(),
            "abandoned preparation must never launch its shell"
        );
    }

    #[switchy_async::test]
    async fn preparing_fresh_runtime_does_not_register_input_or_manager_state() {
        let host = crate::runtime::PaneRuntimeHost::detached();
        let host = &host;
        let existing = SessionId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let manager = manager_with_runtime(existing, runtime_with_panes(&[pane_id]));
        let created = SessionId(crate::ids::new_uuid());
        let prepared = manager.prepare_fresh_runtime(created, None).unwrap();
        switchy_async::task::yield_now().await;
        let pending = prepared.runtime.as_ref().unwrap();
        for pane in pending.panes.values() {
            assert!(pane.startup.borrow().is_none());
            assert!(pane.process_id.lock().unwrap().is_none());
        }
        assert_eq!(manager.runtimes.len(), 1);
        assert!(!manager.runtimes.contains_key(&created));
        assert!(manager.pane_input_index.read().unwrap().is_empty());
        assert_eq!(manager.runtimes[&existing].focused_pane_id, pane_id);
        shutdown_runtime_handle(
            host,
            RemovedRuntime {
                session_id: created,
                handle: prepared.take().unwrap(),
            },
        )
        .await;
    }

    #[switchy_async::test]
    async fn invalid_initial_directory_does_not_mutate_runtime() {
        use bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::InitialShellOptions;
        let session_id = SessionId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let mut runtime = runtime_with_panes(&[pane_id]);
        runtime.zoomed_pane_id = Some(pane_id);
        let mut manager = manager_with_runtime(session_id, runtime);
        let directory = tempfile::tempdir().unwrap();
        for cwd in [
            "relative".to_string(),
            directory
                .path()
                .join("missing")
                .to_string_lossy()
                .into_owned(),
        ] {
            assert!(
                manager
                    .start_runtime_with_options(
                        SessionId(crate::ids::new_uuid()),
                        Some(InitialShellOptions {
                            cwd: Some(cwd.clone())
                        }),
                    )
                    .is_err()
            );
            assert!(
                manager
                    .create_split_pane(
                        session_id,
                        Some(PaneSelector::ById(pane_id)),
                        PaneSplitDirection::Horizontal,
                        None,
                        None,
                        Some(InitialShellOptions { cwd: Some(cwd) }),
                    )
                    .is_err()
            );
            assert_eq!(manager.runtimes.len(), 1);
            let runtime = &manager.runtimes[&session_id];
            assert_eq!(runtime.panes.len(), 1);
            assert_eq!(runtime.focused_pane_id, pane_id);
            assert_eq!(runtime.zoomed_pane_id, Some(pane_id));
            assert!(manager.pane_input_index.read().unwrap().is_empty());
        }
    }

    #[cfg(unix)]
    #[switchy_async::test]
    async fn fresh_shell_options_reach_pty_without_resurrection_metadata() {
        use bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::InitialShellOptions;
        let host = crate::runtime::PaneRuntimeHost::detached();
        let host = &host;
        let directory = tempfile::tempdir().unwrap();
        let session_id = SessionId(crate::ids::new_uuid());
        let (pane_exit_tx, _pane_exit_rx) = mpsc::unbounded_channel();
        let mut manager = SessionRuntimeManager::new(
            PaneRuntimeHost::detached(),
            "/bin/sh".into(),
            "xterm-256color".into(),
            ProtocolProfile::Bmux,
            true,
            None,
            PanePaddingConfig::default(),
            pane_exit_tx,
        );
        manager
            .start_runtime_with_options(
                session_id,
                Some(InitialShellOptions {
                    cwd: Some(directory.path().to_string_lossy().into_owned()),
                }),
            )
            .unwrap();
        let runtime = manager.runtimes.get(&session_id).unwrap();
        let first = runtime.focused_pane_id;
        // Exercise the split path too, with a different directory from pane one.
        let split_directory = tempfile::tempdir().unwrap();
        let split = manager
            .create_split_pane(
                session_id,
                Some(PaneSelector::ById(first)),
                PaneSplitDirection::Horizontal,
                None,
                None,
                Some(InitialShellOptions {
                    cwd: Some(split_directory.path().to_string_lossy().into_owned()),
                }),
            )
            .unwrap();
        let runtime = manager.runtimes.remove(&session_id).unwrap();
        let mut panes = runtime.panes;
        let first_pane = panes.remove(&first).unwrap();
        let pane = panes.remove(&split).unwrap();
        let (first_result, split_result) = switchy_async::join!(
            reported_shell_directory(&first_pane, directory.path()),
            reported_shell_directory(&pane, split_directory.path()),
        );
        shutdown_pane_handle(host, first_pane).await;
        shutdown_pane_handle(host, pane).await;
        assert_eq!(
            first_result.unwrap(),
            directory.path().canonicalize().unwrap()
        );
        assert_eq!(
            split_result.unwrap(),
            split_directory.path().canonicalize().unwrap()
        );
    }

    #[cfg(unix)]
    #[switchy_async::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fresh_split_workflow_checks_authority_again_and_starts_in_requested_directory() {
        use bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::{
            InitialShellOptions, SpawnError,
        };
        let host = crate::runtime::PaneRuntimeHost::detached();
        let host = &host;
        let session = SessionId(crate::ids::new_uuid());
        let target = crate::ids::new_uuid();
        let manager = manager_with_runtime(session, runtime_with_panes(&[target]));
        let adapter = ServerSessionRuntimeAdapter::new(Arc::new(Mutex::new(manager)));
        let directory = tempfile::tempdir().unwrap();
        let checks = AtomicUsize::new(0);
        let denied = adapter.split_fresh_shell(
            session,
            Some(PaneSelector::ById(target)),
            PaneSplitDirection::Horizontal,
            Some(InitialShellOptions {
                cwd: Some(directory.path().to_string_lossy().into_owned()),
            }),
            0.4,
            || {
                if checks.fetch_add(1, Ordering::SeqCst) == 0 {
                    Ok(())
                } else {
                    Err(spawn_failure(SpawnError::Denied {
                        reason: "revoked during startup".into(),
                    }))
                }
            },
        );
        let denied = spawn_wire_error(denied.unwrap_err());
        assert!(
            matches!(denied, SpawnError::Denied { .. }),
            "unexpected split rejection: {denied:?}"
        );
        assert_eq!(checks.load(Ordering::SeqCst), 2);
        assert_eq!(
            adapter.inner.lock().unwrap().runtimes[&session].panes.len(),
            1
        );
        assert!(adapter.pane_input_index.read().unwrap().is_empty());

        let pane_id = adapter
            .split_fresh_shell(
                session,
                Some(PaneSelector::ById(target)),
                PaneSplitDirection::Horizontal,
                Some(InitialShellOptions {
                    cwd: Some(directory.path().to_string_lossy().into_owned()),
                }),
                0.4,
                || Ok(()),
            )
            .unwrap();
        let runtime = adapter
            .inner
            .lock()
            .unwrap()
            .runtimes
            .remove(&session)
            .unwrap();
        let mut panes = runtime.panes;
        let pane = panes.remove(&pane_id).unwrap();
        let reported = reported_shell_directory(&pane, directory.path()).await;
        shutdown_pane_handle(host, pane).await;
        for pane in panes.into_values() {
            shutdown_pane_handle(host, pane).await;
        }
        assert_eq!(reported.unwrap(), directory.path().canonicalize().unwrap());
    }

    #[cfg(unix)]
    #[switchy_async::test]
    async fn fresh_session_roster_failure_never_publishes_runtime() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let shell = directory.path().join("queued-output-shell");
        let ready = directory.path().join("ready");
        // Publication has not released the reader. Ensure output is queued
        // before rollback: Darwin's session anchor cannot exit with this
        // unread output until rollback releases the final master.
        std::fs::write(
            &shell,
            format!(
                "#!/bin/sh\n: > '{}'\nexec dd if=/dev/zero bs=1048576 count=1 2>/dev/null\n",
                ready.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o700)).unwrap();
        let existing = SessionId(crate::ids::new_uuid());
        let target = crate::ids::new_uuid();
        let mut manager = manager_with_runtime(existing, runtime_with_panes(&[target]));
        manager.shell = shell.to_string_lossy().into_owned();
        let adapter = ServerSessionRuntimeAdapter::new(Arc::new(Mutex::new(manager)));
        let created = SessionId(crate::ids::new_uuid());
        let checks = AtomicUsize::new(0);
        let published = AtomicBool::new(false);
        let result = adapter
            .start_fresh_runtime_transaction(
                created,
                None,
                || {
                    checks.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
                || {
                    assert_eq!(checks.load(Ordering::SeqCst), 2);
                    // The callback runs with the manager held after commit
                    // validation; it must not re-enter the manager.
                    assert!(adapter.inner.try_lock().is_err());
                    assert!(adapter.pane_input_index.read().unwrap().is_empty());
                    let deadline = switchy_time::Instant::now() + Duration::from_secs(5);
                    while !ready.exists() && switchy_time::Instant::now() < deadline {
                        switchy_thread::thread::sleep(Duration::from_millis(5));
                    }
                    assert!(ready.exists(), "shell must queue output before rollback");
                    // Let the bounded startup capture queue fill while the
                    // publication gate still excludes its projection reader.
                    switchy_thread::thread::sleep(Duration::from_millis(200));
                    published.store(true, Ordering::SeqCst);
                    anyhow::bail!("injected roster persistence failure")
                },
            )
            .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("roster persistence failure")
        );
        assert!(published.load(Ordering::SeqCst));
        let manager = adapter.inner.lock().unwrap();
        assert!(!manager.runtimes.contains_key(&created));
        assert_eq!(manager.runtimes.len(), 1);
        assert!(manager.pane_input_index.read().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[switchy_async::test]
    async fn fresh_session_options_reach_shell_and_recheck_authority() {
        use bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::InitialShellOptions;
        let host = crate::runtime::PaneRuntimeHost::detached();
        let host = &host;
        let existing = SessionId(crate::ids::new_uuid());
        let target = crate::ids::new_uuid();
        let manager = manager_with_runtime(existing, runtime_with_panes(&[target]));
        let adapter = ServerSessionRuntimeAdapter::new(Arc::new(Mutex::new(manager)));
        let session = SessionId(crate::ids::new_uuid());
        let directory = tempfile::tempdir().unwrap();
        let checks = AtomicUsize::new(0);
        adapter
            .start_fresh_runtime_with_options(
                session,
                Some(InitialShellOptions {
                    cwd: Some(directory.path().to_string_lossy().into_owned()),
                }),
                || {
                    checks.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .await
            .unwrap();
        assert_eq!(checks.load(Ordering::SeqCst), 2);
        let runtime = adapter
            .inner
            .lock()
            .unwrap()
            .runtimes
            .remove(&session)
            .unwrap();
        let pane = runtime.panes.into_values().next().unwrap();
        let reported = reported_shell_directory(&pane, directory.path()).await;
        shutdown_pane_handle(host, pane).await;
        assert_eq!(reported.unwrap(), directory.path().canonicalize().unwrap());
    }

    /// A caller whose deadline passes while the runtime starts gets
    /// `RuntimeStartPending`; the start goes on, and the next start of the
    /// session joins it rather than spawning a second first pane.
    #[cfg(unix)]
    #[switchy_async::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_start_outlasting_its_caller_finishes_and_is_joined() {
        use bmux_pane_runtime_state::SessionRuntimeManagerApi as _;
        let host = crate::runtime::PaneRuntimeHost::detached();
        let (exit_tx, _exit_rx) = mpsc::unbounded_channel();
        let manager = SessionRuntimeManager::new(
            PaneRuntimeHost::detached(),
            "/bin/sh".into(),
            "xterm-256color".into(),
            protocol_profile_for_term("xterm-256color"),
            true,
            None,
            PanePaddingConfig::default(),
            exit_tx,
        );
        let adapter = ServerSessionRuntimeAdapter::new(Arc::new(Mutex::new(manager)));
        let session = SessionId(crate::ids::new_uuid());
        let pending = adapter
            .start_runtime_within(session, Some(Duration::ZERO))
            .unwrap_err();
        assert_eq!(
            pending.downcast_ref::<bmux_pane_runtime_state::RuntimeStartPending>(),
            Some(&bmux_pane_runtime_state::RuntimeStartPending {
                session_id: session
            })
        );
        assert!(adapter.starts.lock().unwrap().contains_key(&session));
        // Joins the start in flight: a second start would fail "already exists".
        adapter
            .start_runtime_within(session, Some(Duration::from_secs(10)))
            .unwrap();
        assert!(adapter.session_exists(session));
        assert!(adapter.starts.lock().unwrap().is_empty());
        let removed = adapter.inner.lock().unwrap().remove_runtime(session).unwrap();
        switchy_async::time::timeout(
            Duration::from_secs(5),
            shutdown_runtime_handle(&host, removed),
        )
        .await
        .unwrap();
    }

    #[cfg(unix)]
    async fn reported_shell_directory(
        pane: &PaneRuntimeHandle,
        directory: &Path,
    ) -> Result<std::path::PathBuf> {
        let mut startup = pane.startup.clone();
        switchy_async::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(result) = startup.borrow_and_update().clone() {
                    break result.map_err(anyhow::Error::msg);
                }
                startup
                    .changed()
                    .await
                    .map_err(|_| anyhow::anyhow!("startup channel closed"))?;
            }
        })
        .await??;
        assert_eq!(pane.meta.resurrection.last_known_cwd, None);
        // The shell writes its real working directory, not an echoed argument.
        pane.input_tx
            .send(PaneRuntimeCommand::Input(b"pwd -P > actual-cwd\n".to_vec()))
            .unwrap();
        let result = switchy_async::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(value) = std::fs::read_to_string(directory.join("actual-cwd"))
                    && !value.trim().is_empty()
                {
                    break value;
                }
                switchy_async::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        Ok(std::path::PathBuf::from(result.trim()))
    }

    #[switchy_async::test]
    async fn restored_shell_metadata_is_healed_before_padding_rule_initial_size() {
        let host = crate::runtime::PaneRuntimeHost::detached();
        let host = &host;
        let configured_shell = if cfg!(windows) { "cmd.exe" } else { "/bin/sh" };
        let stale_shell = if cfg!(windows) {
            r"C:\definitely\missing\cmd.exe"
        } else {
            "/definitely/missing/sh"
        };
        let session_id = SessionId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let (pane_exit_tx, _pane_exit_rx) = mpsc::unbounded_channel();
        let settings: toml::Value = toml::from_str(&format!(
            "[padding]\n[[padding.pane_rules]]\nmatch_shell = {configured_shell:?}\nmax_content_width = 40"
        ))
        .expect("padding settings");
        let mut manager = SessionRuntimeManager::new(
            PaneRuntimeHost::detached(),
            configured_shell.to_string(),
            "xterm-256color".to_string(),
            ProtocolProfile::Bmux,
            true,
            None,
            PanePaddingConfig::parse(Some(&settings)).unwrap(),
            pane_exit_tx,
        );
        let pane = PaneRuntimeMeta {
            id: pane_id,
            name: None,
            shell: stale_shell.to_string(),
            launch: None,
            resurrection: PaneResurrectionSnapshot::default(),
        };
        let request = RestoreRuntimeRequest {
            focused_pane_id: pane_id,
            attach_viewport: Some(AttachViewport {
                cols: 120,
                rows: 40,
                top_inset: 0,
                right_inset: 0,
                bottom_inset: 0,
                left_inset: 0,
            }),
            ..RestoreRuntimeRequest::default()
        };

        manager
            .restore_runtime(session_id, &[pane], request)
            .expect("restore healed shell metadata");

        let restored = &manager.runtimes[&session_id].panes[&pane_id];
        assert_eq!(restored.meta.shell, configured_shell);
        assert_eq!(*restored.last_requested_size.lock().unwrap(), (38, 40));
        let removed = manager.remove_runtime(session_id).expect("remove runtime");
        shutdown_runtime_handle(host, removed).await;
    }

    #[switchy_async::test]
    async fn restored_override_sets_initial_pty_size_before_runtime_is_available() {
        let host = crate::runtime::PaneRuntimeHost::detached();
        let host = &host;
        let configured_shell = if cfg!(windows) { "cmd.exe" } else { "/bin/sh" };
        let session_id = SessionId(crate::ids::new_uuid());
        let pane_id = crate::ids::new_uuid();
        let (pane_exit_tx, _pane_exit_rx) = mpsc::unbounded_channel();
        let mut manager = SessionRuntimeManager::new(
            PaneRuntimeHost::detached(),
            configured_shell.to_string(),
            "xterm-256color".to_string(),
            ProtocolProfile::Bmux,
            true,
            None,
            PanePaddingConfig::default(),
            pane_exit_tx,
        );
        let override_spec = PanePaddingSpec {
            max_content_width: Some(40),
            horizontal_alignment: crate::padding::HorizontalAlignment::Center,
            ..PanePaddingSpec::default()
        };
        manager.install_restored_padding_override(session_id, pane_id, override_spec);
        let pane = PaneRuntimeMeta {
            id: pane_id,
            name: None,
            shell: configured_shell.to_string(),
            launch: None,
            resurrection: PaneResurrectionSnapshot::default(),
        };
        let request = RestoreRuntimeRequest {
            focused_pane_id: pane_id,
            attach_viewport: Some(AttachViewport {
                cols: 120,
                rows: 40,
                top_inset: 0,
                right_inset: 0,
                bottom_inset: 0,
                left_inset: 0,
            }),
            ..RestoreRuntimeRequest::default()
        };

        manager
            .restore_runtime(session_id, &[pane], request)
            .expect("restore runtime with padding override");

        let restored = &manager.runtimes[&session_id].panes[&pane_id];
        assert_eq!(restored.padding_override, Some(override_spec));
        assert_eq!(*restored.last_requested_size.lock().unwrap(), (38, 40));
        let removed = manager.remove_runtime(session_id).expect("remove runtime");
        shutdown_runtime_handle(host, removed).await;
    }

    #[switchy_async::test]
    async fn multi_session_restore_heals_stale_shells_in_persistence_snapshot() {
        let configured_shell = if cfg!(windows) { "cmd.exe" } else { "/bin/sh" };
        let stale_shell = if cfg!(windows) {
            r"C:\definitely\missing\cmd.exe"
        } else {
            "/definitely/missing/sh"
        };
        let (pane_exit_tx, _pane_exit_rx) = mpsc::unbounded_channel();
        let mut manager = SessionRuntimeManager::new(
            PaneRuntimeHost::detached(),
            configured_shell.to_string(),
            "xterm-256color".to_string(),
            ProtocolProfile::Bmux,
            true,
            None,
            PanePaddingConfig::default(),
            pane_exit_tx,
        );
        let session_ids = [
            SessionId(crate::ids::new_uuid()),
            SessionId(crate::ids::new_uuid()),
        ];
        let mut expected_pane_ids = BTreeMap::new();

        for session_id in session_ids {
            let pane_ids = [crate::ids::new_uuid(), crate::ids::new_uuid()];
            let panes = pane_ids.map(|id| PaneRuntimeMeta {
                id,
                name: None,
                shell: stale_shell.to_string(),
                launch: None,
                resurrection: PaneResurrectionSnapshot {
                    execution_token: None,
                    active_command: None,
                    active_command_source: None,
                    last_known_cwd: Some(std::env::temp_dir().to_string_lossy().into_owned()),
                },
            });
            manager
                .restore_runtime(
                    session_id,
                    &panes,
                    RestoreRuntimeRequest::with_focus(pane_ids[0]),
                )
                .expect("restore stale pane metadata");
            expected_pane_ids.insert(session_id, pane_ids);
        }

        let manager = Arc::new(Mutex::new(manager));
        let handle = bmux_pane_runtime_state::SessionRuntimeManagerHandle::new(
            ServerSessionRuntimeAdapter::new(Arc::clone(&manager)),
        );
        for session_id in session_ids {
            let snapshot = handle
                .0
                .snapshot_session_runtime_for_persistence(session_id)
                .expect("build persistence snapshot")
                .expect("restored session runtime");
            let expected = expected_pane_ids[&session_id];
            assert_eq!(snapshot.panes.len(), expected.len());
            assert!(
                snapshot
                    .panes
                    .iter()
                    .all(|pane| expected.contains(&pane.id) && pane.shell == configured_shell)
            );
        }

        let removed = manager.lock().expect("manager lock").remove_all_runtimes();
        for runtime in removed {
            shutdown_runtime_handle(&crate::runtime::PaneRuntimeHost::detached(), runtime).await;
        }
    }

    #[test]
    fn pane_input_handle_rejects_exited_pane() {
        let (input_tx, _input_rx) = mpsc::unbounded_channel();
        let handle = PaneInputHandle {
            execution_token: None,
            host: crate::runtime::PaneRuntimeHost::detached(),
            session_id: SessionId(crate::ids::new_uuid()),
            input_tx,
            exited: Arc::new(AtomicBool::new(true)),
        };

        assert_eq!(
            handle.send_input(b"ignored".to_vec()),
            Err(SessionRuntimeError::Closed)
        );
    }

    #[test]
    fn output_cursor_replays_bytes_withheld_while_frozen() {
        let client_id = ClientId(crate::ids::new_uuid());
        let mut output = OutputFanoutBuffer::new(4096);
        output.register_client_at_tail(client_id);
        output.push_chunk(b"before-freeze");

        // A frozen pane is omitted from output-batch requests, so its cursor is
        // deliberately not read or advanced while these bytes arrive.
        output.push_chunk(b"-during-freeze");
        let replay = output.read_for_client(client_id, 4096);

        assert_eq!(replay.bytes, b"before-freeze-during-freeze");
        assert!(!replay.stream_gap);
        assert_eq!(replay.stream_end, output.end_offset());
    }

    #[test]
    fn output_cursor_reports_gap_when_frozen_replay_exceeds_retention() {
        let client_id = ClientId(crate::ids::new_uuid());
        let mut output = OutputFanoutBuffer::new(8);
        output.register_client_at_tail(client_id);
        output.push_chunk(b"more-than-eight-bytes");

        let replay = output.read_for_client(client_id, 4096);

        assert!(replay.stream_gap);
        assert!(replay.stream_start >= output.start_offset);
    }

    #[test]
    fn concurrent_snapshot_commits_preserve_newest_cursors() {
        let client = ClientId(crate::ids::new_uuid());
        let outputs = (0..3)
            .map(|_| {
                let mut output = OutputFanoutBuffer::new(1024);
                output.register_client_at_tail(client);
                output.push_chunk(b"0123456789");
                (
                    crate::ids::new_uuid(),
                    Arc::new(switchy_thread::sync::Mutex::new(output)),
                )
            })
            .collect::<Vec<_>>();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let workers = [false, true].map(|reverse| {
            let mut updates = outputs
                .iter()
                .map(|(pane, output)| (*pane, Arc::clone(output), if reverse { 9 } else { 4 }))
                .collect::<Vec<_>>();
            if reverse {
                updates.reverse();
            }
            let barrier = Arc::clone(&barrier);
            switchy_thread::thread::spawn(move || {
                barrier.wait();
                acknowledge_grid_snapshots(client, &mut updates).unwrap();
            })
        });
        for worker in workers {
            worker.join().unwrap();
        }
        for (_, output) in &outputs {
            assert_eq!(output.lock().unwrap().cursors.get(&client), Some(&9));
        }
        // A snapshot captured earlier may finish encoding after both commits.
        let mut stale = outputs
            .iter()
            .map(|(pane, output)| (*pane, Arc::clone(output), 2))
            .collect::<Vec<_>>();
        acknowledge_grid_snapshots(client, &mut stale).unwrap();
        for (_, output) in outputs {
            assert_eq!(output.lock().unwrap().cursors.get(&client), Some(&9));
        }
    }

    #[test]
    fn snapshot_cursor_commit_rejects_aliased_outputs_without_mutation() {
        let client = ClientId(crate::ids::new_uuid());
        let mut output = OutputFanoutBuffer::new(1024);
        output.register_client_at_tail(client);
        output.push_chunk(b"hello");
        let output = Arc::new(switchy_thread::sync::Mutex::new(output));
        // The same output under two panes: still aliased.
        let mut updates = [
            (crate::ids::new_uuid(), Arc::clone(&output), 2),
            (crate::ids::new_uuid(), Arc::clone(&output), 5),
        ];
        assert_eq!(
            acknowledge_grid_snapshots(client, &mut updates),
            Err(SessionRuntimeError::Closed)
        );
        assert_eq!(output.lock().unwrap().cursors.get(&client), Some(&0));
        acknowledge_grid_snapshots(client, &mut updates[..1]).unwrap();
        assert_eq!(
            output.lock().unwrap().cursors.get(&client),
            Some(&updates[0].2)
        );
    }

    #[test]
    fn snapshot_cursor_commit_is_atomic_on_poisoned_output() {
        let client = ClientId(crate::ids::new_uuid());
        let mut updates = (0..2)
            .map(|_| {
                let mut output = OutputFanoutBuffer::new(1024);
                output.register_client_at_tail(client);
                output.push_chunk(b"hello");
                (
                    crate::ids::new_uuid(),
                    Arc::new(switchy_thread::sync::Mutex::new(output)),
                    5,
                )
            })
            .collect::<Vec<_>>();
        updates.sort_by_key(|(pane, _, _)| *pane);
        let poisoned = Arc::clone(&updates[1].1);
        assert!(
            switchy_thread::thread::spawn(move || {
                let _guard = poisoned.lock().unwrap();
                panic!("poison output for atomic commit test");
            })
            .join()
            .is_err()
        );
        assert_eq!(
            acknowledge_grid_snapshots(client, &mut updates),
            Err(SessionRuntimeError::Closed)
        );
        assert_eq!(updates[0].1.lock().unwrap().cursors.get(&client), Some(&0));
    }

    #[test]
    fn snapshot_acknowledgement_preserves_eviction_gap() {
        let client_id = ClientId(crate::ids::new_uuid());
        let mut output = OutputFanoutBuffer::new(8);
        output.register_client_at_tail(client_id);
        output.push_chunk(b"first");
        let watermark = output.end_offset();
        output.push_chunk(b"0123456789abcdef");
        assert!(output.start_offset > watermark);
        output.acknowledge_snapshot(client_id, watermark);
        assert_eq!(output.cursors.get(&client_id), Some(&watermark));
        let read = output.read_for_client(client_id, 1024);
        assert!(read.stream_gap);
        assert_eq!(read.bytes, b"89abcdef");
        assert!(!output.read_for_client(client_id, 1024).stream_gap);
    }

    #[test]
    fn snapshot_acknowledgement_preserves_other_clients_unread_output() {
        let first = ClientId(crate::ids::new_uuid());
        let second = ClientId(crate::ids::new_uuid());
        let mut output = OutputFanoutBuffer::new(1024);
        output.register_client_at_tail(first);
        output.register_client_at_tail(second);
        output.push_chunk(b"before");
        let watermark = output.end_offset();
        output.push_chunk(b"after");
        output.acknowledge_snapshot(first, watermark);
        assert_eq!(output.read_for_client(first, 1024).bytes, b"after");
        assert_eq!(output.read_for_client(second, 1024).bytes, b"beforeafter");
        output.unregister_client(first);
        output.push_chunk(b"more");
        output.acknowledge_snapshot(first, output.end_offset());
        assert_eq!(output.read_for_client(second, 1024).bytes, b"more");
        assert!(!output.cursors.contains_key(&first));
    }

    #[test]
    fn snapshot_acknowledgement_does_not_recreate_detached_cursor() {
        let client_id = ClientId(crate::ids::new_uuid());
        let mut output = OutputFanoutBuffer::new(1024);
        output.register_client_at_tail(client_id);
        output.push_chunk(b"first");
        let snapshot_end = output.end_offset();
        output.unregister_client(client_id);
        output.acknowledge_snapshot(client_id, snapshot_end);
        assert!(!output.cursors.contains_key(&client_id));
        output.push_chunk(b"second");
        output.register_client_at_tail(client_id);
        output.acknowledge_snapshot(client_id, snapshot_end);
        assert_eq!(output.cursors.get(&client_id), Some(&output.end_offset()));
        assert_eq!(output.read_for_client(client_id, 1024).bytes, [] as [u8; 0]);
    }

    #[test]
    fn snapshot_acknowledgement_does_not_rewind_concurrent_replay() {
        let client_id = ClientId(crate::ids::new_uuid());
        let mut output = OutputFanoutBuffer::new(1024);
        output.register_client_at_tail(client_id);
        output.push_chunk(b"first");
        let snapshot_end = output.end_offset();
        output.push_chunk(b"second");
        assert_eq!(
            output.read_for_client(client_id, 1024).bytes,
            b"firstsecond"
        );
        output.acknowledge_snapshot(client_id, snapshot_end);
        assert_eq!(output.read_for_client(client_id, 1024).bytes, [] as [u8; 0]);
        output.push_chunk(b"third");
        assert_eq!(output.read_for_client(client_id, 1024).bytes, b"third");
        // Explicit reset remains available for intentional replay.
        output.set_client_cursor(client_id, 0);
        output.acknowledge_snapshot(client_id, snapshot_end);
        assert_eq!(
            output.read_for_client(client_id, 1024).bytes,
            b"secondthird"
        );
    }

    #[test]
    fn read_recent_never_starts_mid_csi() {
        let mut buf = OutputFanoutBuffer::new(65_536);

        let mut chunk1 = vec![b' '; 1020];
        chunk1.extend_from_slice(b"\x1b[48;");
        assert_eq!(chunk1.len(), 1025);

        let mut chunk2 = Vec::new();
        chunk2.extend_from_slice(b"2;10;10;10m");
        chunk2.extend_from_slice(&vec![b' '; 1013]);
        assert_eq!(chunk2.len(), 1024);

        buf.push_chunk(&chunk1);
        buf.push_chunk(&chunk2);

        let result = buf.read_recent_with_offsets(chunk2.len()).bytes;

        assert!(
            !result.starts_with(b"2;10;10;10m"),
            "read_recent returned data starting mid-escape-sequence: {:?}",
            String::from_utf8_lossy(&result[..20.min(result.len())])
        );
        if !result.is_empty() {
            assert_eq!(result[0], b' ');
        }
    }

    #[test]
    fn read_recent_budget_smaller_than_buffer_skips_mid_seq_start() {
        let mut buf = OutputFanoutBuffer::new(4096);
        let payload = b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAaaaa\x1b[38;2;128;128;128mBBBBBBBBBBBBBBBBBB";
        buf.push_chunk(payload);

        let result = buf.read_recent_with_offsets(30).bytes;
        assert_eq!(result, b"BBBBBBBBBBBBBBBBBB");
    }

    #[cfg(unix)]
    #[test]
    fn process_signal_targets_group_with_named_signal() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let capture = directory.path().join("capture");
        let script = directory.path().join("kill");
        std::fs::write(
            &script,
            format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\n", capture.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        send_process_signal_command(
            script.to_str().unwrap(),
            &bmux_pane_runtime_state::PaneProcessIdentity {
                session_id: SessionId(Uuid::nil()),
                pane_id: Uuid::nil(),
                pid: Some(10),
                process_group_id: Some(42),
            },
            bmux_pane_runtime_state::PaneProcessSignal::Interrupt,
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(capture).unwrap(), "-INT\n--\n-42\n");
    }

    /// Output is retained only while a reader asks for bytes: without one, chunks only advance
    /// the offsets (a returning reader is told of the gap); retention resumes at the first
    /// boundary between sequences, with the synchronized-update state the grid had there.
    #[test]
    fn output_is_retained_only_while_bytes_are_read() {
        let client = ClientId(crate::ids::new_uuid());
        let mut output = OutputFanoutBuffer::new(1024);
        output.register_client_at_tail(client);
        let between = |sync_update_open| ChunkBoundary {
            between_sequences: true,
            sync_update_open,
        };
        let stale = || Instant::now().checked_sub(RAW_READ_LEASE * 2).unwrap();
        output.last_read.set(stale());
        output.push_parsed(b"\x1b[?2026hnobody", between(true));
        assert!(output.data.is_empty(), "no reader: nothing is kept");
        assert_eq!(output.end_offset(), 14);

        // A reader returns: it is told of the gap, and output is kept again from the next chunk,
        // the update open as the grid had it.
        let read = output.read_for_client(client, 64);
        assert!(read.stream_gap);
        output.push_parsed(b"kept", between(true));
        let read = output.read_for_client(client, 64);
        assert_eq!(read.bytes, b"kept");
        assert!(!read.stream_gap);
        assert!(output.sync_update_open_at(read.stream_end));

        // Retention stops without readers, and resumes only where the grid was between
        // sequences.
        output.last_read.set(stale());
        output.push_parsed(
            b"\x1b[3",
            ChunkBoundary {
                between_sequences: false,
                sync_update_open: true,
            },
        );
        output.note_read(1);
        output.push_parsed(b"1mred", between(false));
        assert!(output.data.is_empty(), "the chunk began mid-sequence");
        output.push_parsed(b"plain", between(false));
        assert_eq!(output.read_recent_with_offsets(64).bytes, b"plain");
        assert!(!output.sync_update_open_at(output.end_offset()));
    }

    #[test]
    fn node_wide_output_cap_accounts_and_evicts_across_buffers() {
        let retained = Arc::new(AtomicUsize::new(0));
        let mut first = OutputFanoutBuffer::with_accounting(usize::MAX, Arc::clone(&retained));
        let mut second = OutputFanoutBuffer::with_accounting(usize::MAX, Arc::clone(&retained));
        first.push_chunk_with_node_cap(b"abcd", 4);
        assert_eq!(retained.load(Ordering::SeqCst), 4);
        second.push_chunk_with_node_cap(b"bc", 4);
        assert_eq!(retained.load(Ordering::SeqCst), 4);
        assert!(second.read_at(0, 2).stream_gap);
        assert_eq!(second.start_offset, 2);
        drop(first);
        assert_eq!(retained.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn batched_output_eviction_preserves_wrapped_tail_and_accounting() {
        let retained = Arc::new(AtomicUsize::new(0));
        let mut output = OutputFanoutBuffer::with_accounting(8, Arc::clone(&retained));
        for chunk in [b"abcdefgh".as_slice(), b"ijkl", b"mnopqrstuvwxyz"] {
            output.push_chunk_with_node_cap(chunk, 8);
            assert_eq!(retained.load(Ordering::SeqCst), output.data.len());
            assert!(output.data.len() <= 8);
        }
        assert_eq!(output.start_offset, 18);
        assert_eq!(output.end_offset(), 26);
        let read = output.read_at(0, 100);
        assert!(read.stream_gap);
        assert_eq!(read.bytes, b"stuvwxyz");
        drop(output);
        assert_eq!(retained.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn batched_output_eviction_handles_concurrent_node_pressure() {
        let retained = Arc::new(AtomicUsize::new(0));
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let retained = Arc::clone(&retained);
                scope.spawn(move || {
                    let mut output = OutputFanoutBuffer::with_accounting(1024, retained);
                    for _ in 0..100 {
                        output.push_chunk_with_node_cap(&[b'x'; 4096], 2048);
                        assert!(output.data.len() <= 1024);
                    }
                });
            }
        });
        assert_eq!(retained.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn competing_output_batches_retain_node_budget_without_duplicate_eviction() {
        for _ in 0..64 {
            let retained = Arc::new(AtomicUsize::new(0));
            let barrier = std::sync::Barrier::new(2);
            let (first, second) = std::thread::scope(|scope| {
                let build = |byte| {
                    let mut output =
                        OutputFanoutBuffer::with_accounting(2048, Arc::clone(&retained));
                    output.push_chunk_with_node_cap(&[byte; 512], usize::MAX);
                    barrier.wait();
                    output.push_chunk_with_node_cap(&[byte; 512], 1536);
                    output
                };
                let first = scope.spawn(move || build(b'a'));
                let second = scope.spawn(move || build(b'b'));
                (first.join().unwrap(), second.join().unwrap())
            });
            assert_eq!(retained.load(Ordering::SeqCst), 1536);
            assert_eq!(first.data.len() + second.data.len(), 1536);
            assert_eq!(first.end_offset(), 1024);
            assert_eq!(second.end_offset(), 1024);
            assert!(first.data.iter().all(|&byte| byte == b'a'));
            assert!(second.data.iter().all(|&byte| byte == b'b'));
            drop((first, second));
            assert_eq!(retained.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn absolute_output_read_reports_retention_and_source_bounds() {
        let mut output = OutputFanoutBuffer::new(4);
        output.push_chunk(b"abcdef");
        let repaired = output.read_at(0, 2);
        assert!(repaired.stream_gap);
        assert_eq!(repaired.stream_start, 2);
        assert_eq!(repaired.stream_end, 4);
        assert_eq!(repaired.bytes, b"cd");
        let bounded = output.read_at(4, 1);
        assert!(!bounded.stream_gap);
        assert_eq!(bounded.stream_start, 4);
        assert_eq!(bounded.stream_end, 5);
        assert_eq!(bounded.bytes, b"e");
        assert_eq!(output.start_offset, 2);
        assert_eq!(output.end_offset(), 6);
    }

    #[test]
    fn delayed_snapshot_acknowledgement_recovers_after_split_escape() {
        let client = ClientId(crate::ids::new_uuid());
        let mut output = OutputFanoutBuffer::new(8);
        output.register_client_at_tail(client);
        output.push_chunk(b"old");
        let watermark = output.end_offset();
        output.push_chunk(b"\x1b[48;2;10;");
        output.push_chunk(b"10;10mOK");
        assert!(output.start_offset > watermark);
        output.acknowledge_snapshot(client, watermark);
        let read = output.read_for_client(client, 1024);
        assert!(read.stream_gap);
        assert_eq!(read.bytes, b"OK");
        assert_eq!(read.stream_start, output.end_offset() - 2);
        assert_eq!(read.stream_end, output.end_offset());
        output.push_chunk(b"next");
        let next = output.read_for_client(client, 1024);
        assert!(!next.stream_gap);
        assert_eq!(next.bytes, b"next");
    }

    #[test]
    fn reads_report_synchronized_updates_at_their_own_end() {
        let mut buf = OutputFanoutBuffer::new(1 << 16);
        let client = ClientId(crate::ids::new_uuid());
        buf.register_client_at_tail(client);
        // Frame one complete, frame two begun, with its opening split across chunks.
        buf.push_chunk(b"\x1b[?2026hone\x1b[?2026l\x1b[?20");
        buf.push_chunk(b"26htwo");
        let first = buf.read_for_client(client, 3 + 8 + 8);
        assert!(
            !buf.sync_update_open_at(first.stream_end),
            "frame one is complete"
        );
        let rest = buf.read_for_client(client, 1024);
        assert!(
            buf.sync_update_open_at(rest.stream_end),
            "frame two is open"
        );
        assert!(buf.sync_update_open_at(8), "inside frame one");
        // Evicted marks fold into the state before the retained stream.
        let mut small = OutputFanoutBuffer::new(8);
        small.push_chunk(b"\x1b[?2026hpadding-padding");
        assert!(small.sync_update_open_at(small.end_offset()));
    }

    #[test]
    fn read_for_client_gap_advances_to_ground_boundary() {
        let mut buf = OutputFanoutBuffer::new(64);
        let client = ClientId(crate::ids::new_uuid());
        buf.register_client_at_tail(client);

        buf.push_chunk(b"\x1b[31mhello");

        let mut big_chunk = Vec::new();
        big_chunk.extend_from_slice(b"\x1b[48;");
        big_chunk.extend_from_slice(b"2;10;10;10m");
        big_chunk.extend_from_slice(b"safe text here.");
        big_chunk.resize(80, b' ');
        buf.push_chunk(&big_chunk);

        let read = buf.read_for_client(client, 1024);
        assert!(
            !read.bytes.starts_with(b"2;10;10;10m"),
            "gap read returned mid-sequence data: {:?}",
            String::from_utf8_lossy(&read.bytes[..20.min(read.bytes.len())])
        );
        if !read.bytes.is_empty() {
            assert!(read.bytes[0] == b's' || read.bytes[0] == b' ');
        }
    }

    #[test]
    fn esc_spans_pruned_on_eviction() {
        let mut buf = OutputFanoutBuffer::new(32);

        buf.push_chunk(b"\x1b[mA\x1b[mB\x1b[mC");
        let pre_evict_count = buf.esc_spans.len();
        assert!(pre_evict_count >= 3);

        buf.push_chunk(&[b'X'; 40]);

        for &(_esc_start, safe_resume) in &buf.esc_spans {
            assert!(safe_resume == u64::MAX || safe_resume > buf.start_offset);
        }
    }
}
