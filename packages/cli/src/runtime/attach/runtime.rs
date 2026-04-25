use anyhow::{Context, Result};
use bmux_attach_layout_protocol::attach_layout_protocol::{
    AttachLayoutSnapshot, AttachSurfaceSummary, STATE_KIND as ATTACH_LAYOUT_STATE_KIND,
};
use bmux_attach_pipeline::mouse as attach_mouse;
use bmux_attach_pipeline::reconcile::apply_attach_output_chunk_with;
use bmux_attach_pipeline::{AttachChunkApplyOutcome, AttachOutputChunkMeta};
use bmux_client::{
    AttachLayoutState, AttachPaneSnapshotState, AttachSnapshotState, ClientError,
    StreamingBmuxClient,
};
use bmux_config::{BmuxConfig, ConfigPaths, PaneRestoreMethod, ResolvedTimeout, StatusPosition};
use bmux_ipc::{
    AttachViewComponent, CAPABILITY_ATTACH_PANE_SNAPSHOT, ContextSelector,
    ContextSessionBindingSummary, ContextSummary, ControlCatalogSnapshot, InvokeServiceKind,
    PaneFocusDirection, PaneSplitDirection, SessionSelector, SessionSummary,
};
use bmux_keybind::{action_to_config_name, parse_action};
use bmux_plugin_sdk::{HostScope, PluginCommandOutcome, ServiceKind, ServiceRequest};
use crossterm::cursor::{Hide, MoveTo, SavePosition, Show};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::queue;
use crossterm::style::Print;
use crossterm::terminal;
use crossterm::terminal::{BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::{debug, trace, warn};
use uuid::Uuid;

use super::super::prompt::{self, PromptRequest, PromptResponse, PromptValue};
use super::super::{
    ATTACH_SCROLLBACK_UNAVAILABLE_STATUS, ATTACH_SELECTION_CLEARED_STATUS,
    ATTACH_SELECTION_COPIED_STATUS, ATTACH_SELECTION_EMPTY_STATUS, ATTACH_SELECTION_STARTED_STATUS,
    ATTACH_SNAPSHOT_MAX_BYTES_PER_PANE, ATTACH_TRANSIENT_STATUS_TTL, ATTACH_WELCOME_STATUS_TTL,
    BmuxClient, HELP_OVERLAY_SURFACE_ID, InputProcessor, KernelClientFactory, Keymap,
    RuntimeAction, action_dispatch, attach, attach_quit_failure_status,
    available_capability_providers, available_service_descriptors, command_accepts_repeat,
    effective_enabled_plugins, enter_host_kernel_connection, host_kernel_bridge, load_plugin,
    map_attach_client_error, merged_runtime_keybindings, parse_session_selector, parse_uuid_value,
    plugin_command_policy_hints, plugin_host_metadata, recording, resolve_plugin_search_paths,
    run_plugin_keybinding_command, scan_available_plugins,
};
use super::cursor::apply_attach_cursor_state;
use super::events::{AttachLoopControl, AttachLoopEvent};
use super::prompt_ui::{
    AttachInternalPromptAction, AttachPromptCompletion, AttachPromptOrigin, AttachPromptPreview,
    PromptKeyDisposition, prompt_accepts_key_kind,
};
use super::render::{
    AttachLayer, AttachLayerSurface, append_pane_output, opaque_row_text, queue_layer_fill,
    render_attach_scene, visible_scene_pane_ids,
};
use super::state::{
    AttachEventAction, AttachExitReason, AttachScrollbackCursor, AttachScrollbackPosition,
    AttachUiMode, AttachViewState, PaneRect, PaneRenderBuffer,
};
use crate::status::{AttachStatusLine, AttachTab, build_attach_status_line};

const ATTACH_OUTPUT_BATCH_MAX_BYTES: usize = 8 * 1024;
const ATTACH_OUTPUT_DRAIN_MAX_ROUNDS: usize = 8;
/// Maximum wall-clock time the drain loop may spend waiting for an in-
/// progress output burst to complete (e.g. when the server indicates
/// `output_still_pending` or the inner application is mid-synchronized-
/// update).  Each IPC round-trip (~50-200 µs on a local Unix socket)
/// naturally yields CPU time to the PTY reader thread, so no explicit
/// sleep/yield is needed between rounds.
const ATTACH_OUTPUT_DRAIN_TIME_BUDGET: Duration = Duration::from_millis(4);

use super::super::{typed_clients, typed_contexts, typed_sessions, typed_windows};

/// Typed dispatch wrapper for `sessions-commands:kill-session`.
async fn typed_kill_session_attach(
    client: &mut StreamingBmuxClient,
    selector: bmux_ipc::SessionSelector,
    force_local: bool,
) -> std::result::Result<
    std::result::Result<
        bmux_sessions_plugin_api::sessions_commands::SessionAck,
        bmux_sessions_plugin_api::sessions_commands::KillSessionError,
    >,
    ClientError,
> {
    let args = typed_sessions::KillSessionArgs {
        selector: typed_sessions::from_ipc_selector(selector),
        force_local,
    };
    let payload = bmux_codec::to_vec(&args).map_err(|error| ClientError::ServerError {
        code: bmux_ipc::ErrorCode::Internal,
        message: format!("encoding kill-session args: {error}"),
    })?;
    let response_bytes = client
        .invoke_service_raw(
            typed_sessions::SESSIONS_WRITE_CAPABILITY.as_str(),
            typed_sessions::COMMAND_KIND,
            typed_sessions::SESSIONS_COMMANDS_INTERFACE.as_str(),
            typed_sessions::OP_KILL_SESSION,
            payload,
        )
        .await?;
    bmux_codec::from_bytes(&response_bytes).map_err(|error| ClientError::ServerError {
        code: bmux_ipc::ErrorCode::Internal,
        message: format!("decoding kill-session response: {error}"),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ApplyDecorationThemeExtensionArgs {
    toml: String,
    config_dir_candidates: Vec<String>,
}

async fn apply_plugin_theme_extensions(
    client: &mut StreamingBmuxClient,
    theme: bmux_config::ThemeConfig,
    plugin_ids: &[String],
    config_dir_candidates: &[String],
) {
    for plugin_id in plugin_ids {
        let toml = theme
            .plugins
            .get(plugin_id)
            .and_then(|extension| toml::to_string(extension).ok())
            .unwrap_or_default();
        let args = ApplyDecorationThemeExtensionArgs {
            toml,
            config_dir_candidates: config_dir_candidates.to_vec(),
        };
        let Ok(payload) = bmux_codec::to_vec(&args) else {
            continue;
        };
        let capability = format!("{plugin_id}.write");
        let _ = client
            .invoke_service_raw(
                &capability,
                InvokeServiceKind::Command,
                "theme-extension",
                "apply",
                payload,
            )
            .await;
    }
}

/// Typed dispatch wrapper for `contexts-state:list-contexts`.
async fn typed_list_contexts_attach(
    client: &mut StreamingBmuxClient,
) -> std::result::Result<Vec<bmux_contexts_plugin_api::contexts_state::ContextSummary>, ClientError>
{
    let payload = bmux_codec::to_vec(&()).map_err(|error| ClientError::ServerError {
        code: bmux_ipc::ErrorCode::Internal,
        message: format!("encoding list-contexts args: {error}"),
    })?;
    let response_bytes = client
        .invoke_service_raw(
            typed_contexts::CONTEXTS_READ_CAPABILITY.as_str(),
            typed_contexts::QUERY_KIND,
            typed_contexts::CONTEXTS_STATE_INTERFACE.as_str(),
            typed_contexts::OP_LIST_CONTEXTS,
            payload,
        )
        .await?;
    bmux_codec::from_bytes(&response_bytes).map_err(|error| ClientError::ServerError {
        code: bmux_ipc::ErrorCode::Internal,
        message: format!("decoding list-contexts response: {error}"),
    })
}

/// Typed dispatch wrapper for `windows-state:list-windows`.
///
/// Pulls the windows-plugin's current ordered window list via the
/// server's `InvokeService` router. The windows-plugin's state channel
/// (`windows-list`) is published on the server's local event bus and
/// forwarded to streaming clients via `Event::PluginBusEvent`, but the
/// forwarder's initial emit happens before any client connects — the
/// broadcast has zero subscribers at that moment and the message is
/// lost. We pull on attach startup to seed our local state channel;
/// subsequent mutations arrive as `PluginBusEvent`s and drive updates
/// through the normal subscription path. This matches the
/// `ControlCatalogSnapshot` pull-on-connect pattern.
async fn typed_list_windows_attach(
    client: &mut StreamingBmuxClient,
) -> std::result::Result<Vec<bmux_windows_plugin_api::windows_state::WindowEntry>, ClientError> {
    #[derive(serde::Serialize)]
    struct Args {
        #[serde(skip_serializing_if = "Option::is_none")]
        session: Option<String>,
    }
    let payload =
        bmux_codec::to_vec(&Args { session: None }).map_err(|error| ClientError::ServerError {
            code: bmux_ipc::ErrorCode::Internal,
            message: format!("encoding list-windows args: {error}"),
        })?;
    let response_bytes = client
        .invoke_service_raw(
            bmux_windows_plugin_api::capabilities::WINDOWS_READ.as_str(),
            bmux_ipc::InvokeServiceKind::Query,
            bmux_windows_plugin_api::windows_state::INTERFACE_ID.as_str(),
            "list-windows",
            payload,
        )
        .await?;
    bmux_codec::from_bytes(&response_bytes).map_err(|error| ClientError::ServerError {
        code: bmux_ipc::ErrorCode::Internal,
        message: format!("decoding list-windows response: {error}"),
    })
}

/// Typed dispatch wrapper for `contexts-state:current-context`.
async fn typed_current_context_attach(
    client: &mut StreamingBmuxClient,
) -> std::result::Result<
    Option<bmux_contexts_plugin_api::contexts_state::ContextSummary>,
    ClientError,
> {
    let payload = bmux_codec::to_vec(&()).map_err(|error| ClientError::ServerError {
        code: bmux_ipc::ErrorCode::Internal,
        message: format!("encoding current-context args: {error}"),
    })?;
    let response_bytes = client
        .invoke_service_raw(
            typed_contexts::CONTEXTS_READ_CAPABILITY.as_str(),
            typed_contexts::QUERY_KIND,
            typed_contexts::CONTEXTS_STATE_INTERFACE.as_str(),
            typed_contexts::OP_CURRENT_CONTEXT,
            payload,
        )
        .await?;
    bmux_codec::from_bytes(&response_bytes).map_err(|error| ClientError::ServerError {
        code: bmux_ipc::ErrorCode::Internal,
        message: format!("decoding current-context response: {error}"),
    })
}

/// Typed dispatch wrapper for `clients-state:list-clients` on a plain
/// [`BmuxClient`] (used before streaming upgrade).
async fn typed_list_clients_bmux(
    client: &mut BmuxClient,
) -> std::result::Result<Vec<bmux_clients_plugin_api::clients_state::ClientSummary>, ClientError> {
    let payload = bmux_codec::to_vec(&()).map_err(|error| ClientError::ServerError {
        code: bmux_ipc::ErrorCode::Internal,
        message: format!("encoding list-clients args: {error}"),
    })?;
    let response_bytes = client
        .invoke_service_raw(
            typed_clients::CLIENTS_READ_CAPABILITY.as_str(),
            typed_clients::QUERY_KIND,
            typed_clients::CLIENTS_STATE_INTERFACE.as_str(),
            typed_clients::OP_LIST_CLIENTS,
            payload,
        )
        .await?;
    bmux_codec::from_bytes(&response_bytes).map_err(|error| ClientError::ServerError {
        code: bmux_ipc::ErrorCode::Internal,
        message: format!("decoding list-clients response: {error}"),
    })
}

/// Typed dispatch wrapper for `contexts-state:list-contexts` on a
/// plain [`BmuxClient`].
async fn typed_list_contexts_bmux(
    client: &mut BmuxClient,
) -> std::result::Result<Vec<bmux_contexts_plugin_api::contexts_state::ContextSummary>, ClientError>
{
    let payload = bmux_codec::to_vec(&()).map_err(|error| ClientError::ServerError {
        code: bmux_ipc::ErrorCode::Internal,
        message: format!("encoding list-contexts args: {error}"),
    })?;
    let response_bytes = client
        .invoke_service_raw(
            typed_contexts::CONTEXTS_READ_CAPABILITY.as_str(),
            typed_contexts::QUERY_KIND,
            typed_contexts::CONTEXTS_STATE_INTERFACE.as_str(),
            typed_contexts::OP_LIST_CONTEXTS,
            payload,
        )
        .await?;
    bmux_codec::from_bytes(&response_bytes).map_err(|error| ClientError::ServerError {
        code: bmux_ipc::ErrorCode::Internal,
        message: format!("decoding list-contexts response: {error}"),
    })
}

/// Typed dispatch wrapper for `contexts-commands:select-context`.
async fn typed_select_context_attach(
    client: &mut StreamingBmuxClient,
    context_id: uuid::Uuid,
) -> std::result::Result<(), ClientError> {
    #[derive(serde::Serialize)]
    struct Selector {
        id: Option<uuid::Uuid>,
        name: Option<String>,
    }
    #[derive(serde::Serialize)]
    struct Args {
        selector: Selector,
    }
    let args = Args {
        selector: Selector {
            id: Some(context_id),
            name: None,
        },
    };
    let payload = bmux_codec::to_vec(&args).map_err(|error| ClientError::ServerError {
        code: bmux_ipc::ErrorCode::Internal,
        message: format!("encoding select-context args: {error}"),
    })?;
    let response_bytes = client
        .invoke_service_raw(
            typed_contexts::CONTEXTS_WRITE_CAPABILITY.as_str(),
            typed_contexts::COMMAND_KIND,
            typed_contexts::CONTEXTS_COMMANDS_INTERFACE.as_str(),
            typed_contexts::OP_SELECT_CONTEXT,
            payload,
        )
        .await?;
    let outcome: std::result::Result<
        bmux_contexts_plugin_api::contexts_commands::ContextAck,
        bmux_contexts_plugin_api::contexts_commands::SelectContextError,
    > = bmux_codec::from_bytes(&response_bytes).map_err(|error| ClientError::ServerError {
        code: bmux_ipc::ErrorCode::Internal,
        message: format!("decoding select-context response: {error}"),
    })?;
    outcome.map(|_| ()).map_err(|err| ClientError::ServerError {
        code: bmux_ipc::ErrorCode::Internal,
        message: format!("select-context failed: {err:?}"),
    })
}

/// Invoke a `windows-commands` typed command by routing through the
/// server's generic `Request::InvokeService` envelope.
async fn invoke_windows_command<Req, Resp>(
    client: &mut StreamingBmuxClient,
    operation: &str,
    args: &Req,
) -> std::result::Result<Resp, ClientError>
where
    Req: serde::Serialize + Sync,
    Resp: serde::de::DeserializeOwned,
{
    let payload = bmux_codec::to_vec(args).map_err(|error| ClientError::ServerError {
        code: bmux_ipc::ErrorCode::Internal,
        message: format!("encoding {operation}: {error}"),
    })?;
    let response_bytes = client
        .invoke_service_raw(
            typed_windows::WINDOWS_WRITE_CAPABILITY.as_str(),
            InvokeServiceKind::Command,
            typed_windows::WINDOWS_COMMANDS_INTERFACE.as_str(),
            operation,
            payload,
        )
        .await?;
    bmux_codec::from_bytes::<Resp>(&response_bytes).map_err(|error| ClientError::ServerError {
        code: bmux_ipc::ErrorCode::Internal,
        message: format!("decoding {operation} response: {error}"),
    })
}

/// Re-export of the shared arg structs for convenience at call sites.
use typed_windows::args as windows_cmd_args;
use typed_windows::{ipc_split_to_typed_direction, ipc_to_typed_selector};

// ── legacy in-attach-runtime helpers that were moved to
// ── `super::super::typed_windows` are deleted below. Anything that
// ── still needs converter helpers should import them above.

#[derive(Default)]
pub struct DisplayCaptureFanout {
    writers: BTreeMap<Uuid, recording::DisplayCaptureWriter>,
}

impl DisplayCaptureFanout {
    fn open_target(&mut self, target: &bmux_ipc::RecordingCaptureTarget, client_id: Uuid) {
        if self.writers.contains_key(&target.recording_id) {
            return;
        }
        match recording::DisplayCaptureWriter::open(
            target.recording_id,
            Path::new(&target.path),
            client_id,
        ) {
            Ok(writer) => {
                self.writers.insert(target.recording_id, writer);
            }
            Err(error) => {
                tracing::warn!(
                    "failed starting display capture for recording {}: {error}",
                    target.recording_id
                );
            }
        }
    }

    fn close_recording(&mut self, recording_id: Uuid) {
        if let Some(mut writer) = self.writers.remove(&recording_id) {
            let _ = writer.record_stream_closed();
            let _ = writer.flush();
        }
    }

    fn close_all(&mut self) {
        let ids: Vec<Uuid> = self.writers.keys().copied().collect();
        for id in ids {
            self.close_recording(id);
        }
    }

    fn record_resize(&mut self, cols: u16, rows: u16) {
        let mut failed = Vec::new();
        for (id, writer) in &mut self.writers {
            if writer.record_resize(cols, rows).is_err() {
                failed.push(*id);
            }
        }
        for id in failed {
            self.close_recording(id);
        }
    }

    fn record_frame_bytes(&mut self, data: &[u8]) {
        let mut failed = Vec::new();
        for (id, writer) in &mut self.writers {
            if writer.record_frame_bytes(data).is_err() {
                failed.push(*id);
            }
        }
        for id in failed {
            self.close_recording(id);
        }
    }

    fn record_activity(&mut self, kind: bmux_ipc::DisplayActivityKind) {
        let mut failed = Vec::new();
        for (id, writer) in &mut self.writers {
            if writer.record_activity(kind).is_err() {
                failed.push(*id);
            }
        }
        for id in failed {
            self.close_recording(id);
        }
    }

    #[cfg(any(
        feature = "image-sixel",
        feature = "image-kitty",
        feature = "image-iterm2"
    ))]
    fn record_images(&mut self, images: &[bmux_ipc::AttachPaneImage]) {
        let mut failed = Vec::new();
        for (id, writer) in &mut self.writers {
            if writer.record_images(images).is_err() {
                failed.push(*id);
            }
        }
        for id in failed {
            self.close_recording(id);
        }
    }

    fn record_cursor_snapshot(&mut self, cursor_state: Option<super::state::AttachCursorState>) {
        let mut failed = Vec::new();
        for (id, writer) in &mut self.writers {
            if writer.record_cursor_snapshot(cursor_state).is_err() {
                failed.push(*id);
            }
        }
        for id in failed {
            self.close_recording(id);
        }
    }
}

fn apply_attach_output_bytes(
    view_state: &mut AttachViewState,
    pane_id: Uuid,
    bytes: &[u8],
    frame_needs_render: &mut bool,
) -> bool {
    if bytes.is_empty() {
        return false;
    }

    let buffer = view_state.pane_buffers.entry(pane_id).or_default();
    let toggled_alternate = append_pane_output(buffer, bytes);
    let screen = buffer.parser.screen();
    view_state.pane_mouse_protocol_hints.insert(
        pane_id,
        bmux_ipc::AttachMouseProtocolState {
            mode: mouse_protocol_mode_to_ipc(screen.mouse_protocol_mode()),
            encoding: mouse_protocol_encoding_to_ipc(screen.mouse_protocol_encoding()),
        },
    );
    view_state.pane_input_mode_hints.insert(
        pane_id,
        bmux_ipc::AttachInputModeState {
            application_cursor: screen.application_cursor(),
            application_keypad: screen.application_keypad(),
        },
    );
    view_state.dirty.pane_dirty_ids.insert(pane_id);
    *frame_needs_render = true;

    if toggled_alternate {
        view_state.dirty.full_pane_redraw = true;
        view_state.force_cursor_move_next_frame = true;
    }

    true
}

fn apply_attach_output_chunk(
    view_state: &mut AttachViewState,
    pane_id: Uuid,
    bytes: &[u8],
    meta: AttachOutputChunkMeta,
    frame_needs_render: &mut bool,
) -> AttachChunkApplyOutcome {
    let pane_mouse_protocol_hints = &mut view_state.pane_mouse_protocol_hints;
    let pane_input_mode_hints = &mut view_state.pane_input_mode_hints;
    let mut toggled_alternate = false;
    let outcome = apply_attach_output_chunk_with(
        &mut view_state.pane_buffers,
        pane_id,
        bytes,
        meta,
        |buffer, data| {
            if data.is_empty() {
                return false;
            }

            toggled_alternate = append_pane_output(buffer, data) || toggled_alternate;
            let screen = buffer.parser.screen();
            pane_mouse_protocol_hints.insert(
                pane_id,
                bmux_ipc::AttachMouseProtocolState {
                    mode: mouse_protocol_mode_to_ipc(screen.mouse_protocol_mode()),
                    encoding: mouse_protocol_encoding_to_ipc(screen.mouse_protocol_encoding()),
                },
            );
            pane_input_mode_hints.insert(
                pane_id,
                bmux_ipc::AttachInputModeState {
                    application_cursor: screen.application_cursor(),
                    application_keypad: screen.application_keypad(),
                },
            );
            true
        },
    );

    if outcome == (AttachChunkApplyOutcome::Applied { had_data: true }) {
        view_state.dirty.pane_dirty_ids.insert(pane_id);
        *frame_needs_render = true;
    }

    if toggled_alternate {
        view_state.dirty.full_pane_redraw = true;
        view_state.force_cursor_move_next_frame = true;
    }

    outcome
}

async fn recover_attach_output_desync_for_pane(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    pane_id: Uuid,
) -> std::result::Result<(), ClientError> {
    if client.supports_capability(CAPABILITY_ATTACH_PANE_SNAPSHOT)
        && let Some(layout_state) = view_state.cached_layout_state.clone()
        && attach_layout_pane_id_set(&layout_state).contains(&pane_id)
    {
        hydrate_attach_revealed_panes_from_snapshot(client, view_state, &layout_state, &[pane_id])
            .await?;
        view_state.dirty.full_pane_redraw = true;
        return Ok(());
    }

    hydrate_attach_state_from_snapshot_mode(client, view_state, SnapshotHydrationMode::FullResync)
        .await
}

#[derive(Debug, Clone)]
struct AttachPerfWindow {
    started_at: Instant,
    drain_rounds: u64,
    drain_rounds_with_data: u64,
    drain_sync_active_rounds: u64,
    drain_budget_hits: u64,
    drain_ipc_calls: u64,
    drain_bytes: u64,
    drain_ipc_ms_sum: u64,
    drain_ipc_ms_max: u64,
    render_frames: u64,
    render_ms_sum: u64,
    render_ms_max: u64,
}

impl AttachPerfWindow {
    fn new() -> Self {
        Self {
            started_at: Instant::now(),
            drain_rounds: 0,
            drain_rounds_with_data: 0,
            drain_sync_active_rounds: 0,
            drain_budget_hits: 0,
            drain_ipc_calls: 0,
            drain_bytes: 0,
            drain_ipc_ms_sum: 0,
            drain_ipc_ms_max: 0,
            render_frames: 0,
            render_ms_sum: 0,
            render_ms_max: 0,
        }
    }

    const fn record_drain_round(&mut self) {
        self.drain_rounds = self.drain_rounds.saturating_add(1);
    }

    const fn record_drain_result(&mut self, had_data: bool, sync_active: bool) {
        if had_data {
            self.drain_rounds_with_data = self.drain_rounds_with_data.saturating_add(1);
        }
        if sync_active {
            self.drain_sync_active_rounds = self.drain_sync_active_rounds.saturating_add(1);
        }
    }

    const fn record_drain_budget_hit(&mut self) {
        self.drain_budget_hits = self.drain_budget_hits.saturating_add(1);
    }

    fn record_drain_ipc(&mut self, elapsed_ms: u64, bytes: usize) {
        self.drain_ipc_calls = self.drain_ipc_calls.saturating_add(1);
        self.drain_ipc_ms_sum = self.drain_ipc_ms_sum.saturating_add(elapsed_ms);
        self.drain_ipc_ms_max = self.drain_ipc_ms_max.max(elapsed_ms);
        self.drain_bytes = self
            .drain_bytes
            .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
    }

    fn record_render_frame(&mut self, elapsed_ms: u64) {
        self.render_frames = self.render_frames.saturating_add(1);
        self.render_ms_sum = self.render_ms_sum.saturating_add(elapsed_ms);
        self.render_ms_max = self.render_ms_max.max(elapsed_ms);
    }

    fn reset(&mut self) {
        *self = Self::new();
    }
}

#[allow(clippy::cast_possible_truncation)]
fn duration_millis_u64(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedThemeState {
    selected_theme: String,
}

#[derive(Debug, Clone)]
struct ThemeCatalogEntry {
    name: String,
    theme: bmux_config::ThemeConfig,
}

#[derive(Debug, Clone)]
pub struct AttachThemeState {
    active_name: String,
    active_theme: bmux_config::ThemeConfig,
    declared_name: String,
    persistence: bmux_config::ThemePersistence,
    state_file: PathBuf,
    config_dir_candidates: Vec<String>,
    theme_plugin_ids: Vec<String>,
    catalog: Vec<ThemeCatalogEntry>,
    preview_original: Option<(String, bmux_config::ThemeConfig)>,
}

impl AttachThemeState {
    fn new(config: &BmuxConfig, paths: &ConfigPaths) -> Self {
        let declared_name = normalized_theme_name(&config.appearance.theme);
        let mut catalog = load_theme_catalog(paths);
        ensure_theme_catalog_entry(&mut catalog, declared_name.as_str(), paths);
        let persisted_name = match config.appearance.theme_persistence {
            bmux_config::ThemePersistence::PersistBetweenConnects => {
                read_persisted_theme_name(&paths.runtime_theme_state_file())
            }
            bmux_config::ThemePersistence::DeclaredOnConnect => None,
        };
        let active_name = persisted_name
            .filter(|name| catalog.iter().any(|entry| entry.name == *name))
            .unwrap_or_else(|| declared_name.clone());
        let active_theme = catalog
            .iter()
            .find(|entry| entry.name == active_name)
            .map_or_else(bmux_config::ThemeConfig::default, |entry| {
                entry.theme.clone()
            });
        let theme_plugin_ids = theme_catalog_plugin_ids(&catalog);
        Self {
            active_name,
            active_theme,
            declared_name,
            persistence: config.appearance.theme_persistence,
            state_file: paths.runtime_theme_state_file(),
            config_dir_candidates: paths
                .config_dir_candidates()
                .iter()
                .map(|path| path.to_string_lossy().to_string())
                .collect(),
            theme_plugin_ids,
            catalog,
            preview_original: None,
        }
    }

    fn selected_index(&self) -> usize {
        self.catalog
            .iter()
            .position(|entry| entry.name == self.active_name)
            .unwrap_or(0)
    }

    fn prompt_options(&self) -> Vec<prompt::PromptOption> {
        self.catalog
            .iter()
            .map(|entry| {
                let mut label = entry.name.clone();
                if entry.name == self.declared_name {
                    label.push_str(" (declared)");
                }
                if entry.name == self.active_name {
                    label.push_str(" (active)");
                }
                prompt::PromptOption::new(entry.name.as_str(), label)
            })
            .collect()
    }

    fn begin_preview(&mut self) {
        self.preview_original = Some((self.active_name.clone(), self.active_theme.clone()));
    }

    fn preview(&mut self, name: &str) -> bool {
        let Some(entry) = self.catalog.iter().find(|entry| entry.name == name) else {
            return false;
        };
        self.active_name.clone_from(&entry.name);
        self.active_theme.clone_from(&entry.theme);
        true
    }

    fn cancel_preview(&mut self) {
        if let Some((name, theme)) = self.preview_original.take() {
            self.active_name = name;
            self.active_theme = theme;
        }
    }

    fn commit_preview(&mut self) {
        self.preview_original = None;
        if matches!(
            self.persistence,
            bmux_config::ThemePersistence::PersistBetweenConnects
        ) {
            persist_theme_name(&self.state_file, &self.active_name);
        }
    }
}

fn normalized_theme_name(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        "default".to_string()
    } else {
        trimmed.to_string()
    }
}

fn read_persisted_theme_name(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let state: PersistedThemeState = serde_json::from_slice(&bytes).ok()?;
    Some(normalized_theme_name(&state.selected_theme))
}

fn persist_theme_name(path: &Path, name: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let state = PersistedThemeState {
        selected_theme: name.to_string(),
    };
    if let Ok(bytes) = serde_json::to_vec_pretty(&state) {
        let _ = std::fs::write(path, bytes);
    }
}

fn load_theme_catalog(paths: &ConfigPaths) -> Vec<ThemeCatalogEntry> {
    let mut entries = vec![ThemeCatalogEntry {
        name: "default".to_string(),
        theme: bmux_config::ThemeConfig::default(),
    }];
    for dir in paths.config_dir_candidates() {
        let themes_dir = dir.join("themes");
        let Ok(read_dir) = std::fs::read_dir(&themes_dir) else {
            continue;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(std::ffi::OsStr::to_str) != Some("toml") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(std::ffi::OsStr::to_str) else {
                continue;
            };
            if let Ok(text) = std::fs::read_to_string(&path)
                && let Ok(theme) = toml::from_str::<bmux_config::ThemeConfig>(&text)
            {
                upsert_theme_catalog_entry(&mut entries, name.to_string(), theme);
            }
        }
    }
    #[cfg(feature = "bundled-plugin-decoration")]
    for (name, text) in bmux_decoration_plugin::bundled_theme_presets() {
        if let Ok(theme) = toml::from_str::<bmux_config::ThemeConfig>(text) {
            upsert_theme_catalog_entry(&mut entries, (*name).to_string(), theme);
        }
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

fn theme_catalog_plugin_ids(catalog: &[ThemeCatalogEntry]) -> Vec<String> {
    let mut ids = catalog
        .iter()
        .flat_map(|entry| entry.theme.plugins.keys().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    ids.sort();
    ids
}

fn ensure_theme_catalog_entry(
    entries: &mut Vec<ThemeCatalogEntry>,
    name: &str,
    paths: &ConfigPaths,
) {
    if entries.iter().any(|entry| entry.name == name) {
        return;
    }
    if name == "default" {
        upsert_theme_catalog_entry(
            entries,
            name.to_string(),
            bmux_config::ThemeConfig::default(),
        );
        return;
    }
    let path = paths.resolve_theme_file(name);
    if let Ok(text) = std::fs::read_to_string(path)
        && let Ok(theme) = toml::from_str::<bmux_config::ThemeConfig>(&text)
    {
        upsert_theme_catalog_entry(entries, name.to_string(), theme);
    }
}

fn upsert_theme_catalog_entry(
    entries: &mut Vec<ThemeCatalogEntry>,
    name: String,
    mut theme: bmux_config::ThemeConfig,
) {
    theme.name.clone_from(&name);
    if let Some(existing) = entries.iter_mut().find(|entry| entry.name == name) {
        existing.theme = theme;
    } else {
        entries.push(ThemeCatalogEntry { name, theme });
    }
}

fn open_theme_picker(view_state: &mut AttachViewState, theme_state: &mut AttachThemeState) {
    if theme_state.catalog.is_empty() {
        view_state.set_transient_status(
            "no themes available",
            Instant::now(),
            ATTACH_TRANSIENT_STATUS_TTL,
        );
        return;
    }
    theme_state.begin_preview();
    let persistence = match theme_state.persistence {
        bmux_config::ThemePersistence::DeclaredOnConnect => "not persisted",
        bmux_config::ThemePersistence::PersistBetweenConnects => "persisted on accept",
    };
    let request = PromptRequest::single_select("Select Theme", theme_state.prompt_options())
        .message(format!(
            "Move to preview live. Enter applies. Esc restores previous theme. Persistence: {persistence}."
        ))
        .single_default_index(theme_state.selected_index())
        .single_live_preview(true)
        .policy(prompt::PromptPolicy::RejectIfBusy)
        .width_range(48, 96);
    view_state
        .prompt
        .enqueue_internal(request, AttachInternalPromptAction::ThemePicker);
    view_state.dirty.status_needs_redraw = true;
    view_state.dirty.overlay_needs_redraw = true;
}

/// Publish the current attach-layout snapshot on the generic
/// [`AttachLayoutSnapshot`] state channel. Plugins that need
/// per-pane geometry (decoration renderers, overlays, etc.)
/// subscribe to that channel via [`bmux_plugin::global_event_bus`]
/// and receive the current value plus live updates.
///
/// Returns the set of pane ids present in the latest layout so the
/// caller can optionally forward it to other listeners. When the
/// layout is absent the snapshot is empty and the pane-id set is
/// empty too.
async fn notify_extensions_of_layout(
    client: &mut bmux_client::StreamingBmuxClient,
    _previous: Option<&bmux_client::AttachLayoutState>,
    current: Option<&bmux_client::AttachLayoutState>,
) -> std::collections::BTreeSet<Uuid> {
    use bmux_scene_protocol::scene_protocol::Rect as SceneRect;

    let Some(current) = current else {
        publish_attach_layout_snapshot(client, &[]).await;
        return std::collections::BTreeSet::new();
    };

    let layout_entries: Vec<AttachSurfaceSummary> = current
        .scene
        .surfaces
        .iter()
        .map(|surface| AttachSurfaceSummary {
            surface_id: surface.id,
            pane_id: surface.pane_id,
            rect: SceneRect {
                x: surface.rect.x,
                y: surface.rect.y,
                w: surface.rect.w,
                h: surface.rect.h,
            },
            content_rect: SceneRect {
                x: surface.content_rect.x,
                y: surface.content_rect.y,
                w: surface.content_rect.w,
                h: surface.content_rect.h,
            },
            visible: surface.visible,
        })
        .collect();
    publish_attach_layout_snapshot(client, &layout_entries).await;

    let mut current_pane_ids = std::collections::BTreeSet::new();
    for surface in &current.scene.surfaces {
        let Some(pane_id) = surface.pane_id else {
            continue;
        };
        if !surface.visible {
            continue;
        }
        current_pane_ids.insert(pane_id);
    }
    current_pane_ids
}

/// Publish an [`AttachLayoutSnapshot`] on the attach-layout state
/// channel (client-side event bus) AND forward the same payload to
/// the server's event bus via [`Request::EmitOnPluginBus`]. Plugins
/// that live in the server process (e.g. the decoration plugin)
/// subscribe to the server-side channel and see the snapshot
/// alongside client-side subscribers.
async fn publish_attach_layout_snapshot(
    client: &mut bmux_client::StreamingBmuxClient,
    surfaces: &[AttachSurfaceSummary],
) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static REVISION: AtomicU64 = AtomicU64::new(0);
    let revision = REVISION.fetch_add(1, Ordering::Relaxed) + 1;
    let payload = AttachLayoutSnapshot {
        surfaces: surfaces.to_vec(),
        revision,
    };
    // Client-side publish: any render extension running in this
    // process picks up the snapshot.
    let _ =
        bmux_plugin::global_event_bus().publish_state(&ATTACH_LAYOUT_STATE_KIND, payload.clone());
    // Server-side publish via IPC: any plugin that registered the
    // attach-layout channel with a decoder picks up the snapshot on
    // its process-local event bus. When the server hasn't registered
    // a decoder (no subscribing plugin), the server silently drops
    // the payload.
    let encoded = match serde_json::to_vec(&payload) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(%error, "failed encoding attach-layout snapshot for plugin-bus relay");
            return;
        }
    };
    if let Err(error) = client
        .emit_on_plugin_bus(ATTACH_LAYOUT_STATE_KIND.as_str(), encoded)
        .await
    {
        tracing::debug!(%error, "emit_on_plugin_bus for attach-layout failed");
    }
}

async fn maybe_emit_attach_perf_window(
    perf_emitter: &mut recording::PerfEventEmitter,
    client: &mut StreamingBmuxClient,
    session_id: Uuid,
    window: &mut AttachPerfWindow,
) -> Result<()> {
    if !perf_emitter.enabled() {
        return Ok(());
    }

    let elapsed = window.started_at.elapsed();
    if elapsed < Duration::from_millis(perf_emitter.window_ms()) {
        return Ok(());
    }

    let detailed = perf_emitter.level_at_least(recording::PerfCaptureLevel::Detailed);
    let trace = perf_emitter.level_at_least(recording::PerfCaptureLevel::Trace);
    let mut payload = serde_json::json!({
        "window_elapsed_ms": duration_millis_u64(elapsed),
        "drain_rounds": window.drain_rounds,
        "drain_ipc_calls": window.drain_ipc_calls,
        "drain_bytes": window.drain_bytes,
        "render_frames": window.render_frames,
    });
    if detailed && let Some(object) = payload.as_object_mut() {
        object.insert(
            "drain_ipc_ms_sum".to_string(),
            serde_json::Value::from(window.drain_ipc_ms_sum),
        );
        object.insert(
            "drain_ipc_ms_max".to_string(),
            serde_json::Value::from(window.drain_ipc_ms_max),
        );
        object.insert(
            "render_ms_sum".to_string(),
            serde_json::Value::from(window.render_ms_sum),
        );
        object.insert(
            "render_ms_max".to_string(),
            serde_json::Value::from(window.render_ms_max),
        );
        if window.drain_ipc_calls > 0 {
            object.insert(
                "drain_ipc_ms_avg".to_string(),
                serde_json::Value::from(window.drain_ipc_ms_sum / window.drain_ipc_calls),
            );
        }
        if window.render_frames > 0 {
            object.insert(
                "render_ms_avg".to_string(),
                serde_json::Value::from(window.render_ms_sum / window.render_frames),
            );
        }
    }
    if trace && let Some(object) = payload.as_object_mut() {
        object.insert(
            "drain_rounds_with_data".to_string(),
            serde_json::Value::from(window.drain_rounds_with_data),
        );
        object.insert(
            "drain_sync_active_rounds".to_string(),
            serde_json::Value::from(window.drain_sync_active_rounds),
        );
        object.insert(
            "drain_budget_hits".to_string(),
            serde_json::Value::from(window.drain_budget_hits),
        );
    }

    perf_emitter
        .emit_with_streaming_client(client, Some(session_id), None, "attach.window", payload)
        .await?;
    window.reset();
    Ok(())
}

#[allow(clippy::too_many_arguments)] // keep frame/attach telemetry emit context explicit
async fn maybe_emit_attach_frame_perf(
    perf_emitter: &mut recording::PerfEventEmitter,
    client: &mut StreamingBmuxClient,
    session_id: Uuid,
    attach_started_at: Instant,
    rendered_frame_count: u64,
    frame_render_ms: u64,
    scene_hydrated: bool,
    first_frame_emitted: &mut bool,
    interactive_ready_emitted: &mut bool,
) -> Result<()> {
    if !perf_emitter.enabled() {
        return Ok(());
    }

    let since_attach_start_ms = duration_millis_u64(attach_started_at.elapsed());
    if !*first_frame_emitted && perf_emitter.level_at_least(recording::PerfCaptureLevel::Basic) {
        perf_emitter
            .emit_with_streaming_client(
                client,
                Some(session_id),
                None,
                "attach.first_frame",
                serde_json::json!({
                    "time_to_first_frame_ms": since_attach_start_ms,
                    "frame_render_ms": frame_render_ms,
                    "frame_index": rendered_frame_count,
                    "scene_hydrated": scene_hydrated,
                }),
            )
            .await?;
        *first_frame_emitted = true;
    }

    if scene_hydrated
        && !*interactive_ready_emitted
        && perf_emitter.level_at_least(recording::PerfCaptureLevel::Basic)
    {
        perf_emitter
            .emit_with_streaming_client(
                client,
                Some(session_id),
                None,
                "attach.interactive.ready",
                serde_json::json!({
                    "time_to_interactive_ms": since_attach_start_ms,
                    "frame_render_ms": frame_render_ms,
                    "frame_index": rendered_frame_count,
                }),
            )
            .await?;
        *interactive_ready_emitted = true;
    }

    if perf_emitter.level_at_least(recording::PerfCaptureLevel::Trace) {
        perf_emitter
            .emit_with_streaming_client(
                client,
                Some(session_id),
                None,
                "attach.frame.trace",
                serde_json::json!({
                    "frame_render_ms": frame_render_ms,
                    "frame_index": rendered_frame_count,
                    "since_attach_start_ms": since_attach_start_ms,
                    "scene_hydrated": scene_hydrated,
                }),
            )
            .await?;
    }

    Ok(())
}

/// Install every bundled plugin's client-side render extension.
///
/// Each `install` call is idempotent (uses its own `OnceLock` to
/// guard extension construction). Extensions are feature-gated so
/// builds that don't bundle a plugin simply skip its install call.
///
/// Called once per attach session from
/// [`run_session_attach_with_client`]; subsequent attaches reuse the
/// already-installed extensions.
fn install_bundled_render_extensions() {
    #[cfg(feature = "bundled-plugin-decoration")]
    {
        bmux_decoration_plugin_renderer::install();
    }
}

#[allow(clippy::too_many_lines)] // Core attach loop -- splitting would fragment state management
pub async fn run_session_attach_with_client(
    mut client: BmuxClient,
    target: Option<&str>,
    follow: Option<&str>,
    global: bool,
    kernel_client_factory: Option<KernelClientFactory>,
) -> Result<AttachRunOutcome> {
    if target.is_none() && follow.is_none() {
        anyhow::bail!("attach requires a session target or --follow <client-uuid>");
    }
    if target.is_some() && follow.is_some() {
        anyhow::bail!("attach accepts either a session target or --follow, not both");
    }

    // Install client-side render extensions for any bundled plugins
    // that ship one. Each crate's `install` is idempotent; process-
    // wide state is initialised on first call. Extensions populate
    // themselves lazily as scene events arrive.
    install_bundled_render_extensions();

    let follow_target_id = match follow {
        Some(follow_target) => Some(parse_uuid_value(follow_target, "follow target client id")?),
        None => None,
    };

    let mut attach_config_paths = ConfigPaths::default();
    if let Some(config_parent) = std::env::var_os(bmux_config::BMUX_CONFIG_ENV)
        .map(PathBuf::from)
        .and_then(|path| path.parent().map(Path::to_path_buf))
    {
        attach_config_paths.prepend_config_dir_candidate(config_parent);
    }

    let attach_config = match BmuxConfig::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!(
                "bmux warning: failed loading config for attach keymap, using defaults ({error})"
            );
            BmuxConfig::default()
        }
    };
    let attach_keymap = attach_keymap_from_config(&attach_config);
    let attach_help_lines = build_attach_help_lines(&attach_config);
    let mut perf_emitter = recording::PerfEventEmitter::new(
        recording::PerfCaptureSettings::from_config(&attach_config),
    );
    if let Ok(settings) =
        bmux_performance_plugin_api::typed_client::performance_status(&mut client).await
    {
        perf_emitter.update_settings(recording::PerfCaptureSettings::from_runtime_settings(
            &settings,
        ));
    }
    let mut perf_window = AttachPerfWindow::new();
    let attach_started_at = Instant::now();
    let mut rendered_frame_count = 0_u64;
    let mut first_frame_emitted = false;
    let mut interactive_ready_emitted = false;
    let mut theme_state = AttachThemeState::new(&attach_config, &attach_config_paths);

    if let Some(leader_client_id) = follow_target_id {
        client
            .subscribe_events()
            .await
            .map_err(map_attach_client_error)?;
        bmux_clients_plugin_api::typed_client::follow_client(&mut client, leader_client_id, global)
            .await?;
    }

    let self_client_id = bmux_clients_plugin_api::typed_client::whoami(&mut client).await?;

    let attach_info = if let Some(leader_client_id) = follow_target_id {
        // Inline follow target resolution using BmuxClient (before streaming upgrade).
        let clients = typed_list_clients_bmux(&mut client)
            .await
            .map_err(map_attach_client_error)?;
        let leader = clients
            .into_iter()
            .find(|entry| entry.id == leader_client_id)
            .ok_or_else(|| anyhow::anyhow!("follow target not found"))?;
        let context_id = if let Some(cid) = leader.selected_context_id {
            cid
        } else if let Some(sid) = leader.selected_session_id {
            let contexts = typed_list_contexts_bmux(&mut client)
                .await
                .map_err(map_attach_client_error)?;
            contexts
                .into_iter()
                .find(|ctx| {
                    ctx.attributes
                        .get("bmux.session_id")
                        .is_some_and(|v| v == &sid.to_string())
                })
                .map(|ctx| ctx.id)
                .ok_or_else(|| anyhow::anyhow!("follow target has no selected context"))?
        } else {
            anyhow::bail!("follow target has no selected context");
        };
        let grant = client
            .attach_context_grant(ContextSelector::ById(context_id))
            .await
            .map_err(map_attach_client_error)?;
        client
            .open_attach_stream_info(&grant)
            .await
            .map_err(map_attach_client_error)?
    } else {
        let target = target.expect("target is present when not follow");
        let grant = client
            .attach_grant(parse_session_selector(target))
            .await
            .map_err(map_attach_client_error)?;
        client
            .open_attach_stream_info(&grant)
            .await
            .map_err(map_attach_client_error)?
    };

    if let Some(leader_client_id) = follow_target_id {
        println!(
            "attached to session: {} (following {}{})",
            attach_info.session_id,
            leader_client_id,
            if global { ", global" } else { "" }
        );
    } else {
        println!("attached to session: {}", attach_info.session_id);
    }

    let capture_targets =
        match bmux_recording_plugin_api::typed_client::recording_capture_targets(&mut client).await
        {
            Ok(targets) => targets,
            Err(error) => {
                tracing::warn!("failed querying recording capture targets on attach: {error}");
                Vec::new()
            }
        };

    // Upgrade to streaming client for event-driven operation.
    // All subsequent operations use the streaming client.
    let mut client =
        bmux_client::StreamingBmuxClient::from_client(client).map_err(map_attach_client_error)?;
    client
        .subscribe_events()
        .await
        .map_err(map_attach_client_error)?;
    client
        .enable_event_push()
        .await
        .map_err(map_attach_client_error)?;

    let mut display_capture = DisplayCaptureFanout::default();
    for target in &capture_targets {
        display_capture.open_target(target, self_client_id);
    }

    let mut view_state = AttachViewState::new(attach_info);
    view_state.self_client_id = Some(self_client_id);
    view_state.mouse.config = attach_config.attach_mouse_config();
    view_state.status_position = if attach_config.status_bar.enabled {
        attach_config.appearance.status_position
    } else {
        StatusPosition::Off
    };
    apply_plugin_theme_extensions(
        &mut client,
        theme_state.active_theme.clone(),
        &theme_state.theme_plugin_ids,
        &theme_state.config_dir_candidates,
    )
    .await;

    // Register the generic attach-layout state channel so any plugin
    // subscribing via `EventBus::subscribe_state::<AttachLayoutSnapshot>`
    // sees a valid initial payload. Subsequent layout refreshes update
    // the retained value via `publish_attach_layout_snapshot`. The
    // channel is process-wide; re-registration is a no-op.
    let _ = bmux_plugin::global_event_bus().register_state_channel::<AttachLayoutSnapshot>(
        ATTACH_LAYOUT_STATE_KIND,
        AttachLayoutSnapshot {
            surfaces: Vec::new(),
            revision: 0,
        },
    );

    // Register the windows-plugin's `windows-list` state channel in
    // this attach process's event bus. The windows plugin publishes
    // into the server process's bus; the server's
    // `spawn_plugin_bus_state_forwarder` forwards each snapshot
    // replacement as `Event::PluginBusEvent`, which this process
    // decodes and re-publishes onto the local channel (see
    // `PluginBusEvent` handler below). The local `subscribe_state`
    // call a few lines down reads from this channel.
    //
    // Re-registration is a no-op, so harmless if this runs twice.
    let _ = bmux_plugin::global_event_bus()
        .register_state_channel::<bmux_windows_plugin_api::windows_list::WindowListSnapshot>(
        bmux_windows_plugin_api::windows_list::STATE_KIND,
        bmux_windows_plugin_api::windows_list::WindowListSnapshot {
            windows: Vec::new(),
            revision: 0,
        },
    );

    // Honour any startup gates plugins have self-registered via
    // `bmux_plugin::register_startup_ready_gate`. Attach startup
    // blocks briefly for each gate so subsystems like the
    // decoration renderer have a chance to publish their initial
    // scene before the first frame is painted.
    let ready_tracker = super::super::plugin_runtime::ready_tracker_snapshot();
    for gate in bmux_plugin::registered_startup_ready_gates() {
        if ready_tracker
            .status(&gate.plugin_id, &gate.signal)
            .is_some()
        {
            let _ = ready_tracker.await_ready(&gate.plugin_id, &gate.signal, gate.timeout);
        }
    }

    update_attach_viewport(
        &mut client,
        view_state.attached_id,
        view_state.status_position,
    )
    .await?;
    hydrate_attach_state_from_snapshot(&mut client, &mut view_state).await?;
    // `hydrate_attach_state_from_snapshot` populated
    // `cached_layout_state`; emit the first attach-layout snapshot so
    // plugins subscribing to the state channel observe initial
    // per-pane rects at attach entry. Subsequent layout changes flow
    // through the loop-body call to
    // `notify_extensions_of_layout` at the layout-refresh site.
    let _ = notify_extensions_of_layout(&mut client, None, view_state.cached_layout_state.as_ref())
        .await;
    refresh_attach_status_catalog_best_effort(&mut client, &mut view_state).await;
    sync_attach_active_mode_from_processor(&mut view_state, &attach_keymap, None);
    view_state.set_transient_status(
        initial_attach_status(
            &attach_keymap,
            &view_state.active_mode_id,
            view_state.can_write,
        ),
        Instant::now(),
        ATTACH_WELCOME_STATUS_TTL,
    );

    if !view_state.can_write {
        println!("read-only attach: input disabled");
    }
    if let Some(detach_key) = attach_keymap.primary_binding_for_action(&RuntimeAction::Detach) {
        println!("press {detach_key} to detach");
    } else {
        println!("detach is unbound in current keymap");
    }

    let raw_mode_guard = RawModeGuard::enable(
        attach_config.behavior.kitty_keyboard,
        attach_config.attach_mouse_config().enabled,
    )
    .context("failed to enable raw mode for attach")?;
    let mut attach_input_processor =
        InputProcessor::new(attach_keymap.clone(), raw_mode_guard.keyboard_enhanced);
    let (prompt_host_tx, mut prompt_host_rx) = tokio::sync::mpsc::unbounded_channel();
    let _prompt_host_guard = prompt::register_host(prompt_host_tx);
    let (action_dispatch_tx, mut action_dispatch_rx) = tokio::sync::mpsc::unbounded_channel();
    let _action_dispatch_guard = action_dispatch::register_host(action_dispatch_tx);
    // Default exit reason; always overwritten before the loop breaks, but the
    // compiler cannot prove this through the tokio::select! macro expansion.
    #[allow(unused_assignments)]
    let mut exit_reason = AttachExitReason::Detached;

    // Detect host terminal image capabilities (Sixel, Kitty graphics, iTerm2)
    // and store in view_state for the compositor.
    #[cfg(any(
        feature = "image-sixel",
        feature = "image-kitty",
        feature = "image-iterm2"
    ))]
    {
        let mut caps = bmux_image::host_caps::detect_with_queries();
        let (cpw, cph) = bmux_image::host_caps::query_cell_pixel_size();
        caps.cell_pixel_width = cpw;
        caps.cell_pixel_height = cph;
        view_state.host_image_caps = caps;
        // Cache the decode mode from config so we don't read config per-frame.
        let img_cfg = attach_config.behavior.images.decode_mode;
        view_state.image_decode_mode = match img_cfg {
            bmux_config::ImageDecodeMode::Server => bmux_image::config::ImageDecodeMode::Server,
            bmux_config::ImageDecodeMode::Client => bmux_image::config::ImageDecodeMode::Client,
            bmux_config::ImageDecodeMode::Passthrough => {
                bmux_image::config::ImageDecodeMode::Passthrough
            }
        };
    }

    // Async terminal event stream — replaces spawn_blocking + poll(15ms).
    let mut terminal_stream = crossterm::event::EventStream::new();
    let mut pane_output_pending = false;
    #[cfg(any(
        feature = "image-sixel",
        feature = "image-kitty",
        feature = "image-iterm2"
    ))]
    let mut image_fetch_pending = false;

    // Subscribe to the generic scene-protocol broadcast so the
    // attach runtime can mark the frame dirty whenever a new scene
    // arrives. Render extensions subscribe to the same channel
    // independently. When no plugin has registered the channel the
    // subscription returns `Err` and we run without scene updates —
    // extensions that aren't loaded simply have nothing to draw.
    let mut scene_event_rx = bmux_plugin::global_event_bus()
        .subscribe::<bmux_scene_protocol::scene_protocol::EventPayload>(
            &bmux_scene_protocol::scene_protocol::EVENT_KIND,
        )
        .ok();

    // Subscribe to the windows-plugin `windows-list` state channel so
    // the attach tab bar can render the authoritative plugin-owned
    // window order without polling. Seeds `view_state.cached_window_list`
    // synchronously with the current value (if any), then drives live
    // updates through a `tokio::select!` arm below. When the plugin
    // has not registered the channel (plugin absent / not yet
    // activated), the tab bar falls back to `cached_contexts` in raw
    // server order — baseline behavior per AGENTS.md.
    let mut window_list_rx = match bmux_plugin::global_event_bus()
        .subscribe_state::<bmux_windows_plugin_api::windows_list::WindowListSnapshot>(
        &bmux_windows_plugin_api::windows_list::STATE_KIND,
    ) {
        Ok((initial, rx)) => {
            view_state.cached_window_list = Some(initial);
            Some(rx)
        }
        Err(_) => None,
    };

    // Pull the current window list from the windows-plugin via typed
    // dispatch. The server-side state-channel forwarder emits the
    // initial snapshot before any client connects, so it's always
    // lost on a fresh attach. Pulling here seeds our local state
    // channel with the correct current value; subsequent mutations
    // arrive via `PluginBusEvent` and drive updates through the
    // `tokio::select!` arm below.
    //
    // Silent-failure policy: if the plugin isn't loaded, the
    // `invoke_service_raw` call errors and we skip. The tab bar then
    // falls through to the raw-contexts render path, which is the
    // baseline-fallback behavior per AGENTS.md.
    if let Ok(entries) = typed_list_windows_attach(&mut client).await {
        let windows: Vec<bmux_windows_plugin_api::windows_list::WindowListEntry> = entries
            .into_iter()
            .filter_map(|entry| {
                let id = Uuid::parse_str(&entry.id).ok()?;
                Some(bmux_windows_plugin_api::windows_list::WindowListEntry {
                    id,
                    name: entry.name,
                    active: entry.active,
                })
            })
            .collect();
        let snapshot = bmux_windows_plugin_api::windows_list::WindowListSnapshot {
            windows,
            revision: 0,
        };
        view_state.cached_window_list = Some(std::sync::Arc::new(snapshot.clone()));
        let _ = bmux_plugin::global_event_bus()
            .publish_state(&bmux_windows_plugin_api::windows_list::STATE_KIND, snapshot);
    }

    loop {
        // ── Event-driven select: sleep until something happens ────────
        tokio::select! {
            // Server-pushed events (layout changes, session events, pane output)
            event = client.event_receiver().recv() => {
                let Some(server_event) = event else {
                    // Event stream closed — server disconnected.
                    exit_reason = AttachExitReason::StreamClosed;
                    break;
                };

                // PaneOutputAvailable sets a flag; fall through to the
                // post-event processing block which fetches output.
                if matches!(
                    server_event,
                    bmux_client::ServerEvent::PaneOutputAvailable { .. }
                ) {
                    pane_output_pending = true;
                    // Fall through to post-event processing (no event dispatch needed).
                } else if let bmux_client::ServerEvent::PaneOutput {
                    pane_id,
                    ref data,
                    stream_start,
                    stream_end,
                    stream_gap,
                    sync_update_active,
                    ..
                } = server_event
                {
                    // Inline output push — apply using the same continuity
                    // checks as batch chunks so parser state remains
                    // deterministic even under cursor gaps or out-of-order
                    // delivery.
                    let mut render = false;
                    match apply_attach_output_chunk(
                        &mut view_state,
                        pane_id,
                        data,
                        AttachOutputChunkMeta {
                            stream_start,
                            stream_end,
                            stream_gap,
                            sync_update_active,
                        },
                        &mut render,
                    ) {
                        AttachChunkApplyOutcome::Applied { .. } | AttachChunkApplyOutcome::Stale => {}
                        AttachChunkApplyOutcome::Desync => {
                            recover_attach_output_desync_for_pane(
                                &mut client,
                                &mut view_state,
                                pane_id,
                            )
                            .await?;
                            pane_output_pending = false;
                        }
                    }
                } else if matches!(
                    server_event,
                    bmux_client::ServerEvent::PaneImageAvailable { .. }
                ) {
                    // Image state changed on the server — fetch deltas on the
                    // next render cycle instead of polling every frame.
                    #[cfg(any(
                        feature = "image-sixel",
                        feature = "image-kitty",
                        feature = "image-iterm2"
                    ))]
                    {
                        image_fetch_pending = true;
                    }
                } else if let bmux_client::ServerEvent::RecordingStarted {
                    recording_id,
                    ref path,
                } = server_event
                {
                    let target = bmux_ipc::RecordingCaptureTarget {
                        recording_id,
                        path: path.clone(),
                        rolling_window_secs: None,
                    };
                    display_capture.open_target(&target, self_client_id);
                } else if let bmux_client::ServerEvent::RecordingStopped {
                    recording_id,
                } = server_event
                {
                    display_capture.close_recording(recording_id);
                } else if let bmux_client::ServerEvent::PerformanceSettingsUpdated {
                    ref settings,
                } = server_event
                {
                    perf_emitter.update_settings(recording::PerfCaptureSettings::from_runtime_settings(
                        settings,
                    ));
                } else if let bmux_client::ServerEvent::PluginBusEvent {
                    ref kind,
                    ref payload,
                } = server_event
                {
                    // Server-forwarded plugin event. Decode by kind
                    // and re-emit onto the local event bus so any
                    // render extension (decoration, future overlays,
                    // etc.) subscribing via
                    // `EventBus::subscribe::<T>` sees the payload.
                    // Core code does NOT interpret the payload itself
                    // — extensions own that.
                    if kind.as_str()
                        == bmux_scene_protocol::scene_protocol::EVENT_KIND.as_str()
                    {
                        match serde_json::from_slice::<
                            bmux_scene_protocol::scene_protocol::EventPayload,
                        >(payload)
                        {
                            Ok(scene) => {
                                // Re-emit on the local event bus so
                                // render extensions pick it up. When
                                // no subscriber has registered the
                                // channel the emit returns
                                // `ChannelNotRegistered`; we ignore
                                // that so builds without extensions
                                // stay silent.
                                let _ = bmux_plugin::global_event_bus().emit(
                                    &bmux_scene_protocol::scene_protocol::EVENT_KIND,
                                    scene,
                                );
                                // Any new scene makes the frame dirty
                                // so render extensions get a chance
                                // to repaint.
                                view_state.dirty.full_pane_redraw = true;
                            }
                            Err(error) => {
                                tracing::warn!(
                                    kind = %kind,
                                    error = %error,
                                    "decoding forwarded scene-protocol payload",
                                );
                            }
                        }
                    } else if kind.as_str()
                        == bmux_contexts_plugin_api::contexts_events::EVENT_KIND.as_str()
                    {
                        // Attach-retarget on context lifecycle events.
                        // `create-context` and `select-context` each emit
                        // two events of interest here: `Selected` (for
                        // the initiating client) and
                        // `SessionActiveContextChanged` (multi-client
                        // broadcast). We act on whichever arrives first
                        // per (session, context) pair and dedup via
                        // `attached_context_id` — retargeting to the
                        // already-attached context is a no-op but wastes
                        // round-trips.
                        match serde_json::from_slice::<
                            bmux_contexts_plugin_api::contexts_events::ContextEvent,
                        >(payload)
                        {
                            Ok(event) => {
                                handle_context_event_forwarded(
                                    &mut client,
                                    &mut view_state,
                                    &event,
                                    self_client_id,
                                    &attach_config,
                                )
                                .await?;
                            }
                            Err(error) => {
                                tracing::warn!(
                                    kind = %kind,
                                    error = %error,
                                    "decoding forwarded contexts-events payload",
                                );
                            }
                        }
                    } else if kind.as_str()
                        == bmux_windows_plugin_api::windows_list::STATE_KIND.as_str()
                    {
                        // Server-forwarded windows-list snapshot.
                        // Decode and republish on the local state
                        // channel. The attach loop's
                        // `subscribe_state::<WindowListSnapshot>`
                        // arm wakes and refreshes
                        // `cached_window_list`, which the tab-bar
                        // renderer reads on the next draw.
                        match serde_json::from_slice::<
                            bmux_windows_plugin_api::windows_list::WindowListSnapshot,
                        >(payload)
                        {
                            Ok(snapshot) => {
                                let _ = bmux_plugin::global_event_bus().publish_state(
                                    &bmux_windows_plugin_api::windows_list::STATE_KIND,
                                    snapshot,
                                );
                            }
                            Err(error) => {
                                tracing::warn!(
                                    kind = %kind,
                                    error = %error,
                                    "decoding forwarded windows-list payload",
                                );
                            }
                        }
                    }
                } else {
                    if let bmux_client::ServerEvent::AttachViewChanged { .. } = &server_event {
                        pane_output_pending = true;
                    }

                    match handle_attach_loop_event(
                        AttachLoopEvent::Server(server_event),
                        &mut client,
                        &mut attach_input_processor,
                        follow_target_id,
                        Some(self_client_id),
                        global,
                        &attach_help_lines,
                        &mut view_state,
                        &mut theme_state,
                        &mut display_capture,
                        kernel_client_factory.as_ref(),
                    )
                    .await?
                    {
                        AttachLoopControl::Continue => {}
                        AttachLoopControl::Break(reason) => {
                            exit_reason = reason;
                            break;
                        }
                    }
                }
            }

            // Terminal input (keyboard, mouse, resize) via async EventStream.
            terminal_result = terminal_stream.next() => {
                let Some(result) = terminal_result else {
                    // Terminal stream ended unexpectedly.
                    exit_reason = AttachExitReason::StreamClosed;
                    break;
                };
                let terminal_event = result.context("failed reading terminal event")?;

                if let Event::Resize(cols, rows) = terminal_event {
                    display_capture.record_resize(cols, rows);
                }

                match handle_attach_loop_event(
                    AttachLoopEvent::Terminal(terminal_event),
                    &mut client,
                    &mut attach_input_processor,
                    follow_target_id,
                    Some(self_client_id),
                    global,
                    &attach_help_lines,
                    &mut view_state,
                    &mut theme_state,
                    &mut display_capture,
                    kernel_client_factory.as_ref(),
                )
                .await?
                {
                    AttachLoopControl::Continue => {}
                    AttachLoopControl::Break(reason) => {
                        exit_reason = reason;
                        break;
                    }
                }
            }

            prompt_request = prompt_host_rx.recv() => {
                if let Some(prompt_request) = prompt_request {
                    view_state.prompt.enqueue_external(prompt_request);
                    view_state.dirty.status_needs_redraw = true;
                    view_state.dirty.overlay_needs_redraw = true;
                }
            }

            dispatch_request = action_dispatch_rx.recv() => {
                if let Some(dispatch_request) = dispatch_request {
                    match handle_attach_loop_event(
                        AttachLoopEvent::ActionDispatch(dispatch_request),
                        &mut client,
                        &mut attach_input_processor,
                        follow_target_id,
                        Some(self_client_id),
                        global,
                        &attach_help_lines,
                        &mut view_state,
                        &mut theme_state,
                        &mut display_capture,
                        kernel_client_factory.as_ref(),
                    )
                    .await?
                    {
                        AttachLoopControl::Continue => {}
                        AttachLoopControl::Break(reason) => {
                            exit_reason = reason;
                            break;
                        }
                    }
                }
            }

            // Scene events pushed on the local event bus. The
            // PluginBusEvent handler above emits incoming scenes
            // onto the same channel; render extensions subscribe
            // independently. Core observes the stream only to mark
            // the frame dirty so the renderer consults extensions
            // on the next pass.
            scene_result = async {
                match &mut scene_event_rx {
                    Some(rx) => rx.recv().await.ok(),
                    None => std::future::pending().await,
                }
            } => {
                if scene_result.is_some() {
                    view_state.dirty.full_pane_redraw = true;
                } else {
                    // Broadcast lagged/closed — drop subscription.
                    scene_event_rx = None;
                }
            }

            // Windows-plugin published a new ordered window list.
            // The tab bar reads `cached_window_list` on every render;
            // capturing the latest value here + marking the status as
            // needing redraw is all the work required — no polling,
            // no re-invocation, no round-trip.
            window_list_result = async {
                match &mut window_list_rx {
                    Some(rx) => rx.changed().await.ok().map(|()| rx.borrow().clone()),
                    None => std::future::pending().await,
                }
            } => {
                if let Some(snapshot) = window_list_result {
                    view_state.cached_window_list = Some(snapshot);
                    view_state.dirty.status_needs_redraw = true;
                } else {
                    // Channel closed — drop subscription; fallback
                    // path in build_attach_tabs_from_catalog covers
                    // the plugin-absent case.
                    window_list_rx = None;
                }
            }

        }

        // ── Post-event processing: layout, output fetch, render ──────

        let _ = view_state.clear_expired_transient_status(Instant::now());

        let mut frame_needs_render = view_state.dirty.status_needs_redraw
            || view_state.dirty.full_pane_redraw
            || view_state.dirty.overlay_needs_redraw
            || !view_state.dirty.pane_dirty_ids.is_empty();

        let mut scene_hydrated = false;

        if view_state.dirty.layout_needs_refresh || view_state.cached_layout_state.is_none() {
            let previous_layout = view_state.cached_layout_state.clone();
            let layout_state = match client.attach_layout(view_state.attached_id).await {
                Ok(state) => state,
                Err(error)
                    if is_attach_stream_closed_error(&error)
                        || is_attach_not_attached_runtime_error(&error) =>
                {
                    exit_reason = AttachExitReason::StreamClosed;
                    break;
                }
                Err(error) => return Err(map_attach_client_error(error)),
            };
            if view_state.cached_layout_state.as_ref() != Some(&layout_state) {
                frame_needs_render = true;
                let pane_ids = visible_scene_pane_ids(&layout_state.scene);
                for pane_id in pane_ids {
                    view_state.dirty.pane_dirty_ids.insert(pane_id);
                }
                match previous_layout.as_ref() {
                    None => {
                        view_state.dirty.full_pane_redraw = true;
                    }
                    Some(previous) => {
                        if previous.scene != layout_state.scene {
                            let revealed_pane_ids = attach_scene_revealed_pane_ids(
                                &previous.scene,
                                &layout_state.scene,
                            );
                            if attach_config.behavior.pane_restore_method
                                == PaneRestoreMethod::Snapshot
                            {
                                if attach_layout_requires_snapshot_hydration(
                                    previous,
                                    &layout_state,
                                ) {
                                    hydrate_attach_state_from_snapshot(
                                        &mut client,
                                        &mut view_state,
                                    )
                                    .await?;
                                    scene_hydrated = true;
                                } else if !revealed_pane_ids.is_empty() {
                                    if client.supports_capability(CAPABILITY_ATTACH_PANE_SNAPSHOT) {
                                        let revealed =
                                            revealed_pane_ids.into_iter().collect::<Vec<_>>();
                                        hydrate_attach_revealed_panes_from_snapshot(
                                            &mut client,
                                            &mut view_state,
                                            &layout_state,
                                            &revealed,
                                        )
                                        .await?;
                                    } else {
                                        hydrate_attach_state_from_snapshot(
                                            &mut client,
                                            &mut view_state,
                                        )
                                        .await?;
                                        scene_hydrated = true;
                                    }
                                }
                            }

                            if !scene_hydrated {
                                view_state.dirty.full_pane_redraw = true;
                            }
                        } else if previous.focused_pane_id != layout_state.focused_pane_id {
                            view_state
                                .dirty
                                .pane_dirty_ids
                                .insert(previous.focused_pane_id);
                            view_state
                                .dirty
                                .pane_dirty_ids
                                .insert(layout_state.focused_pane_id);
                        }
                    }
                }
                if !scene_hydrated {
                    view_state.mouse.last_focused_pane_id = Some(layout_state.focused_pane_id);
                    view_state.cached_layout_state = Some(layout_state.clone());
                }
            }
            // Publish the new layout on the attach-layout state
            // channel. Render extensions (decoration, overlays, etc.)
            // subscribe to this channel and react without core needing
            // to know which plugins care.
            let current_pane_ids = notify_extensions_of_layout(
                &mut client,
                previous_layout.as_ref(),
                view_state.cached_layout_state.as_ref(),
            )
            .await;
            drop(current_pane_ids);
            view_state.dirty.layout_needs_refresh = false;

            // Reset image sequences on layout change so the next fetch
            // gets a full snapshot from the server (handles zoom/unzoom).
            #[cfg(any(
                feature = "image-sixel",
                feature = "image-kitty",
                feature = "image-iterm2"
            ))]
            {
                view_state.image_sequences.clear();
                view_state.pane_images.clear();
                view_state.kitty_host_state.transmitted.clear();
                image_fetch_pending = true;
            }
        }

        let Some(layout_state) = view_state.cached_layout_state.clone() else {
            continue;
        };

        if scene_hydrated {
            let help_scroll = view_state.help_overlay_scroll;
            let render_started_at = Instant::now();
            render_attach_frame(
                &mut client,
                &mut view_state,
                &layout_state,
                &attach_config.status_bar,
                &theme_state.active_theme,
                follow_target_id,
                global,
                &attach_keymap,
                &attach_help_lines,
                help_scroll,
                &mut display_capture,
            )?;
            let render_ms = duration_millis_u64(render_started_at.elapsed());
            perf_window.record_render_frame(render_ms);
            rendered_frame_count = rendered_frame_count.saturating_add(1);
            maybe_emit_attach_frame_perf(
                &mut perf_emitter,
                &mut client,
                view_state.attached_id,
                attach_started_at,
                rendered_frame_count,
                render_ms,
                true,
                &mut first_frame_emitted,
                &mut interactive_ready_emitted,
            )
            .await?;
            maybe_emit_attach_perf_window(
                &mut perf_emitter,
                &mut client,
                view_state.attached_id,
                &mut perf_window,
            )
            .await?;
            pane_output_pending = false;
            continue;
        }

        resize_attach_parsers_for_scene(&mut view_state.pane_buffers, &layout_state.scene);

        // Only fetch pane output when new pane bytes are pending.
        // Pure redraw dirty flags (layout/status/overlay) must not trigger
        // pane-output IPC on their own.
        if pane_output_pending {
            let pane_ids = visible_scene_pane_ids(&layout_state.scene);
            let active_pane_ids = attach_layout_pane_id_set(&layout_state);
            view_state
                .pane_buffers
                .retain(|pane_id, _| active_pane_ids.contains(pane_id));
            view_state
                .pane_mouse_protocol_hints
                .retain(|pane_id, _| active_pane_ids.contains(pane_id));
            #[cfg(any(
                feature = "image-sixel",
                feature = "image-kitty",
                feature = "image-iterm2"
            ))]
            view_state
                .pane_images
                .retain(|pane_id, _| active_pane_ids.contains(pane_id));

            // Drain all available pane output before rendering to avoid
            // visible tearing from partial redraws.  TUI programs like
            // lazygit can emit 20-30 KB when switching views; with 8 KB
            // per fetch we need a few rounds to consume the full burst.
            //
            // Two server-side signals tell us the burst is not yet complete:
            //  1. `output_still_pending` — the PTY reader has flagged new
            //     data that was not included in the batch.
            //  2. `sync_update_active` per pane — the server's byte-by-byte
            //     CSI parser has seen `\x1b[?2026h` but not `\x1b[?2026l`.
            //
            // We keep draining while either signal is active, bounded by a
            // time budget to keep the event loop responsive.
            let mut last_round_had_data = false;
            let drain_start = Instant::now();
            for _round in 0..ATTACH_OUTPUT_DRAIN_MAX_ROUNDS {
                perf_window.record_drain_round();
                let drain_call_started_at = Instant::now();
                let result = match client
                    .attach_pane_output_batch(
                        view_state.attached_id,
                        pane_ids.clone(),
                        ATTACH_OUTPUT_BATCH_MAX_BYTES,
                    )
                    .await
                {
                    Ok(result) => result,
                    Err(error)
                        if is_attach_stream_closed_error(&error)
                            || is_attach_not_attached_runtime_error(&error) =>
                    {
                        #[allow(unused_assignments)] // Read after breaking inner drain loop
                        {
                            exit_reason = AttachExitReason::StreamClosed;
                        }
                        last_round_had_data = false;
                        break;
                    }
                    Err(error) => return Err(map_attach_client_error(error)),
                };
                let batch_bytes: usize = result.chunks.iter().map(|chunk| chunk.data.len()).sum();
                perf_window.record_drain_ipc(
                    duration_millis_u64(drain_call_started_at.elapsed()),
                    batch_bytes,
                );

                let mut had_data = false;
                let mut any_sync_active = false;
                let mut desynced_pane_id = None;
                for chunk in result.chunks {
                    match apply_attach_output_chunk(
                        &mut view_state,
                        chunk.pane_id,
                        &chunk.data,
                        AttachOutputChunkMeta {
                            stream_start: chunk.stream_start,
                            stream_end: chunk.stream_end,
                            stream_gap: chunk.stream_gap,
                            sync_update_active: chunk.sync_update_active,
                        },
                        &mut frame_needs_render,
                    ) {
                        AttachChunkApplyOutcome::Applied {
                            had_data: chunk_had_data,
                        } => {
                            had_data |= chunk_had_data;
                            any_sync_active |= chunk.sync_update_active;
                        }
                        AttachChunkApplyOutcome::Stale => {}
                        AttachChunkApplyOutcome::Desync => {
                            desynced_pane_id = Some(chunk.pane_id);
                            break;
                        }
                    }
                }

                if let Some(desynced_pane_id) = desynced_pane_id {
                    recover_attach_output_desync_for_pane(
                        &mut client,
                        &mut view_state,
                        desynced_pane_id,
                    )
                    .await?;
                    last_round_had_data = false;
                    break;
                }

                perf_window.record_drain_result(had_data, any_sync_active);
                last_round_had_data = had_data;

                if !had_data {
                    // No data this round.  Check whether the burst is truly
                    // complete before breaking out of the drain loop.
                    if !result.output_still_pending && !any_sync_active {
                        break; // Burst complete.
                    }

                    // More data expected — continue if within time budget.
                    // Each IPC round-trip gives the PTY reader thread CPU
                    // time to push pending data, so no explicit yield needed.
                    if drain_start.elapsed() >= ATTACH_OUTPUT_DRAIN_TIME_BUDGET {
                        perf_window.record_drain_budget_hit();
                        break; // Safety valve.
                    }
                }
            }
            // Keep output pending if the last round still produced bytes OR
            // if any pane is mid-synchronized-update so the next iteration
            // re-enters the drain immediately.
            let any_sync_still_active = view_state
                .pane_buffers
                .values()
                .any(|b| b.sync_update_in_progress);
            pane_output_pending = last_round_had_data || any_sync_still_active;
        }

        // Fetch image deltas only when the server notified us that image
        // state changed (feature-gated).
        #[cfg(any(
            feature = "image-sixel",
            feature = "image-kitty",
            feature = "image-iterm2"
        ))]
        if image_fetch_pending
            && view_state.host_image_caps.any_supported()
            && !view_state.dirty.pane_dirty_ids.is_empty()
        {
            image_fetch_pending = false;
            let dirty_panes: Vec<Uuid> = view_state.dirty.pane_dirty_ids.iter().copied().collect();
            let sequences: Vec<u64> = dirty_panes
                .iter()
                .map(|id| view_state.image_sequences.get(id).copied().unwrap_or(0))
                .collect();
            if let Ok(deltas) = client
                .attach_pane_images(view_state.attached_id, dirty_panes, sequences)
                .await
            {
                for delta in deltas {
                    if !delta.added.is_empty() || !delta.removed.is_empty() {
                        view_state
                            .image_sequences
                            .insert(delta.pane_id, delta.sequence);
                        // Apply delta incrementally: remove deleted images,
                        // then append newly added ones.
                        let images = view_state.pane_images.entry(delta.pane_id).or_default();
                        if !delta.removed.is_empty() {
                            images.retain(|img| !delta.removed.contains(&img.id));
                        }
                        images.extend(delta.added);
                        frame_needs_render = true;
                    }
                }
            }
        }

        if !frame_needs_render {
            maybe_emit_attach_perf_window(
                &mut perf_emitter,
                &mut client,
                view_state.attached_id,
                &mut perf_window,
            )
            .await?;
            continue;
        }

        let help_scroll = view_state.help_overlay_scroll;
        let render_started_at = Instant::now();
        render_attach_frame(
            &mut client,
            &mut view_state,
            &layout_state,
            &attach_config.status_bar,
            &theme_state.active_theme,
            follow_target_id,
            global,
            &attach_keymap,
            &attach_help_lines,
            help_scroll,
            &mut display_capture,
        )?;
        let render_ms = duration_millis_u64(render_started_at.elapsed());
        perf_window.record_render_frame(render_ms);
        rendered_frame_count = rendered_frame_count.saturating_add(1);
        maybe_emit_attach_frame_perf(
            &mut perf_emitter,
            &mut client,
            view_state.attached_id,
            attach_started_at,
            rendered_frame_count,
            render_ms,
            false,
            &mut first_frame_emitted,
            &mut interactive_ready_emitted,
        )
        .await?;
        maybe_emit_attach_perf_window(
            &mut perf_emitter,
            &mut client,
            view_state.attached_id,
            &mut perf_window,
        )
        .await?;
    }

    if perf_emitter.level_at_least(recording::PerfCaptureLevel::Basic) {
        let mut payload = serde_json::json!({
            "attach_runtime_ms": duration_millis_u64(attach_started_at.elapsed()),
            "exit_reason": attach_exit_reason_label(exit_reason),
            "rendered_frames": rendered_frame_count,
            "first_frame_recorded": first_frame_emitted,
            "interactive_ready_recorded": interactive_ready_emitted,
        });
        if perf_emitter.level_at_least(recording::PerfCaptureLevel::Trace)
            && let Some(object) = payload.as_object_mut()
        {
            object.insert(
                "pending_output_on_exit".to_string(),
                serde_json::Value::from(pane_output_pending),
            );
        }
        perf_emitter
            .emit_with_streaming_client(
                &mut client,
                Some(view_state.attached_id),
                None,
                "attach.exit",
                payload,
            )
            .await?;
    }

    drop(raw_mode_guard);
    restore_terminal_after_attach_ui()?;

    if exit_reason != AttachExitReason::Detached {
        let _ = client.detach().await;
    }
    if follow_target_id.is_some() {
        let _ = bmux_clients_plugin_api::typed_client::unfollow(&mut client).await;
    }
    if let Some(message) = attach_exit_message(exit_reason) {
        println!("{message}");
    }
    display_capture.close_all();
    Ok(AttachRunOutcome {
        status_code: 0,
        exit_reason,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachRunOutcome {
    pub status_code: u8,
    pub exit_reason: AttachExitReason,
}

/// Apply a plugin-command outcome against the attach view state.
///
/// Cross-domain state changes are plugin-to-plugin typed-dispatch
/// calls, not a plugin-emitted effect list. The attach runtime detects
/// that a plugin command changed the current context by observing the
/// before/after `current-context` delta (see
/// [`plugin_fallback_retarget_context_id`] and the caller).
///
/// The function is kept as a no-op-friendly shim so call sites don't
/// need conditional compilation; it always returns `Ok(false)`.
#[allow(clippy::unused_async)] // Keep async to preserve signature for call sites.
pub async fn apply_plugin_command_outcome(
    _client: &mut StreamingBmuxClient,
    _view_state: &mut AttachViewState,
    _outcome: PluginCommandOutcome,
) -> std::result::Result<bool, ClientError> {
    Ok(false)
}

/// React to a `contexts-events` payload forwarded from the server
/// via `ServerEvent::PluginBusEvent`.
///
/// Two event variants drive attach retargeting:
///
/// - `Selected { context_id }` — classic selection delta. We do NOT
///   know which client initiated it from this variant, so we only
///   retarget if the event id differs from the already-attached
///   context. This is a safety net; the normal path uses
///   `SessionActiveContextChanged`.
///
/// - `SessionActiveContextChanged { session_id, context_id,
///   initiator_client_id }` — multi-client-aware broadcast. If the
///   initiator is this client, always retarget (the local user just
///   asked for a new tab). If the initiator is another client, only
///   retarget when the attached session matches AND the user opted
///   into follow semantics via `multi_client.default_follow_mode`.
///
/// Retargeting to the already-attached context is a no-op — early
/// return so we don't thrash viewport / snapshot hydration.
async fn handle_context_event_forwarded(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    event: &bmux_contexts_plugin_api::contexts_events::ContextEvent,
    self_client_id: Uuid,
    attach_config: &BmuxConfig,
) -> std::result::Result<(), ClientError> {
    use bmux_contexts_plugin_api::contexts_events::ContextEvent;
    match event {
        ContextEvent::SessionActiveContextChanged {
            session_id,
            context_id,
            initiator_client_id,
        } => {
            if view_state.attached_context_id == Some(*context_id) {
                return Ok(());
            }
            let is_self = *initiator_client_id == Some(self_client_id);
            if !is_self {
                // Only drag peers when follow-mode is on and they are
                // attached to the same session as the initiator.
                if !attach_config.multi_client.default_follow_mode {
                    return Ok(());
                }
                if view_state.attached_id != *session_id {
                    return Ok(());
                }
            }
            debug!(
                context_id = %context_id,
                session_id = %session_id,
                is_self,
                "attach.context_event.retarget"
            );
            retarget_attach_to_context(client, view_state, *context_id).await?;
            view_state.dirty.layout_needs_refresh = true;
            view_state.dirty.full_pane_redraw = true;
        }
        ContextEvent::Selected { context_id } => {
            // Safety net: if the richer
            // `SessionActiveContextChanged` variant did not arrive
            // (e.g. legacy select path, or a future plugin that only
            // emits `Selected`), still retarget when this client
            // appears to have initiated — we can't know the initiator
            // here, so we rely on the `attached_context_id` diff as a
            // conservative trigger and defer to
            // `SessionActiveContextChanged` for multi-client policy.
            if view_state.attached_context_id == Some(*context_id) {
                return Ok(());
            }
            // Without initiator info we cannot drag peers safely; only
            // retarget self. A reasonable heuristic is "always retarget
            // on Selected" because `Selected` is always scoped to a
            // client, and any plugin emitting it for another client
            // should use the richer variant. Implementers of new
            // selection events should emit
            // `SessionActiveContextChanged` instead.
            debug!(
                context_id = %context_id,
                "attach.context_event.selected_retarget"
            );
            retarget_attach_to_context(client, view_state, *context_id).await?;
            view_state.dirty.layout_needs_refresh = true;
            view_state.dirty.full_pane_redraw = true;
        }
        ContextEvent::Created { .. } | ContextEvent::Closed { .. } => {
            // Creation and closure are informational here. Retarget
            // happens via the accompanying `Selected` /
            // `SessionActiveContextChanged` events that
            // `create_context_local` / `close_context_local` emit.
        }
    }
    Ok(())
}

pub async fn retarget_attach_to_context(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    context_id: Uuid,
) -> std::result::Result<(), ClientError> {
    let started_at = Instant::now();
    debug!(
        from_context_id = ?view_state.attached_context_id,
        from_session_id = %view_state.attached_id,
        to_context_id = %context_id,
        "attach.retarget.start"
    );
    typed_select_context_attach(client, context_id).await?;
    let attach_info = open_attach_for_context(client, context_id).await?;
    view_state.attached_id = attach_info.session_id;
    view_state.attached_context_id = attach_info.context_id.or(Some(context_id));
    view_state.can_write = attach_info.can_write;
    update_attach_viewport(client, view_state.attached_id, view_state.status_position).await?;
    hydrate_attach_state_from_snapshot(client, view_state).await?;
    refresh_attach_status_catalog_best_effort(client, view_state).await;
    view_state.ui_mode = AttachUiMode::Normal;
    let status = attach_context_status_from_catalog(view_state);
    set_attach_context_status(
        view_state,
        status,
        Instant::now(),
        ATTACH_TRANSIENT_STATUS_TTL,
    );
    debug!(
        to_context_id = ?view_state.attached_context_id,
        to_session_id = %view_state.attached_id,
        can_write = view_state.can_write,
        elapsed_ms = started_at.elapsed().as_millis(),
        "attach.retarget.done"
    );
    Ok(())
}

pub fn plugin_fallback_retarget_context_id(
    before_context_id: Option<Uuid>,
    after_context_id: Option<Uuid>,
    attached_context_id: Option<Uuid>,
    outcome_applied: bool,
) -> Option<Uuid> {
    if outcome_applied {
        return None;
    }
    after_context_id
        .filter(|after| Some(*after) != before_context_id && Some(*after) != attached_context_id)
}

pub fn plugin_fallback_new_context_id(
    before_context_ids: Option<&std::collections::BTreeSet<Uuid>>,
    after_context_ids: Option<&std::collections::BTreeSet<Uuid>>,
    attached_context_id: Option<Uuid>,
    after_context_id: Option<Uuid>,
    outcome_applied: bool,
) -> Option<Uuid> {
    if outcome_applied {
        return None;
    }
    let (Some(before), Some(after)) = (before_context_ids, after_context_ids) else {
        return None;
    };

    let mut new_context_ids = after
        .difference(before)
        .copied()
        .filter(|context_id| Some(*context_id) != attached_context_id)
        .collect::<Vec<_>>();

    if new_context_ids.is_empty() {
        return None;
    }
    if new_context_ids.len() == 1 {
        return new_context_ids.pop();
    }

    after_context_id.filter(|context_id| new_context_ids.contains(context_id))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct HotPathExecutionPolicyCheckRequest {
    session_id: Uuid,
    #[serde(default)]
    context_id: Option<Uuid>,
    client_id: Uuid,
    principal_id: Uuid,
    action: String,
    plugin_id: String,
    capability: String,
    execution_class: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct HotPathExecutionPolicyCheckResponse {
    allowed: bool,
    reason: Option<String>,
}

async fn enforce_hot_path_plugin_policy(
    client: &mut StreamingBmuxClient,
    plugin_id: &str,
    command_name: &str,
    attached_session_id: Uuid,
    attached_context_id: Option<Uuid>,
) -> anyhow::Result<()> {
    let hints = plugin_command_policy_hints(plugin_id, command_name).map_err(|error| {
        ClientError::ServerError {
            code: bmux_ipc::ErrorCode::InvalidRequest,
            message: error.to_string(),
        }
    })?;

    if !matches!(
        hints.execution,
        bmux_plugin_sdk::CommandExecutionKind::RuntimeHook
    ) {
        return Ok(());
    }

    if matches!(
        hints.execution_class,
        bmux_plugin::PluginExecutionClass::NativeFast
    ) {
        return Ok(());
    }

    let Some(hot_path_capability) = hints
        .required_capabilities
        .iter()
        .find(|capability| capability.is_hot_path())
    else {
        return Ok(());
    };

    let client_id = bmux_clients_plugin_api::typed_client::whoami(client).await?;
    let principal_info = client.whoami_principal().await?;
    let request = HotPathExecutionPolicyCheckRequest {
        session_id: attached_session_id,
        context_id: attached_context_id,
        client_id,
        principal_id: principal_info.principal_id,
        action: "hot_path_execution".to_string(),
        plugin_id: plugin_id.to_string(),
        capability: hot_path_capability.to_string(),
        execution_class: match hints.execution_class {
            bmux_plugin::PluginExecutionClass::NativeFast => "native_fast",
            bmux_plugin::PluginExecutionClass::NativeStandard => "native_standard",
            bmux_plugin::PluginExecutionClass::Interpreter => "interpreter",
        }
        .to_string(),
    };
    let payload = bmux_plugin_sdk::encode_service_message(&request).map_err(|error| {
        ClientError::ServerError {
            code: bmux_ipc::ErrorCode::Internal,
            message: format!("failed to encode hot-path policy request: {error}"),
        }
    })?;
    let response_payload = client
        .invoke_service_raw(
            "bmux.sessions.policy",
            InvokeServiceKind::Query,
            "session-policy-query/v1",
            "check",
            payload,
        )
        .await?;
    let response: HotPathExecutionPolicyCheckResponse =
        bmux_plugin_sdk::decode_service_message(&response_payload).map_err(|error| {
            ClientError::ServerError {
                code: bmux_ipc::ErrorCode::Internal,
                message: format!("failed to decode hot-path policy response: {error}"),
            }
        })?;
    if response.allowed {
        Ok(())
    } else {
        Err(ClientError::ServerError {
            code: bmux_ipc::ErrorCode::InvalidRequest,
            message: response.reason.unwrap_or_else(|| {
                format!(
                    "hot-path plugin execution denied for {plugin_id}:{command_name}; grant scoped override or use execution_class=native_fast"
                )
            }),
        }
        .into())
    }
}

#[allow(clippy::too_many_lines)]
pub async fn handle_attach_plugin_command_action(
    client: &mut StreamingBmuxClient,
    plugin_id: &str,
    command_name: &str,
    args: &[String],
    view_state: &mut AttachViewState,
    kernel_client_factory: Option<&KernelClientFactory>,
) -> std::result::Result<(), ClientError> {
    let before_context_id = typed_current_context_attach(client)
        .await
        .map_or(None, |context| context.map(|entry| entry.id));
    let before_context_ids = typed_list_contexts_attach(client)
        .await
        .ok()
        .map(|contexts| {
            contexts
                .into_iter()
                .map(|context| context.id)
                .collect::<std::collections::BTreeSet<_>>()
        });
    debug!(
        plugin_id = %plugin_id,
        command_name = %command_name,
        before_context_id = ?before_context_id,
        attached_context_id = ?view_state.attached_context_id,
        attached_session_id = %view_state.attached_id,
        "attach.plugin_command.start"
    );
    if let Err(error) = enforce_hot_path_plugin_policy(
        client,
        plugin_id,
        command_name,
        view_state.attached_id,
        view_state.attached_context_id,
    )
    .await
    {
        warn!(
            plugin_id = %plugin_id,
            command_name = %command_name,
            error = %error,
            attached_context_id = ?view_state.attached_context_id,
            attached_session_id = %view_state.attached_id,
            "attach.plugin_command.policy_denied"
        );
        view_state.set_transient_status(
            format!("plugin action denied by policy: {error:#}"),
            Instant::now(),
            ATTACH_TRANSIENT_STATUS_TTL,
        );
        return Ok(());
    }
    match run_plugin_keybinding_command(
        plugin_id,
        command_name,
        args,
        kernel_client_factory,
        view_state.self_client_id,
    ) {
        Err(error) => {
            warn!(
                plugin_id = %plugin_id,
                command_name = %command_name,
                error = %error,
                "attach.plugin_command.run_failed"
            );
            view_state.set_transient_status(
                format!("plugin action failed ({plugin_id}:{command_name}) — see logs"),
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        Ok(execution) => {
            let status = execution.status;
            if status != 0 {
                // Route the plugin's error text (captured by the SDK
                // into `PluginCommandOutcome.error_message` instead of
                // the stderr-corrupting `eprintln!` path) to the log
                // file at `warn` level. The attach status line shows
                // only a concise, non-corrupting indicator so pane
                // rendering stays intact.
                let error_detail = execution.outcome.error_message.as_deref();
                if let Some(detail) = error_detail {
                    warn!(
                        plugin_id = %plugin_id,
                        command_name = %command_name,
                        status,
                        error = %detail,
                        before_context_id = ?before_context_id,
                        attached_context_id = ?view_state.attached_context_id,
                        attached_session_id = %view_state.attached_id,
                        "attach.plugin_command.nonzero_status"
                    );
                } else {
                    warn!(
                        plugin_id = %plugin_id,
                        command_name = %command_name,
                        status,
                        before_context_id = ?before_context_id,
                        attached_context_id = ?view_state.attached_context_id,
                        attached_session_id = %view_state.attached_id,
                        "attach.plugin_command.nonzero_status"
                    );
                }
                let status_text = if error_detail.is_some() {
                    format!("plugin action failed ({plugin_id}:{command_name}) — see logs")
                } else {
                    format!("plugin action failed ({plugin_id}:{command_name}) exit {status}")
                };
                view_state.set_transient_status(
                    status_text,
                    Instant::now(),
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
                return Ok(());
            }

            let outcome_applied =
                match apply_plugin_command_outcome(client, view_state, execution.outcome).await {
                    Ok(applied) => applied,
                    Err(error) => {
                        view_state.set_transient_status(
                            format!(
                                "plugin outcome apply failed: {}",
                                map_attach_client_error(error)
                            ),
                            Instant::now(),
                            ATTACH_TRANSIENT_STATUS_TTL,
                        );
                        return Ok(());
                    }
                };

            let after_context_id = typed_current_context_attach(client)
                .await
                .map_or(None, |context| context.map(|entry| entry.id));
            let after_context_ids = typed_list_contexts_attach(client)
                .await
                .ok()
                .map(|contexts| {
                    contexts
                        .into_iter()
                        .map(|context| context.id)
                        .collect::<std::collections::BTreeSet<_>>()
                });
            debug!(
                plugin_id = %plugin_id,
                command_name = %command_name,
                outcome_applied,
                before_context_id = ?before_context_id,
                after_context_id = ?after_context_id,
                attached_context_id = ?view_state.attached_context_id,
                attached_session_id = %view_state.attached_id,
                "attach.plugin_command.outcome"
            );

            if let Some(fallback_context_id) = plugin_fallback_retarget_context_id(
                before_context_id,
                after_context_id,
                view_state.attached_context_id,
                outcome_applied,
            ) {
                debug!(
                    plugin_id = %plugin_id,
                    command_name = %command_name,
                    fallback_context_id = %fallback_context_id,
                    "attach.plugin_command.fallback_retarget"
                );
                if let Err(error) =
                    retarget_attach_to_context(client, view_state, fallback_context_id).await
                {
                    warn!(
                        plugin_id = %plugin_id,
                        command_name = %command_name,
                        fallback_context_id = %fallback_context_id,
                        error = %error,
                        "attach.plugin_command.fallback_retarget_failed"
                    );
                    view_state.set_transient_status(
                        format!(
                            "plugin fallback retarget failed: {}",
                            map_attach_client_error(error)
                        ),
                        Instant::now(),
                        ATTACH_TRANSIENT_STATUS_TTL,
                    );
                    return Ok(());
                }
                view_state.set_transient_status(
                    format!("plugin action: {plugin_id}:{command_name} (fallback retarget)"),
                    Instant::now(),
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
                view_state.dirty.layout_needs_refresh = true;
                view_state.dirty.full_pane_redraw = true;
                return Ok(());
            }

            if let Some(fallback_context_id) = plugin_fallback_new_context_id(
                before_context_ids.as_ref(),
                after_context_ids.as_ref(),
                view_state.attached_context_id,
                after_context_id,
                outcome_applied,
            ) {
                debug!(
                    plugin_id = %plugin_id,
                    command_name = %command_name,
                    fallback_context_id = %fallback_context_id,
                    "attach.plugin_command.new_context_fallback_retarget"
                );
                if let Err(error) =
                    retarget_attach_to_context(client, view_state, fallback_context_id).await
                {
                    warn!(
                        plugin_id = %plugin_id,
                        command_name = %command_name,
                        fallback_context_id = %fallback_context_id,
                        error = %error,
                        "attach.plugin_command.new_context_fallback_retarget_failed"
                    );
                    view_state.set_transient_status(
                        format!(
                            "plugin new-context fallback failed: {}",
                            map_attach_client_error(error)
                        ),
                        Instant::now(),
                        ATTACH_TRANSIENT_STATUS_TTL,
                    );
                    return Ok(());
                }
                view_state.set_transient_status(
                    format!("plugin action: {plugin_id}:{command_name} (new context retarget)"),
                    Instant::now(),
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
                view_state.dirty.layout_needs_refresh = true;
                view_state.dirty.full_pane_redraw = true;
                return Ok(());
            }

            view_state.set_transient_status(
                format!("plugin action: {plugin_id}:{command_name}"),
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
            view_state.dirty.layout_needs_refresh = true;
            view_state.dirty.full_pane_redraw = true;
        }
    }

    Ok(())
}

#[allow(clippy::too_many_lines)]
pub async fn handle_attach_ui_action(
    client: &mut StreamingBmuxClient,
    action: RuntimeAction,
    view_state: &mut AttachViewState,
) -> std::result::Result<(), ClientError> {
    match action {
        RuntimeAction::EnterWindowMode => {
            view_state.set_transient_status(
                "workspace mode unavailable in core baseline",
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        RuntimeAction::EnterScrollMode => {
            if enter_attach_scrollback(view_state) {
            } else {
                view_state.set_transient_status(
                    ATTACH_SCROLLBACK_UNAVAILABLE_STATUS,
                    Instant::now(),
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
            }
        }
        RuntimeAction::ExitScrollMode => {
            if view_state.selection_active() {
                clear_attach_selection(view_state, true);
            } else {
                view_state.exit_scrollback();
            }
        }
        RuntimeAction::ScrollUpLine => {
            step_attach_scrollback(view_state, -1);
        }
        RuntimeAction::ScrollDownLine => {
            step_attach_scrollback(view_state, 1);
        }
        RuntimeAction::ScrollUpPage => {
            step_attach_scrollback(
                view_state,
                -(attach_scrollback_page_size(view_state).cast_signed()),
            );
        }
        RuntimeAction::ScrollDownPage => {
            step_attach_scrollback(
                view_state,
                attach_scrollback_page_size(view_state).cast_signed(),
            );
        }
        RuntimeAction::ScrollTop => {
            if view_state.scrollback_active {
                view_state.scrollback_offset = max_attach_scrollback(view_state);
                clamp_attach_scrollback_cursor(view_state);
            }
        }
        RuntimeAction::ScrollBottom => {
            if view_state.scrollback_active {
                view_state.scrollback_offset = 0;
                clamp_attach_scrollback_cursor(view_state);
            }
        }
        RuntimeAction::MoveCursorLeft => {
            move_attach_scrollback_cursor_horizontal(view_state, -1);
        }
        RuntimeAction::MoveCursorRight => {
            move_attach_scrollback_cursor_horizontal(view_state, 1);
        }
        RuntimeAction::MoveCursorUp => {
            move_attach_scrollback_cursor_vertical(view_state, -1);
        }
        RuntimeAction::MoveCursorDown => {
            move_attach_scrollback_cursor_vertical(view_state, 1);
        }
        RuntimeAction::BeginSelection => {
            if begin_attach_selection(view_state) {
                view_state.set_transient_status(
                    ATTACH_SELECTION_STARTED_STATUS,
                    Instant::now(),
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
            }
        }
        RuntimeAction::CopyScrollback => {
            copy_attach_selection(view_state, false);
        }
        RuntimeAction::ConfirmScrollback => {
            confirm_attach_scrollback(view_state);
        }
        RuntimeAction::SwitchProfile(_) => {
            view_state.set_transient_status(
                "switch_profile is handled by attach input loop",
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        RuntimeAction::Quit => {
            if view_state.prompt.is_busy() {
                view_state.set_transient_status(
                    "prompt already active",
                    Instant::now(),
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
                return Ok(());
            }
            view_state.prompt.enqueue_internal(
                PromptRequest::confirm("Quit session and all panes?")
                    .message("This will terminate the attached session and every pane.")
                    .submit_label("Quit")
                    .cancel_label("Cancel")
                    .confirm_default(false)
                    .policy(prompt::PromptPolicy::RejectIfBusy),
                AttachInternalPromptAction::QuitSession,
            );
        }
        RuntimeAction::WindowPrev
        | RuntimeAction::WindowNext
        | RuntimeAction::WindowGoto1
        | RuntimeAction::WindowGoto2
        | RuntimeAction::WindowGoto3
        | RuntimeAction::WindowGoto4
        | RuntimeAction::WindowGoto5
        | RuntimeAction::WindowGoto6
        | RuntimeAction::WindowGoto7
        | RuntimeAction::WindowGoto8
        | RuntimeAction::WindowGoto9
        | RuntimeAction::WindowClose => {
            view_state.exit_scrollback();
        }
        RuntimeAction::SplitFocusedVertical => {
            let selector = attached_session_selector(view_state);
            let _ack: bmux_windows_plugin_api::windows_commands::PaneAck = invoke_windows_command(
                client,
                "split-pane",
                &windows_cmd_args::SplitPane {
                    session: Some(ipc_to_typed_selector(selector)),
                    target: None,
                    direction: ipc_split_to_typed_direction(PaneSplitDirection::Vertical),
                    ratio_pct: None,
                },
            )
            .await?;
        }
        RuntimeAction::SplitFocusedHorizontal => {
            let selector = attached_session_selector(view_state);
            let _ack: bmux_windows_plugin_api::windows_commands::PaneAck = invoke_windows_command(
                client,
                "split-pane",
                &windows_cmd_args::SplitPane {
                    session: Some(ipc_to_typed_selector(selector)),
                    target: None,
                    direction: ipc_split_to_typed_direction(PaneSplitDirection::Horizontal),
                    ratio_pct: None,
                },
            )
            .await?;
        }
        RuntimeAction::FocusNext
        | RuntimeAction::FocusPrev
        | RuntimeAction::FocusLeft
        | RuntimeAction::FocusRight
        | RuntimeAction::FocusUp
        | RuntimeAction::FocusDown => {
            let direction = if matches!(
                action,
                RuntimeAction::FocusLeft | RuntimeAction::FocusUp | RuntimeAction::FocusPrev
            ) {
                PaneFocusDirection::Prev
            } else {
                PaneFocusDirection::Next
            };
            let selector = attached_session_selector(view_state);
            let _ack: bmux_windows_plugin_api::windows_commands::PaneAck = invoke_windows_command(
                client,
                "focus-pane-in-direction",
                &windows_cmd_args::FocusPaneInDirection {
                    session: Some(ipc_to_typed_selector(selector)),
                    direction: typed_windows::ipc_focus_to_typed_direction(direction),
                },
            )
            .await?;
        }
        RuntimeAction::IncreaseSplit
        | RuntimeAction::DecreaseSplit
        | RuntimeAction::ResizeLeft
        | RuntimeAction::ResizeRight
        | RuntimeAction::ResizeUp
        | RuntimeAction::ResizeDown => {
            let delta = if matches!(
                action,
                RuntimeAction::IncreaseSplit
                    | RuntimeAction::ResizeRight
                    | RuntimeAction::ResizeDown
            ) {
                1
            } else {
                -1
            };
            let selector = attached_session_selector(view_state);
            let _ack: bmux_windows_plugin_api::windows_commands::PaneAck = invoke_windows_command(
                client,
                "resize-pane",
                &windows_cmd_args::ResizePane {
                    session: Some(ipc_to_typed_selector(selector)),
                    target: None,
                    delta,
                },
            )
            .await?;
        }
        RuntimeAction::CloseFocusedPane => {
            let Some(pane_id) = focused_attach_pane_id(view_state) else {
                view_state.set_transient_status(
                    "no focused pane",
                    Instant::now(),
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
                return Ok(());
            };
            if view_state.prompt.is_busy() {
                view_state.set_transient_status(
                    "prompt already active",
                    Instant::now(),
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
                return Ok(());
            }
            view_state.prompt.enqueue_internal(
                PromptRequest::confirm("Close focused pane?")
                    .message("This will stop the pane process.")
                    .submit_label("Close")
                    .cancel_label("Cancel")
                    .confirm_default(false)
                    .policy(prompt::PromptPolicy::RejectIfBusy),
                AttachInternalPromptAction::ClosePane { pane_id },
            );
        }
        RuntimeAction::ZoomPane => {
            let selector = attached_session_selector(view_state);
            let ack: bmux_windows_plugin_api::windows_commands::PaneZoomAck =
                invoke_windows_command(
                    client,
                    "zoom-pane",
                    &windows_cmd_args::ZoomPane {
                        session: Some(ipc_to_typed_selector(selector)),
                    },
                )
                .await?;
            let status = if ack.zoomed {
                "Pane zoomed"
            } else {
                "Zoom exited"
            };
            view_state.set_transient_status(status, Instant::now(), ATTACH_TRANSIENT_STATUS_TTL);
        }
        RuntimeAction::RestartFocusedPane => {
            #[derive(serde::Serialize)]
            struct Args {
                selector: Option<bmux_ipc::SessionSelector>,
            }
            let selector = attached_session_selector(view_state);
            // Typed dispatch replaces the legacy `BmuxClient::restart_pane`
            // convenience method; route through
            // `windows-commands:restart-pane` directly.
            let payload = bmux_codec::to_vec(&Args {
                selector: Some(selector),
            })
            .map_err(|error| ClientError::ServerError {
                code: bmux_ipc::ErrorCode::Internal,
                message: format!("encoding restart-pane args: {error}"),
            })?;
            let _response_bytes = client
                .invoke_service_raw(
                    typed_windows::WINDOWS_WRITE_CAPABILITY.as_str(),
                    typed_windows::COMMAND_KIND,
                    typed_windows::WINDOWS_COMMANDS_INTERFACE.as_str(),
                    "restart-pane",
                    payload,
                )
                .await?;
            view_state.set_transient_status(
                "pane restarted",
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        _ => {}
    }

    Ok(())
}

pub fn enter_attach_scrollback(view_state: &mut AttachViewState) -> bool {
    let Some((inner_w, inner_h)) = focused_attach_pane_inner_size(view_state) else {
        return false;
    };
    let Some(buffer) = focused_attach_pane_buffer(view_state) else {
        return false;
    };
    let (row, col) = buffer.parser.screen().cursor_position();
    view_state.scrollback_active = true;
    view_state.scrollback_offset = 0;
    view_state.scrollback_cursor = Some(AttachScrollbackCursor {
        row: usize::from(row).min(inner_h.saturating_sub(1)),
        col: usize::from(col).min(inner_w.saturating_sub(1)),
    });
    view_state.selection_anchor = None;
    true
}

pub fn begin_attach_selection(view_state: &mut AttachViewState) -> bool {
    if !view_state.scrollback_active {
        return false;
    }
    view_state.selection_anchor = attach_scrollback_cursor_absolute_position(view_state);
    view_state.selection_anchor.is_some()
}

pub fn clear_attach_selection(view_state: &mut AttachViewState, show_status: bool) {
    view_state.selection_anchor = None;
    if show_status {
        view_state.set_transient_status(
            ATTACH_SELECTION_CLEARED_STATUS,
            Instant::now(),
            ATTACH_TRANSIENT_STATUS_TTL,
        );
    }
}

pub fn attach_scrollback_cursor_absolute_position(
    view_state: &AttachViewState,
) -> Option<AttachScrollbackPosition> {
    let cursor = view_state.scrollback_cursor?;
    Some(AttachScrollbackPosition {
        row: view_state.scrollback_offset.saturating_add(cursor.row),
        col: cursor.col,
    })
}

pub fn attach_selection_bounds(
    view_state: &AttachViewState,
) -> Option<(AttachScrollbackPosition, AttachScrollbackPosition)> {
    let anchor = view_state.selection_anchor?;
    let head = attach_scrollback_cursor_absolute_position(view_state)?;
    Some(if anchor <= head {
        (anchor, head)
    } else {
        (head, anchor)
    })
}

pub fn step_attach_scrollback(view_state: &mut AttachViewState, delta: isize) {
    if !view_state.scrollback_active {
        return;
    }
    let max_offset = max_attach_scrollback(view_state);
    view_state.scrollback_offset =
        adjust_attach_scrollback_offset(view_state.scrollback_offset, delta, max_offset);
    clamp_attach_scrollback_cursor(view_state);
}

pub fn move_attach_scrollback_cursor_horizontal(view_state: &mut AttachViewState, delta: isize) {
    if !view_state.scrollback_active {
        return;
    }
    let Some((inner_w, _)) = focused_attach_pane_inner_size(view_state) else {
        return;
    };
    let Some(cursor) = view_state.scrollback_cursor.as_mut() else {
        return;
    };
    cursor.col = adjust_scrollback_cursor_component(cursor.col, delta, inner_w.saturating_sub(1));
}

pub fn move_attach_scrollback_cursor_vertical(view_state: &mut AttachViewState, delta: isize) {
    if !view_state.scrollback_active || delta == 0 {
        return;
    }
    let Some((_, inner_h)) = focused_attach_pane_inner_size(view_state) else {
        return;
    };
    let max_offset = max_attach_scrollback(view_state);
    let Some(cursor) = view_state.scrollback_cursor.as_mut() else {
        return;
    };

    if delta < 0 {
        for _ in 0..delta.unsigned_abs() {
            if cursor.row > 0 {
                cursor.row -= 1;
            } else if view_state.scrollback_offset < max_offset {
                view_state.scrollback_offset += 1;
            }
        }
    } else {
        for _ in 0..(delta.cast_unsigned()) {
            if cursor.row + 1 < inner_h {
                cursor.row += 1;
            } else if view_state.scrollback_offset > 0 {
                view_state.scrollback_offset -= 1;
            }
        }
    }

    clamp_attach_scrollback_cursor(view_state);
}

pub fn adjust_scrollback_cursor_component(current: usize, delta: isize, max_value: usize) -> usize {
    if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        current.saturating_add(delta.cast_unsigned()).min(max_value)
    }
}

pub fn copy_attach_selection(view_state: &mut AttachViewState, exit_after_copy: bool) {
    let Some(text) = selected_attach_text(view_state) else {
        if exit_after_copy {
            view_state.exit_scrollback();
        } else {
            view_state.set_transient_status(
                ATTACH_SELECTION_EMPTY_STATUS,
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        return;
    };

    match copy_text_with_clipboard_plugin(&text) {
        Ok(()) => {
            view_state.set_transient_status(
                ATTACH_SELECTION_COPIED_STATUS,
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
            if exit_after_copy {
                view_state.exit_scrollback();
            }
        }
        Err(error) => {
            view_state.set_transient_status(
                format_clipboard_service_error(&error),
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
    }
}

pub fn confirm_attach_scrollback(view_state: &mut AttachViewState) {
    copy_attach_selection(view_state, true);
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ClipboardWriteRequest {
    text: String,
}

pub fn copy_text_with_clipboard_plugin(text: &str) -> Result<()> {
    let config = BmuxConfig::load()?;
    let paths = ConfigPaths::default();
    let registry = scan_available_plugins(&config, &paths)?;
    let services = available_service_descriptors(&config, &registry)?;
    let capability = HostScope::new("bmux.clipboard.write")?;
    let service = services
        .into_iter()
        .find(|entry| {
            entry.capability == capability
                && entry.kind == ServiceKind::Command
                && entry.interface_id == "clipboard-write/v1"
        })
        .context("clipboard service unavailable; ensure a provider is enabled and discoverable")?;

    let provider_plugin_id = match &service.provider {
        bmux_plugin_sdk::ProviderId::Plugin(plugin_id) => plugin_id,
        bmux_plugin_sdk::ProviderId::Host => {
            anyhow::bail!("clipboard service provider must be plugin-owned")
        }
    };
    let provider = registry.get(provider_plugin_id).with_context(|| {
        format!("clipboard service provider '{provider_plugin_id}' was not found")
    })?;

    let payload = bmux_plugin_sdk::encode_service_message(&ClipboardWriteRequest {
        text: text.to_string(),
    })?;
    let enabled_plugins = effective_enabled_plugins(&config, &registry);
    let available_capabilities = available_capability_providers(&config, &registry)?
        .into_keys()
        .map(|entry| entry.to_string())
        .collect::<Vec<_>>();
    let plugin_search_roots = resolve_plugin_search_paths(&config, &paths)?
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let loaded = load_plugin(
        provider,
        &plugin_host_metadata(),
        &available_capability_providers(&config, &registry)?,
    )
    .with_context(|| format!("failed loading clipboard service provider '{provider_plugin_id}'"))?;

    let connection = bmux_plugin_sdk::HostConnectionInfo {
        config_dir: paths.config_dir.to_string_lossy().into_owned(),
        config_dir_candidates: paths
            .config_dir_candidates()
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        runtime_dir: paths.runtime_dir.to_string_lossy().into_owned(),
        data_dir: paths.data_dir.to_string_lossy().into_owned(),
        state_dir: paths.state_dir.to_string_lossy().into_owned(),
    };
    let _host_kernel_connection_guard = enter_host_kernel_connection(connection.clone());
    let response = loaded.invoke_service(&bmux_plugin_sdk::NativeServiceContext {
        plugin_id: provider_plugin_id.clone(),
        request: ServiceRequest {
            caller_plugin_id: "bmux.core".to_string(),
            service,
            operation: "copy_text".to_string(),
            payload,
        },
        required_capabilities: provider
            .declaration
            .required_capabilities
            .iter()
            .map(ToString::to_string)
            .collect(),
        provided_capabilities: provider
            .declaration
            .provided_capabilities
            .iter()
            .map(ToString::to_string)
            .collect(),
        services: available_service_descriptors(&config, &registry)?,
        available_capabilities,
        enabled_plugins,
        plugin_search_roots,
        host: plugin_host_metadata(),
        connection,
        settings: None,
        plugin_settings_map: std::collections::BTreeMap::new(),
        caller_client_id: None,
        host_kernel_bridge: Some(bmux_plugin_sdk::HostKernelBridge::from_fn(
            host_kernel_bridge,
        )),
    })?;
    if let Some(error) = response.error {
        anyhow::bail!(error.message);
    }

    let _: () = bmux_plugin_sdk::decode_service_message(&response.payload)
        .context("failed decoding clipboard service response payload")?;
    Ok(())
}

pub fn format_clipboard_service_error(error: &anyhow::Error) -> String {
    let message = error.to_string();
    if message.contains("clipboard backend unavailable") {
        return "clipboard backend unavailable".to_string();
    }
    if message.starts_with("clipboard copy failed:") {
        return message;
    }
    format!("clipboard copy failed: {message}")
}

pub fn selected_attach_text(view_state: &mut AttachViewState) -> Option<String> {
    let (start, end) = attach_selection_bounds(view_state)?;
    extract_attach_text(view_state, start, end)
}

#[allow(clippy::cast_possible_truncation)] // Terminal dimensions bounded by u16
pub fn extract_attach_text(
    view_state: &mut AttachViewState,
    start: AttachScrollbackPosition,
    end: AttachScrollbackPosition,
) -> Option<String> {
    let buffer = focused_attach_pane_buffer(view_state)?;
    let original_scrollback = buffer.parser.screen().scrollback();
    buffer.parser.screen_mut().set_scrollback(start.row);
    let text = buffer.parser.screen().contents_between(
        0,
        start.col as u16,
        end.row.saturating_sub(start.row) as u16,
        end.col.saturating_add(1) as u16,
    );
    buffer
        .parser
        .screen_mut()
        .set_scrollback(original_scrollback);
    Some(text)
}

pub fn adjust_attach_scrollback_offset(current: usize, delta: isize, max_offset: usize) -> usize {
    if delta < 0 {
        current.saturating_add(delta.unsigned_abs()).min(max_offset)
    } else {
        current.saturating_sub(delta.cast_unsigned())
    }
}

pub fn max_attach_scrollback(view_state: &mut AttachViewState) -> usize {
    let Some(buffer) = focused_attach_pane_buffer(view_state) else {
        return 0;
    };
    let previous = buffer.parser.screen().scrollback();
    buffer.parser.screen_mut().set_scrollback(usize::MAX);
    let max_offset = buffer.parser.screen().scrollback();
    buffer.parser.screen_mut().set_scrollback(previous);
    max_offset
}

pub fn clamp_attach_scrollback_cursor(view_state: &mut AttachViewState) {
    let Some((inner_w, inner_h)) = focused_attach_pane_inner_size(view_state) else {
        view_state.scrollback_cursor = None;
        return;
    };
    let Some(cursor) = view_state.scrollback_cursor.as_mut() else {
        return;
    };
    cursor.row = cursor.row.min(inner_h.saturating_sub(1));
    cursor.col = cursor.col.min(inner_w.saturating_sub(1));
}

pub fn attach_scrollback_page_size(view_state: &AttachViewState) -> usize {
    focused_attach_pane_inner_size(view_state).map_or(10, |(_, inner_h)| inner_h)
}

pub fn focused_attach_pane_buffer(
    view_state: &mut AttachViewState,
) -> Option<&mut attach::state::PaneRenderBuffer> {
    let focused_pane_id = focused_attach_pane_id(view_state)?;
    view_state.pane_buffers.get_mut(&focused_pane_id)
}

pub fn focused_attach_pane_id(view_state: &AttachViewState) -> Option<Uuid> {
    Some(view_state.cached_layout_state.as_ref()?.focused_pane_id)
}

pub fn focused_attach_pane_inner_size(view_state: &AttachViewState) -> Option<(usize, usize)> {
    let layout_state = view_state.cached_layout_state.as_ref()?;
    layout_state
        .scene
        .surfaces
        .iter()
        .find(|surface| surface.visible && surface.pane_id == Some(layout_state.focused_pane_id))
        .map(|surface| {
            // Read the authoritative content_rect from the scene rather than recomputing
            // a border inset from `surface.rect`. See AGENTS.md "core architecture boundary"
            // and the content_rect contract on `bmux_ipc::AttachSurface`.
            (
                usize::from(surface.content_rect.w.max(1)),
                usize::from(surface.content_rect.h.max(1)),
            )
        })
}

#[allow(
    clippy::too_many_arguments,
    clippy::fn_params_excessive_bools,
    clippy::needless_pass_by_ref_mut
)]
pub fn build_attach_status_line_for_draw(
    _client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    status_config: &bmux_config::StatusBarConfig,
    global_theme: &bmux_config::ThemeConfig,
    context_id: Option<Uuid>,
    session_id: Uuid,
    can_write: bool,
    ui_mode: AttachUiMode,
    scrollback_active: bool,
    follow_target_id: Option<Uuid>,
    follow_global: bool,
    prompt_active: bool,
    prompt_hint: Option<&str>,
    help_overlay_open: bool,
    transient_status: Option<&str>,
    keymap: &Keymap,
) -> AttachStatusLine {
    let (cols, _) = terminal::size().unwrap_or((0, 0));
    if cols == 0 {
        return AttachStatusLine {
            rendered: String::new(),
            tab_hitboxes: Vec::new(),
        };
    }

    let cached_contexts = view_state.cached_contexts.clone();
    let cached_sessions = view_state.cached_sessions.clone();

    let tabs = build_attach_tabs_from_catalog(
        &cached_contexts,
        view_state,
        status_config,
        context_id,
        session_id,
    );
    let (session_label, session_count) =
        resolve_attach_session_label_and_count_from_catalog(&cached_sessions, session_id);
    let current_context_label =
        resolve_attach_context_label_from_catalog(&cached_contexts, context_id, session_id);
    let tab_position_label = tabs
        .iter()
        .position(|tab| tab.active)
        .map(|active_index| format!("tab:{}/{}", active_index + 1, tabs.len()));
    let zoomed = view_state
        .cached_layout_state
        .as_ref()
        .is_some_and(|s| s.zoomed);
    let mode_label = if help_overlay_open {
        "HELP"
    } else if prompt_active {
        "PROMPT"
    } else if scrollback_active {
        "SCROLL"
    } else if zoomed {
        "ZOOM"
    } else {
        view_state.active_mode_label.as_str()
    };
    let role_label = if can_write { "write" } else { "read-only" };
    let follow_label = follow_target_id.map(|id| {
        if follow_global {
            format!("following {} (global)", short_uuid(id))
        } else {
            format!("following {}", short_uuid(id))
        }
    });
    let hint = if prompt_active {
        prompt_hint.unwrap_or("Prompt active").to_string()
    } else if help_overlay_open {
        "Help overlay open | ? toggles | Esc/Enter close".to_string()
    } else if let Some(status) = transient_status {
        status.to_string()
    } else if scrollback_active {
        attach_scrollback_hint(keymap)
    } else {
        attach_mode_hint(&view_state.active_mode_id, ui_mode, keymap)
    };

    build_attach_status_line(
        cols,
        status_config,
        global_theme,
        &session_label,
        session_count,
        &current_context_label,
        &tabs,
        tab_position_label.as_deref(),
        mode_label,
        role_label,
        follow_label.as_deref(),
        &hint,
    )
}

pub fn attach_mode_hint(mode_id: &str, _ui_mode: AttachUiMode, keymap: &Keymap) -> String {
    let detach = key_hint_or_unbound(keymap, mode_id, &RuntimeAction::Detach);
    let quit = key_hint_or_unbound(keymap, mode_id, &RuntimeAction::Quit);
    let help = key_hint_or_unbound(keymap, mode_id, &RuntimeAction::ShowHelp);
    let restart = key_hint_or_unbound(keymap, mode_id, &RuntimeAction::RestartFocusedPane);
    let prev = key_hint_or_unbound(
        keymap,
        mode_id,
        &RuntimeAction::PluginCommand {
            plugin_id: "bmux.windows".to_string(),
            command_name: "prev-window".to_string(),
            args: Vec::new(),
        },
    );
    let next = key_hint_or_unbound(
        keymap,
        mode_id,
        &RuntimeAction::PluginCommand {
            plugin_id: "bmux.windows".to_string(),
            command_name: "next-window".to_string(),
            args: Vec::new(),
        },
    );
    format!(
        "{prev}/{next} tabs | {detach} detach | {restart} restart pane | {quit} quit | {help} help"
    )
}

pub fn initial_attach_status(keymap: &Keymap, mode_id: &str, can_write: bool) -> String {
    let help = key_hint_or_unbound(keymap, mode_id, &RuntimeAction::ShowHelp);
    if can_write {
        format!("{help} help | modal input enabled")
    } else {
        format!("read-only attach | {help} help")
    }
}

pub const fn attach_exit_message(reason: AttachExitReason) -> Option<&'static str> {
    match reason {
        AttachExitReason::Detached | AttachExitReason::Quit => None,
        AttachExitReason::StreamClosed => Some("attach ended unexpectedly: server stream closed"),
    }
}

pub const fn attach_exit_reason_label(reason: AttachExitReason) -> &'static str {
    match reason {
        AttachExitReason::Detached => "detached",
        AttachExitReason::StreamClosed => "stream_closed",
        AttachExitReason::Quit => "quit",
    }
}

pub fn attach_scrollback_hint(keymap: &Keymap) -> String {
    let exit = scroll_key_hint_or_unbound(keymap, &RuntimeAction::ExitScrollMode);
    let confirm = scroll_key_hint_or_unbound(keymap, &RuntimeAction::ConfirmScrollback);
    let left = scroll_key_hint_or_unbound(keymap, &RuntimeAction::MoveCursorLeft);
    let right = scroll_key_hint_or_unbound(keymap, &RuntimeAction::MoveCursorRight);
    let up = scroll_key_hint_or_unbound(keymap, &RuntimeAction::MoveCursorUp);
    let down = scroll_key_hint_or_unbound(keymap, &RuntimeAction::MoveCursorDown);
    let page_up = scroll_key_hint_or_unbound(keymap, &RuntimeAction::ScrollUpPage);
    let page_down = scroll_key_hint_or_unbound(keymap, &RuntimeAction::ScrollDownPage);
    let top = scroll_key_hint_or_unbound(keymap, &RuntimeAction::ScrollTop);
    let bottom = scroll_key_hint_or_unbound(keymap, &RuntimeAction::ScrollBottom);
    let select = scroll_key_hint_or_unbound(keymap, &RuntimeAction::BeginSelection);
    let copy = scroll_key_hint_or_unbound(keymap, &RuntimeAction::CopyScrollback);
    format!(
        "{up}/{down} line | {left}/{right} col | {page_up}/{page_down} page | {top}/{bottom} top/bottom | {select} select | {copy} copy | {confirm} copy+exit | {exit} cancel/exit scroll"
    )
}

pub fn scroll_key_hint_or_unbound(keymap: &Keymap, action: &RuntimeAction) -> String {
    keymap
        .primary_scroll_binding_for_action(action)
        .unwrap_or_else(|| "unbound".to_string())
}

pub fn key_hint_or_unbound(keymap: &Keymap, mode_id: &str, action: &RuntimeAction) -> String {
    keymap
        .primary_binding_for_action_in_mode(mode_id, action)
        .unwrap_or_else(|| "unbound".to_string())
}

pub fn sync_attach_active_mode_from_processor(
    view_state: &mut AttachViewState,
    keymap: &Keymap,
    processor_mode_id: Option<&str>,
) {
    let mode_id = processor_mode_id
        .or_else(|| keymap.initial_mode_id())
        .unwrap_or("normal")
        .to_string();
    let mode_label = keymap
        .mode_label(&mode_id)
        .map_or_else(|| mode_id.to_ascii_uppercase(), ToString::to_string);
    view_state.active_mode_id = mode_id;
    view_state.active_mode_label = mode_label;
}

pub fn apply_attach_profile_switch(
    profile_id: &str,
    attach_input_processor: &mut InputProcessor,
    view_state: &mut AttachViewState,
) -> Result<()> {
    let config_path = ConfigPaths::default().config_file();
    apply_attach_profile_switch_with_path(
        profile_id,
        attach_input_processor,
        view_state,
        &config_path,
    )
}

fn apply_attach_profile_switch_with_path(
    profile_id: &str,
    attach_input_processor: &mut InputProcessor,
    view_state: &mut AttachViewState,
    config_path: &std::path::Path,
) -> Result<()> {
    let previous_config_source = if config_path.exists() {
        Some(
            std::fs::read_to_string(config_path)
                .with_context(|| format!("failed reading {}", config_path.display()))?,
        )
    } else {
        None
    };

    let previous_keymap = attach_input_processor.keymap().clone();
    let previous_mouse_config = view_state.mouse.config.clone();
    let previous_status_position = view_state.status_position;

    if let Err(error) =
        super::super::run_config_profiles_set_active_at_path(profile_id, config_path)
    {
        return Err(error.context("failed updating composition.active_profile"));
    }

    let result = (|| -> Result<()> {
        let (resolved_config, resolution) =
            bmux_config::BmuxConfig::load_from_path_with_resolution(config_path, Some(profile_id))
                .map_err(|error| anyhow::anyhow!("{error}"))?;
        let keymap = attach_keymap_from_config(&resolved_config);
        attach_input_processor.replace_keymap(keymap);
        attach_input_processor.set_scroll_mode(view_state.scrollback_active);
        view_state.status_position = if resolved_config.status_bar.enabled {
            resolved_config.appearance.status_position
        } else {
            StatusPosition::Off
        };
        view_state.mouse.config = resolved_config.attach_mouse_config();
        sync_attach_active_mode_from_processor(
            view_state,
            attach_input_processor.keymap(),
            attach_input_processor.active_mode_id(),
        );
        view_state.set_transient_status(
            format!(
                "active profile: {}",
                resolution
                    .selected_profile
                    .unwrap_or_else(|| profile_id.to_ascii_lowercase())
            ),
            Instant::now(),
            ATTACH_TRANSIENT_STATUS_TTL,
        );
        Ok(())
    })();

    if let Err(error) = result {
        match previous_config_source {
            Some(source) => {
                let _ = std::fs::write(config_path, source);
            }
            None => {
                let _ = std::fs::remove_file(config_path);
            }
        }
        attach_input_processor.replace_keymap(previous_keymap);
        attach_input_processor.set_scroll_mode(view_state.scrollback_active);
        view_state.mouse.config = previous_mouse_config;
        view_state.status_position = previous_status_position;
        sync_attach_active_mode_from_processor(
            view_state,
            attach_input_processor.keymap(),
            attach_input_processor.active_mode_id(),
        );
        return Err(error.context("rolled back profile switch"));
    }

    Ok(())
}

pub const fn status_insets_for_position(status_position: StatusPosition) -> (u16, u16) {
    match status_position {
        StatusPosition::Top => (1, 0),
        StatusPosition::Bottom => (0, 1),
        StatusPosition::Off => (0, 0),
    }
}

pub const fn status_row_for_position(status_position: StatusPosition, rows: u16) -> Option<u16> {
    if rows == 0 {
        return None;
    }
    match status_position {
        StatusPosition::Top => Some(0),
        StatusPosition::Bottom => Some(rows.saturating_sub(1)),
        StatusPosition::Off => None,
    }
}

pub fn queue_attach_status_line(
    stdout: &mut impl Write,
    status_line: &AttachStatusLine,
    status_position: StatusPosition,
) -> Result<()> {
    let (cols, rows) = terminal::size().unwrap_or((0, 0));
    if cols == 0 || rows == 0 {
        return Ok(());
    }
    let Some(status_row) = status_row_for_position(status_position, rows) else {
        return Ok(());
    };
    queue!(stdout, MoveTo(0, status_row), Print(&status_line.rendered))
        .context("failed queuing attach status line")
}

pub fn help_overlay_visible_rows(lines: &[String]) -> usize {
    let (_cols, rows) = terminal::size().unwrap_or((0, 0));
    let max_content_rows = (rows as usize).saturating_sub(6);
    let content_rows = lines.len().min(max_content_rows);
    let height = (content_rows + 4).min((rows as usize).saturating_sub(2));
    height.saturating_sub(4).max(1)
}

pub fn adjust_help_overlay_scroll(
    current: usize,
    delta: isize,
    total_lines: usize,
    visible_rows: usize,
) -> usize {
    if total_lines == 0 {
        return 0;
    }
    let max_scroll = total_lines.saturating_sub(visible_rows.max(1));
    let next = if delta.is_negative() {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        current.saturating_add(delta.cast_unsigned())
    };
    next.min(max_scroll)
}

pub const fn help_overlay_accepts_key_kind(kind: KeyEventKind) -> bool {
    matches!(kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

pub fn handle_help_overlay_key_event(
    key: &KeyEvent,
    help_lines: &[String],
    view_state: &mut AttachViewState,
) -> bool {
    if !help_overlay_accepts_key_kind(key.kind) {
        return false;
    }

    match key.code {
        KeyCode::Esc | KeyCode::Enter => {
            view_state.help_overlay_open = false;
            view_state.help_overlay_scroll = 0;
            view_state.dirty.status_needs_redraw = true;
            view_state.dirty.full_pane_redraw = true;
            true
        }
        KeyCode::Up | KeyCode::Char('k') => {
            view_state.help_overlay_scroll = adjust_help_overlay_scroll(
                view_state.help_overlay_scroll,
                -1,
                help_lines.len(),
                help_overlay_visible_rows(help_lines),
            );
            view_state.dirty.overlay_needs_redraw = true;
            true
        }
        KeyCode::Down | KeyCode::Char('j') => {
            view_state.help_overlay_scroll = adjust_help_overlay_scroll(
                view_state.help_overlay_scroll,
                1,
                help_lines.len(),
                help_overlay_visible_rows(help_lines),
            );
            view_state.dirty.overlay_needs_redraw = true;
            true
        }
        KeyCode::PageUp => {
            let page = help_overlay_visible_rows(help_lines).cast_signed();
            view_state.help_overlay_scroll = adjust_help_overlay_scroll(
                view_state.help_overlay_scroll,
                -page,
                help_lines.len(),
                help_overlay_visible_rows(help_lines),
            );
            view_state.dirty.overlay_needs_redraw = true;
            true
        }
        KeyCode::PageDown => {
            let page = help_overlay_visible_rows(help_lines).cast_signed();
            view_state.help_overlay_scroll = adjust_help_overlay_scroll(
                view_state.help_overlay_scroll,
                page,
                help_lines.len(),
                help_overlay_visible_rows(help_lines),
            );
            view_state.dirty.overlay_needs_redraw = true;
            true
        }
        KeyCode::Home => {
            view_state.help_overlay_scroll = 0;
            view_state.dirty.overlay_needs_redraw = true;
            true
        }
        KeyCode::End => {
            let visible = help_overlay_visible_rows(help_lines);
            view_state.help_overlay_scroll = help_lines.len().saturating_sub(visible);
            view_state.dirty.overlay_needs_redraw = true;
            true
        }
        _ => false,
    }
}

#[allow(clippy::cast_possible_truncation)] // Terminal dimensions bounded by u16
pub fn help_overlay_surface(lines: &[String]) -> Option<bmux_ipc::AttachSurface> {
    let (cols, rows) = terminal::size().unwrap_or((0, 0));
    if cols < 20 || rows < 6 {
        return None;
    }

    let content_width = lines
        .iter()
        .map(std::string::String::len)
        .max()
        .unwrap_or(0)
        .min(80);
    let width = (content_width + 4)
        .max(36)
        .min((cols as usize).saturating_sub(2));
    let max_content_rows = (rows as usize).saturating_sub(6);
    let content_rows = lines.len().min(max_content_rows);
    let height = (content_rows + 4).min((rows as usize).saturating_sub(2));
    let x = ((cols as usize).saturating_sub(width)) / 2;
    let y = ((rows as usize).saturating_sub(height)) / 2;

    Some(bmux_ipc::AttachSurface {
        id: HELP_OVERLAY_SURFACE_ID,
        kind: bmux_ipc::AttachSurfaceKind::Overlay,
        layer: bmux_ipc::AttachLayer::Overlay,
        z: i32::MAX,
        rect: bmux_ipc::AttachRect {
            x: x as u16,
            y: y as u16,
            w: width as u16,
            h: height as u16,
        },
        content_rect: bmux_ipc::AttachRect {
            x: x as u16,
            y: y as u16,
            w: width as u16,
            h: height as u16,
        },
        interactive_regions: Vec::new(),
        opaque: true,
        visible: true,
        accepts_input: true,
        cursor_owner: false,
        pane_id: None,
    })
}

#[allow(clippy::cast_possible_truncation)] // Terminal dimensions bounded by u16
pub fn queue_attach_help_overlay(
    stdout: &mut impl Write,
    surface_meta: &bmux_ipc::AttachSurface,
    lines: &[String],
    scroll: usize,
) -> Result<()> {
    let width = usize::from(surface_meta.rect.w);
    let height = usize::from(surface_meta.rect.h);
    let x = usize::from(surface_meta.rect.x);
    let y = usize::from(surface_meta.rect.y);
    let body_rows = height.saturating_sub(4).max(1);
    let outer = PaneRect {
        x: surface_meta.rect.x,
        y: surface_meta.rect.y,
        w: surface_meta.rect.w,
        h: surface_meta.rect.h,
    };
    // Help overlay paints its own 1-cell frame; the fill area is the interior.
    let content = PaneRect {
        x: outer.x.saturating_add(1),
        y: outer.y.saturating_add(1),
        w: outer.w.saturating_sub(2),
        h: outer.h.saturating_sub(2),
    };
    let surface = AttachLayerSurface::new(outer, content, AttachLayer::Overlay, true);
    let text_width = width.saturating_sub(4);

    let top = format!("+{}+", "-".repeat(width.saturating_sub(2)));
    queue!(stdout, MoveTo(x as u16, y as u16), Print(&top))
        .context("failed drawing help overlay top")?;

    let title = " bmux help ";
    let title_x = x + ((width.saturating_sub(title.len())) / 2);
    queue!(stdout, MoveTo(title_x as u16, y as u16), Print(title))
        .context("failed drawing help overlay title")?;

    for row in 1..height.saturating_sub(1) {
        let y_row = (y + row) as u16;
        queue!(
            stdout,
            MoveTo(x as u16, y_row),
            Print("|"),
            MoveTo((x + width - 1) as u16, y_row),
            Print("|")
        )
        .context("failed drawing help overlay border")?;
    }

    queue_layer_fill(stdout, surface).context("failed filling help overlay body")?;

    queue!(
        stdout,
        MoveTo(x as u16, (y + height - 1) as u16),
        Print(&top)
    )
    .context("failed drawing help overlay bottom")?;

    let header = "scope    chord                action";
    let header_rendered = opaque_row_text(header, text_width);
    queue!(
        stdout,
        MoveTo((x + 2) as u16, (y + 1) as u16),
        Print(header_rendered)
    )
    .context("failed drawing help overlay header")?;

    let start = scroll.min(lines.len().saturating_sub(body_rows));
    let end = (start + body_rows).min(lines.len());
    for (idx, line) in lines.iter().skip(start).take(body_rows).enumerate() {
        let rendered = opaque_row_text(line, text_width);
        let row = y + 2 + idx;
        if row >= y + height - 1 {
            break;
        }
        queue!(stdout, MoveTo((x + 2) as u16, row as u16), Print(rendered))
            .context("failed drawing help overlay entry")?;
    }

    let footer = format!(
        "j/k or ↑/↓ scroll | PgUp/PgDn | Esc close | {}-{} / {}",
        if lines.is_empty() { 0 } else { start + 1 },
        end,
        lines.len()
    );
    let footer_rendered = opaque_row_text(&footer, text_width);
    queue!(
        stdout,
        MoveTo((x + 2) as u16, (y + height - 2) as u16),
        Print(footer_rendered)
    )
    .context("failed drawing help overlay footer")?;

    Ok(())
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub fn render_attach_frame(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    layout_state: &AttachLayoutState,
    status_config: &bmux_config::StatusBarConfig,
    global_theme: &bmux_config::ThemeConfig,
    follow_target_id: Option<Uuid>,
    follow_global: bool,
    keymap: &crate::input::Keymap,
    help_lines: &[String],
    help_scroll: usize,
    display_capture: &mut DisplayCaptureFanout,
) -> Result<()> {
    if view_state.dirty.status_needs_redraw {
        let now = Instant::now();
        let transient_status = view_state.transient_status_text(now).map(str::to_owned);
        view_state.cached_status_line = Some(build_attach_status_line_for_draw(
            client,
            view_state,
            status_config,
            global_theme,
            view_state.attached_context_id,
            view_state.attached_id,
            view_state.can_write,
            view_state.ui_mode,
            view_state.scrollback_active,
            follow_target_id,
            follow_global,
            view_state.prompt.is_active(),
            view_state.prompt.active_hint(),
            view_state.help_overlay_open,
            transient_status.as_deref(),
            keymap,
        ));
        view_state.dirty.status_needs_redraw = false;
    }

    let mut frame_bytes = Vec::new();
    // Wrap the entire frame in a synchronized update so the terminal
    // buffers all output and displays it atomically (Mode 2026).
    // Terminals that don't support this silently ignore the sequences.
    queue!(frame_bytes, BeginSynchronizedUpdate)
        .context("failed queuing begin synchronized update")?;
    queue!(frame_bytes, SavePosition).context("failed queuing cursor save for attach frame")?;
    // Hide the cursor during the frame render to prevent it from visibly
    // jumping to every MoveTo position as pane content is drawn.
    queue!(frame_bytes, Hide).context("failed queuing cursor hide for attach frame")?;
    // Reflect the forced-hide in tracked state so apply_attach_cursor_state
    // will re-emit Show if the cursor should be visible after the frame.
    if let Some(ref mut cs) = view_state.last_cursor_state {
        cs.visible = false;
    }
    if let Some(status_line) = view_state.cached_status_line.as_ref() {
        queue_attach_status_line(&mut frame_bytes, status_line, view_state.status_position)?;
    }
    let (status_top_inset, status_bottom_inset) =
        status_insets_for_position(view_state.status_position);
    let render_scene =
        view_state.dirty.full_pane_redraw || !view_state.dirty.pane_dirty_ids.is_empty();
    let cursor_state = if render_scene {
        // Snapshot the current set of registered render extensions
        // once per frame. The snapshot is cheap (Arc-clones) and
        // keeps the per-surface loop inside `render_attach_scene`
        // free of registry-lock churn.
        let extensions = bmux_plugin::registered_render_extensions();
        render_attach_scene(
            &mut frame_bytes,
            &layout_state.scene,
            &layout_state.panes,
            &mut view_state.pane_buffers,
            &view_state.dirty.pane_dirty_ids,
            view_state.dirty.full_pane_redraw,
            status_top_inset,
            status_bottom_inset,
            view_state.scrollback_active,
            view_state.scrollback_offset,
            view_state.scrollback_cursor,
            view_state.selection_anchor,
            layout_state.zoomed,
            terminal::size().unwrap_or((0, 0)),
            &extensions,
        )?
    } else {
        view_state.last_cursor_state
    };

    // Image overlay: render terminal images (Sixel, Kitty, iTerm2) on top
    // of the cell content, translated to host terminal coordinates.
    #[cfg(any(
        feature = "image-sixel",
        feature = "image-kitty",
        feature = "image-iterm2"
    ))]
    if view_state.host_image_caps.any_supported() {
        for surface in &layout_state.scene.surfaces {
            let Some(pane_id) = surface.pane_id else {
                continue;
            };
            if let Some(images) = view_state.pane_images.get(&pane_id)
                && !images.is_empty()
            {
                let pane_images: Vec<bmux_image::PaneImage> =
                    images.iter().map(bmux_image::PaneImage::from).collect();
                let pane_rect = bmux_image::compositor::PaneRect {
                    x: surface.content_rect.x,
                    y: surface.content_rect.y,
                    w: surface.content_rect.w,
                    h: surface.content_rect.h,
                };
                let decode_mode = view_state.image_decode_mode;
                let _ = bmux_image::compositor::render_pane_images(
                    &mut frame_bytes,
                    &pane_images,
                    pane_rect,
                    &view_state.host_image_caps,
                    decode_mode,
                    &mut view_state.kitty_host_state,
                );
            }
        }
    }

    let previous_cursor_state = view_state.last_cursor_state;
    let mut overlay_cursor_state = None;
    if view_state.help_overlay_open
        && let Some(help_surface) = help_overlay_surface(help_lines)
    {
        queue_attach_help_overlay(&mut frame_bytes, &help_surface, help_lines, help_scroll)?;
    }
    if view_state.prompt.is_active() {
        overlay_cursor_state = view_state
            .prompt
            .queue_attach_prompt_overlay(&mut frame_bytes)?;
    }

    if view_state.help_overlay_open || view_state.prompt.is_active() {
        apply_attach_cursor_state(
            &mut frame_bytes,
            overlay_cursor_state,
            &mut view_state.last_cursor_state,
            false,
        )?;
    } else {
        let force_cursor_move = std::mem::take(&mut view_state.force_cursor_move_next_frame);
        apply_attach_cursor_state(
            &mut frame_bytes,
            cursor_state,
            &mut view_state.last_cursor_state,
            force_cursor_move,
        )?;
    }

    display_capture.record_frame_bytes(&frame_bytes);
    display_capture.record_activity(bmux_ipc::DisplayActivityKind::Output);
    display_capture.record_cursor_snapshot(view_state.last_cursor_state);
    if previous_cursor_state != view_state.last_cursor_state {
        display_capture.record_activity(bmux_ipc::DisplayActivityKind::Cursor);
    }
    // Record structured image data for GIF export.
    #[cfg(any(
        feature = "image-sixel",
        feature = "image-kitty",
        feature = "image-iterm2"
    ))]
    {
        let mut all_images: Vec<bmux_ipc::AttachPaneImage> = Vec::new();
        for surface in &layout_state.scene.surfaces {
            let Some(pane_id) = surface.pane_id else {
                continue;
            };
            if let Some(images) = view_state.pane_images.get(&pane_id) {
                for img in images {
                    let mut adjusted = img.clone();
                    // Offset pane-local coords by surface position + 1
                    // for the pane border, matching the live compositor's
                    // PaneRect translation in render_pane_images.
                    adjusted.position_col = adjusted
                        .position_col
                        .saturating_add(surface.rect.x.saturating_add(1));
                    adjusted.position_row = adjusted
                        .position_row
                        .saturating_add(surface.rect.y.saturating_add(1));
                    all_images.push(adjusted);
                }
            }
        }
        display_capture.record_images(&all_images);
    }

    queue!(frame_bytes, EndSynchronizedUpdate).context("failed queuing end synchronized update")?;

    let mut stdout = io::stdout();
    stdout
        .write_all(&frame_bytes)
        .context("failed writing attach frame")?;
    stdout.flush().context("failed flushing attach frame")?;
    view_state.dirty.full_pane_redraw = false;
    view_state.dirty.overlay_needs_redraw = false;
    view_state.dirty.pane_dirty_ids.clear();
    Ok(())
}

pub fn build_attach_tabs_from_catalog(
    contexts: &[ContextSummary],
    view_state: &AttachViewState,
    status_config: &bmux_config::StatusBarConfig,
    context_id: Option<Uuid>,
    session_id: Uuid,
) -> Vec<AttachTab> {
    // Prefer the windows-plugin's authoritative ordered window list
    // whenever it has been published. The plugin owns tab ordering
    // (`windows.order` persisted storage) and publishes a fresh
    // snapshot on every mutation via the `windows-list` state channel.
    // Subscribing to that channel (see attach main loop) keeps
    // `cached_window_list` live without polling.
    //
    // Two opt-out paths remain for MRU-flavored status-bar configs so
    // existing semantics survive the migration:
    //   - `tab_scope = Mru` and `tab_order = Mru` — use the raw MRU
    //     order from `cached_contexts`.
    //   - `tab_scope = SessionContexts` — filter the plugin list
    //     (if the plugin filtered view is empty, fall back to the full
    //     plugin list).
    let use_mru = matches!(status_config.tab_scope, bmux_config::StatusTabScope::Mru)
        || matches!(status_config.tab_order, bmux_config::StatusTabOrder::Mru);

    if !use_mru
        && let Some(snapshot) = view_state.cached_window_list.as_ref()
        && !snapshot.windows.is_empty()
    {
        return build_attach_tabs_from_plugin_snapshot(
            snapshot,
            contexts,
            status_config,
            context_id,
            session_id,
        );
    }

    build_attach_tabs_from_raw_contexts(contexts, status_config, context_id, session_id)
}

/// Project the windows-plugin's authoritative ordered window list onto
/// the attach tab-bar view. Called whenever `cached_window_list` has
/// been populated from the `windows-list` state channel.
fn build_attach_tabs_from_plugin_snapshot(
    snapshot: &bmux_windows_plugin_api::windows_list::WindowListSnapshot,
    contexts: &[ContextSummary],
    status_config: &bmux_config::StatusBarConfig,
    context_id: Option<Uuid>,
    session_id: Uuid,
) -> Vec<AttachTab> {
    // Filter to this session's contexts if the user asked for
    // `SessionContexts` scope. Cross-reference `cached_contexts` to
    // locate the `bmux.session_id` attribute for each window.
    let filtered: Vec<&bmux_windows_plugin_api::windows_list::WindowListEntry> = if matches!(
        status_config.tab_scope,
        bmux_config::StatusTabScope::SessionContexts
    ) {
        let session_match: Vec<_> = snapshot
            .windows
            .iter()
            .filter(|entry| {
                contexts
                    .iter()
                    .find(|context| context.id == entry.id)
                    .is_some_and(|context| {
                        context
                            .attributes
                            .get("bmux.session_id")
                            .is_some_and(|value| value == &session_id.to_string())
                    })
            })
            .collect();
        if session_match.is_empty() {
            snapshot.windows.iter().collect()
        } else {
            session_match
        }
    } else {
        snapshot.windows.iter().collect()
    };

    let current_id = context_id.or_else(|| {
        contexts
            .iter()
            .find(|context| {
                context
                    .attributes
                    .get("bmux.session_id")
                    .is_some_and(|value| value == &session_id.to_string())
            })
            .map(|context| context.id)
    });

    filtered
        .into_iter()
        .map(|entry| AttachTab {
            label: entry.name.clone(),
            active: entry.active || current_id == Some(entry.id),
            context_id: Some(entry.id),
        })
        .collect()
}

/// Baseline tab-bar renderer when no plugin snapshot is available
/// (plugin absent, not yet activated, or user opted into MRU mode).
/// Renders `cached_contexts` in the order the server delivered them.
/// Core does not stabilize or reorder — that's the plugin's job.
fn build_attach_tabs_from_raw_contexts(
    contexts: &[ContextSummary],
    status_config: &bmux_config::StatusBarConfig,
    context_id: Option<Uuid>,
    session_id: Uuid,
) -> Vec<AttachTab> {
    if contexts.is_empty() {
        return vec![AttachTab {
            label: "terminal".to_string(),
            active: true,
            context_id: None,
        }];
    }

    let tab_contexts = match status_config.tab_scope {
        bmux_config::StatusTabScope::AllContexts | bmux_config::StatusTabScope::Mru => {
            contexts.to_vec()
        }
        bmux_config::StatusTabScope::SessionContexts => {
            let filtered = contexts
                .iter()
                .filter(|context| {
                    context
                        .attributes
                        .get("bmux.session_id")
                        .is_some_and(|value| value == &session_id.to_string())
                })
                .cloned()
                .collect::<Vec<_>>();
            if filtered.is_empty() {
                contexts.to_vec()
            } else {
                filtered
            }
        }
    };

    let current_context_id = context_id.or_else(|| {
        tab_contexts
            .iter()
            .find(|context| {
                context
                    .attributes
                    .get("bmux.session_id")
                    .is_some_and(|value| value == &session_id.to_string())
            })
            .map(|context| context.id)
    });

    tab_contexts
        .into_iter()
        .enumerate()
        .map(|(index, context)| AttachTab {
            label: context_summary_label(&context, Some(index.saturating_add(1))),
            active: current_context_id == Some(context.id),
            context_id: Some(context.id),
        })
        .collect()
}

pub fn resolve_attach_context_label_from_catalog(
    contexts: &[ContextSummary],
    context_id: Option<Uuid>,
    session_id: Uuid,
) -> String {
    if let Some(context_id) = context_id
        && let Some((index, context)) = contexts
            .iter()
            .enumerate()
            .find(|(_, context)| context.id == context_id)
    {
        return context_summary_label(context, Some(index.saturating_add(1)));
    }

    if let Some((index, context)) = contexts.iter().enumerate().find(|(_, context)| {
        context
            .attributes
            .get("bmux.session_id")
            .is_some_and(|value| value == &session_id.to_string())
    }) {
        return context_summary_label(context, Some(index.saturating_add(1)));
    }

    "terminal".to_string()
}

pub fn context_summary_label(context: &ContextSummary, fallback_index: Option<usize>) -> String {
    context
        .name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .map_or_else(
            || fallback_index.map_or_else(|| "tab".to_string(), |index| format!("tab-{index}")),
            ToString::to_string,
        )
}

pub fn resolve_attach_session_label_and_count_from_catalog(
    sessions: &[SessionSummary],
    session_id: Uuid,
) -> (String, usize) {
    let count = sessions.len();
    let label = sessions
        .iter()
        .find(|session| session.id == session_id)
        .map_or_else(
            || format!("session-{}", short_uuid(session_id)),
            session_summary_label,
        );
    (label, count)
}

pub fn session_summary_label(session: &bmux_ipc::SessionSummary) -> String {
    session
        .name
        .clone()
        .unwrap_or_else(|| format!("session-{}", short_uuid(session.id)))
}

pub fn attach_context_status_from_catalog(view_state: &AttachViewState) -> String {
    let (session_label, _count) = resolve_attach_session_label_and_count_from_catalog(
        &view_state.cached_sessions,
        view_state.attached_id,
    );
    let context_label = resolve_attach_context_label_from_catalog(
        &view_state.cached_contexts,
        view_state.attached_context_id,
        view_state.attached_id,
    );
    format!("session: {session_label} | context: {context_label}")
}

pub fn set_attach_context_status(
    view_state: &mut AttachViewState,
    status: String,
    now: Instant,
    ttl: Duration,
) {
    view_state.set_transient_status(status, now, ttl);
}

pub fn short_uuid(id: Uuid) -> String {
    id.to_string().chars().take(8).collect()
}

pub async fn open_attach_for_session(
    client: &mut StreamingBmuxClient,
    session_id: Uuid,
) -> std::result::Result<bmux_client::AttachOpenInfo, ClientError> {
    let grant = client
        .attach_grant(SessionSelector::ById(session_id))
        .await?;
    client.open_attach_stream_info(&grant).await
}

pub async fn open_attach_for_context(
    client: &mut StreamingBmuxClient,
    context_id: Uuid,
) -> std::result::Result<bmux_client::AttachOpenInfo, ClientError> {
    let grant = client
        .attach_context_grant(ContextSelector::ById(context_id))
        .await?;
    client.open_attach_stream_info(&grant).await
}

pub const fn attached_session_selector(view_state: &AttachViewState) -> SessionSelector {
    SessionSelector::ById(view_state.attached_id)
}

fn parse_context_session_id(context: &ContextSummary) -> Option<Uuid> {
    context
        .attributes
        .get("bmux.session_id")
        .and_then(|value| Uuid::parse_str(value).ok())
}

fn apply_context_session_bindings_to_contexts(
    contexts: &mut [ContextSummary],
    bindings: &[ContextSessionBindingSummary],
) {
    let binding_by_context = bindings
        .iter()
        .map(|binding| (binding.context_id, binding.session_id))
        .collect::<BTreeMap<_, _>>();
    for context in contexts {
        if let Some(session_id) = binding_by_context.get(&context.id) {
            context
                .attributes
                .insert("bmux.session_id".to_string(), session_id.to_string());
        }
    }
}

fn apply_control_catalog_snapshot(
    view_state: &mut AttachViewState,
    mut snapshot: ControlCatalogSnapshot,
) {
    apply_context_session_bindings_to_contexts(
        &mut snapshot.contexts,
        &snapshot.context_session_bindings,
    );
    view_state.cached_contexts = snapshot.contexts;
    view_state.cached_sessions = snapshot.sessions;
    view_state.control_catalog_revision = snapshot.revision;
}

pub async fn reconcile_attached_session_from_catalog(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
) -> std::result::Result<bool, ClientError> {
    let Some(context_id) = view_state.attached_context_id else {
        return Ok(false);
    };

    let Some(mapped_session_id) = view_state
        .cached_contexts
        .iter()
        .find(|context| context.id == context_id)
        .and_then(parse_context_session_id)
    else {
        return Ok(false);
    };

    if mapped_session_id == view_state.attached_id {
        return Ok(false);
    }

    let started_at = Instant::now();
    trace!(
        context_id = %context_id,
        previous_session_id = %view_state.attached_id,
        mapped_session_id = %mapped_session_id,
        "attach.catalog_reconcile.start"
    );

    let attach_info = open_attach_for_context(client, context_id).await?;
    view_state.attached_id = attach_info.session_id;
    view_state.attached_context_id = attach_info.context_id.or(Some(context_id));
    view_state.can_write = attach_info.can_write;
    update_attach_viewport(client, view_state.attached_id, view_state.status_position).await?;
    hydrate_attach_state_from_snapshot(client, view_state).await?;
    view_state.ui_mode = AttachUiMode::Normal;
    let status = attach_context_status_from_catalog(view_state);
    set_attach_context_status(
        view_state,
        status,
        Instant::now(),
        ATTACH_TRANSIENT_STATUS_TTL,
    );

    trace!(
        context_id = ?view_state.attached_context_id,
        refreshed_session_id = %view_state.attached_id,
        elapsed_ms = started_at.elapsed().as_millis(),
        "attach.catalog_reconcile.done"
    );

    Ok(true)
}

pub async fn refresh_attach_status_catalog(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
) -> anyhow::Result<()> {
    let snapshot = bmux_control_catalog_plugin_api::typed_client::control_catalog_snapshot(
        client,
        Some(view_state.control_catalog_revision),
    )
    .await?;
    apply_control_catalog_snapshot(view_state, snapshot);
    Ok(())
}

async fn refresh_attach_status_catalog_best_effort(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
) {
    if let Err(error) = refresh_attach_status_catalog(client, view_state).await {
        warn!(
            attached_context_id = ?view_state.attached_context_id,
            attached_session_id = %view_state.attached_id,
            "attach.status_catalog.refresh_failed: {error:#}"
        );
    }
}

pub fn attach_keymap_from_config(config: &BmuxConfig) -> crate::input::Keymap {
    let (_runtime_bindings, global_bindings, scroll_bindings) = filtered_attach_keybindings(config);
    let timeout_ms = config
        .keybindings
        .resolve_timeout()
        .map(|timeout| timeout.timeout_ms())
        .unwrap_or(None);
    let modal_modes = config
        .keybindings
        .modes
        .iter()
        .map(|(mode_id, mode)| {
            (
                mode_id.clone(),
                crate::input::ModalModeConfig {
                    label: mode.label.clone(),
                    passthrough: mode.passthrough,
                    bindings: mode.bindings.clone(),
                },
            )
        })
        .collect();
    match crate::input::Keymap::from_modal_parts_with_scroll(
        timeout_ms,
        &config.keybindings.initial_mode,
        &modal_modes,
        &global_bindings,
        &scroll_bindings,
    ) {
        Ok(keymap) => keymap,
        Err(error) => {
            eprintln!("bmux warning: invalid attach keymap config, using defaults ({error})");
            default_attach_keymap()
        }
    }
}

pub fn filtered_attach_keybindings(
    config: &BmuxConfig,
) -> (
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeMap<String, String>,
) {
    let (runtime, global, scroll) = merged_runtime_keybindings(config);
    let runtime = normalize_attach_keybindings(runtime, "runtime");
    let mut global = normalize_attach_keybindings(global, "global");
    let scroll = normalize_attach_keybindings(scroll, "scroll");

    inject_attach_global_defaults(&mut global);
    (runtime, global, scroll)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachKeybindingScope {
    Runtime,
    Global,
}

impl AttachKeybindingScope {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Runtime => "runtime",
            Self::Global => "global",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachKeybindingEntry {
    pub scope: AttachKeybindingScope,
    pub chord: String,
    pub action: RuntimeAction,
    pub action_name: String,
}

pub fn effective_attach_keybindings(config: &BmuxConfig) -> Vec<AttachKeybindingEntry> {
    let (runtime, global, _) = filtered_attach_keybindings(config);
    let mut entries = Vec::new();

    for (chord, action_name) in runtime {
        if let Ok(action) = crate::input::parse_runtime_action_name(&action_name) {
            entries.push(AttachKeybindingEntry {
                scope: AttachKeybindingScope::Runtime,
                chord,
                action,
                action_name,
            });
        }
    }
    for (chord, action_name) in global {
        if let Ok(action) = crate::input::parse_runtime_action_name(&action_name) {
            entries.push(AttachKeybindingEntry {
                scope: AttachKeybindingScope::Global,
                chord,
                action,
                action_name,
            });
        }
    }

    entries.sort_by(|left, right| {
        left.scope
            .as_str()
            .cmp(right.scope.as_str())
            .then_with(|| left.chord.cmp(&right.chord))
    });
    entries
}

#[allow(clippy::too_many_lines)]
pub fn build_attach_help_lines(config: &BmuxConfig) -> Vec<String> {
    let keymap = attach_keymap_from_config(config);
    let active_mode_id = keymap.initial_mode_id().unwrap_or("normal");
    let help = key_hint_or_unbound(&keymap, active_mode_id, &RuntimeAction::ShowHelp);
    let detach = key_hint_or_unbound(&keymap, active_mode_id, &RuntimeAction::Detach);
    let scroll = key_hint_or_unbound(&keymap, active_mode_id, &RuntimeAction::EnterScrollMode);
    let close = key_hint_or_unbound(&keymap, active_mode_id, &RuntimeAction::CloseFocusedPane);
    let restart = key_hint_or_unbound(&keymap, active_mode_id, &RuntimeAction::RestartFocusedPane);
    let mut groups: Vec<(&str, Vec<AttachKeybindingEntry>)> = vec![
        ("Session", Vec::new()),
        ("Pane", Vec::new()),
        ("Mode", Vec::new()),
        ("Other", Vec::new()),
    ];

    for entry in effective_attach_keybindings(config) {
        let category = match entry.action {
            RuntimeAction::Detach | RuntimeAction::Quit => "Session",
            RuntimeAction::SplitFocusedVertical
            | RuntimeAction::SplitFocusedHorizontal
            | RuntimeAction::FocusNext
            | RuntimeAction::FocusPrev
            | RuntimeAction::FocusLeft
            | RuntimeAction::FocusRight
            | RuntimeAction::FocusUp
            | RuntimeAction::FocusDown
            | RuntimeAction::IncreaseSplit
            | RuntimeAction::DecreaseSplit
            | RuntimeAction::ResizeLeft
            | RuntimeAction::ResizeRight
            | RuntimeAction::ResizeUp
            | RuntimeAction::ResizeDown
            | RuntimeAction::CloseFocusedPane
            | RuntimeAction::RestartFocusedPane => "Pane",
            RuntimeAction::EnterWindowMode
            | RuntimeAction::ExitMode
            | RuntimeAction::EnterScrollMode
            | RuntimeAction::ExitScrollMode
            | RuntimeAction::ScrollUpLine
            | RuntimeAction::ScrollDownLine
            | RuntimeAction::ScrollUpPage
            | RuntimeAction::ScrollDownPage
            | RuntimeAction::ScrollTop
            | RuntimeAction::ScrollBottom
            | RuntimeAction::BeginSelection
            | RuntimeAction::CopyScrollback
            | RuntimeAction::ConfirmScrollback
            | RuntimeAction::EnterMode(_)
            | RuntimeAction::ShowHelp => "Mode",
            _ => "Other",
        };

        if let Some((_, entries)) = groups.iter_mut().find(|(name, _)| *name == category) {
            entries.push(entry);
        }
    }

    let mut lines = Vec::new();
    lines.push("Attach Help".to_string());
    lines.push(format!(
        "Modal keybindings are active. Use {scroll} for scrollback, {detach} to detach, and {help} to toggle help."
    ));
    lines.push(format!(
        "Pane recovery: use {restart} to restart an exited pane in place; {close} opens a confirmation prompt before closing."
    ));
    lines.push(String::new());
    for (category, mut entries) in groups {
        if entries.is_empty() {
            continue;
        }
        entries.sort_by(|left, right| {
            left.scope
                .as_str()
                .cmp(right.scope.as_str())
                .then_with(|| left.chord.cmp(&right.chord))
        });
        lines.push(format!("-- {category} --"));
        for entry in entries {
            lines.push(format!(
                "[{:<7}] {:<20} {}",
                entry.scope.as_str(),
                entry.chord,
                entry.action_name
            ));
        }
        lines.push(String::new());
    }

    if lines.last().is_some_and(String::is_empty) {
        let _ = lines.pop();
    }
    lines
}

pub fn normalize_attach_keybindings(
    bindings: std::collections::BTreeMap<String, String>,
    scope: &str,
) -> std::collections::BTreeMap<String, String> {
    bindings
        .into_iter()
        .filter_map(
            |(chord, action_name)| match crate::input::parse_runtime_action_name(&action_name) {
                Ok(action) if is_attach_runtime_action(&action) => {
                    Some((chord, action_to_config_name(&action)))
                }
                Ok(_) => None,
                Err(error) => {
                    eprintln!(
                        "bmux warning: dropping invalid {scope} keybinding '{chord}' -> '{action_name}' ({error})"
                    );
                    None
                }
            },
        )
        .collect()
}

pub const fn inject_attach_global_defaults(
    _global: &mut std::collections::BTreeMap<String, String>,
) {
    // Global defaults are now provided by KeyBindingConfig::default_global_runtime_bindings().
    // This function is retained as a hook for future attach-specific global defaults.
}

pub const fn is_attach_runtime_action(action: &RuntimeAction) -> bool {
    matches!(
        action,
        RuntimeAction::Detach
            | RuntimeAction::Quit
            | RuntimeAction::EnterWindowMode
            | RuntimeAction::ExitMode
            | RuntimeAction::EnterScrollMode
            | RuntimeAction::ExitScrollMode
            | RuntimeAction::ScrollUpLine
            | RuntimeAction::ScrollDownLine
            | RuntimeAction::ScrollUpPage
            | RuntimeAction::ScrollDownPage
            | RuntimeAction::ScrollTop
            | RuntimeAction::ScrollBottom
            | RuntimeAction::BeginSelection
            | RuntimeAction::CopyScrollback
            | RuntimeAction::ConfirmScrollback
            | RuntimeAction::WindowPrev
            | RuntimeAction::WindowNext
            | RuntimeAction::WindowGoto1
            | RuntimeAction::WindowGoto2
            | RuntimeAction::WindowGoto3
            | RuntimeAction::WindowGoto4
            | RuntimeAction::WindowGoto5
            | RuntimeAction::WindowGoto6
            | RuntimeAction::WindowGoto7
            | RuntimeAction::WindowGoto8
            | RuntimeAction::WindowGoto9
            | RuntimeAction::WindowClose
            | RuntimeAction::PluginCommand { .. }
            | RuntimeAction::SplitFocusedVertical
            | RuntimeAction::SplitFocusedHorizontal
            | RuntimeAction::FocusNext
            | RuntimeAction::FocusPrev
            | RuntimeAction::FocusLeft
            | RuntimeAction::FocusRight
            | RuntimeAction::FocusUp
            | RuntimeAction::FocusDown
            | RuntimeAction::IncreaseSplit
            | RuntimeAction::DecreaseSplit
            | RuntimeAction::ResizeLeft
            | RuntimeAction::ResizeRight
            | RuntimeAction::ResizeUp
            | RuntimeAction::ResizeDown
            | RuntimeAction::CloseFocusedPane
            | RuntimeAction::ZoomPane
            | RuntimeAction::ThemePicker
            | RuntimeAction::ShowHelp
            | RuntimeAction::EnterMode(_)
    )
}

pub fn default_attach_keymap() -> crate::input::Keymap {
    let defaults = BmuxConfig::default();
    let (_runtime_bindings, global_bindings, scroll_bindings) =
        filtered_attach_keybindings(&defaults);
    let timeout_ms = defaults
        .keybindings
        .resolve_timeout()
        .expect("default timeout config must be valid")
        .timeout_ms();
    let modal_modes = defaults
        .keybindings
        .modes
        .iter()
        .map(|(mode_id, mode)| {
            (
                mode_id.clone(),
                crate::input::ModalModeConfig {
                    label: mode.label.clone(),
                    passthrough: mode.passthrough,
                    bindings: mode.bindings.clone(),
                },
            )
        })
        .collect();
    crate::input::Keymap::from_modal_parts_with_scroll(
        timeout_ms,
        &defaults.keybindings.initial_mode,
        &modal_modes,
        &global_bindings,
        &scroll_bindings,
    )
    .expect("default attach keymap must be valid")
}

pub fn describe_timeout(timeout: &ResolvedTimeout) -> String {
    match timeout {
        ResolvedTimeout::Indefinite => "indefinite".to_string(),
        ResolvedTimeout::Exact(ms) => format!("exact ({ms}ms)"),
        ResolvedTimeout::Profile { name, ms } => format!("profile:{name} ({ms}ms)"),
    }
}

pub struct RawModeGuard {
    keyboard_enhanced: bool,
    mouse_capture_enabled: bool,
}

impl RawModeGuard {
    fn enable(kitty_keyboard_enabled: bool, mouse_capture_enabled: bool) -> Result<Self> {
        enable_raw_mode().context("failed enabling raw mode")?;

        #[cfg(feature = "kitty-keyboard")]
        let keyboard_enhanced = kitty_keyboard_enabled
            && crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
        #[cfg(not(feature = "kitty-keyboard"))]
        let keyboard_enhanced = false;

        let _ = kitty_keyboard_enabled; // suppress unused warning when feature is disabled

        let mut stdout = io::stdout();
        if keyboard_enhanced {
            use crossterm::event::{KeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
            queue!(
                stdout,
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                )
            )
            .context("failed to push keyboard enhancement flags")?;
            stdout
                .flush()
                .context("failed to flush after pushing keyboard flags")?;
        }

        if mouse_capture_enabled {
            queue!(stdout, EnableMouseCapture).context("failed to enable mouse capture")?;
            stdout
                .flush()
                .context("failed to flush after enabling mouse capture")?;
        }

        Ok(Self {
            keyboard_enhanced,
            mouse_capture_enabled,
        })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.mouse_capture_enabled {
            let mut stdout = io::stdout();
            let _ = queue!(stdout, DisableMouseCapture);
            let _ = stdout.flush();
        }
        if self.keyboard_enhanced {
            use crossterm::event::PopKeyboardEnhancementFlags;
            let mut stdout = io::stdout();
            let _ = queue!(stdout, PopKeyboardEnhancementFlags);
            let _ = stdout.flush();
        }
        let _ = disable_raw_mode();
    }
}

pub async fn update_attach_viewport(
    client: &mut StreamingBmuxClient,
    session_id: Uuid,
    status_position: StatusPosition,
) -> std::result::Result<(), ClientError> {
    let (cols, rows) = terminal::size().unwrap_or((0, 0));
    if cols == 0 || rows == 0 {
        return Ok(());
    }
    let (status_top_inset, status_bottom_inset) = status_insets_for_position(status_position);
    client
        .attach_set_viewport_with_insets(
            session_id,
            cols,
            rows,
            status_top_inset,
            status_bottom_inset,
        )
        .await?;
    Ok(())
}

pub async fn hydrate_attach_state_from_snapshot(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
) -> std::result::Result<(), ClientError> {
    hydrate_attach_state_from_snapshot_mode(client, view_state, SnapshotHydrationMode::Incremental)
        .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapshotHydrationMode {
    Incremental,
    FullResync,
}

async fn hydrate_attach_state_from_snapshot_mode(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    mode: SnapshotHydrationMode,
) -> std::result::Result<(), ClientError> {
    let AttachSnapshotState {
        context_id: _,
        session_id,
        focused_pane_id,
        panes,
        layout_root,
        scene,
        chunks,
        pane_mouse_protocols,
        pane_input_modes,
        zoomed,
    } = client
        .attach_snapshot(view_state.attached_id, ATTACH_SNAPSHOT_MAX_BYTES_PER_PANE)
        .await?;

    let active_pane_ids = panes
        .iter()
        .map(|pane| pane.id)
        .collect::<std::collections::BTreeSet<_>>();
    let full_resync = mode == SnapshotHydrationMode::FullResync;
    let session_changed = view_state
        .cached_layout_state
        .as_ref()
        .is_none_or(|layout| layout.session_id != session_id);
    if session_changed || full_resync {
        view_state.pane_buffers.clear();
        view_state.pane_mouse_protocol_hints.clear();
        view_state.pane_input_mode_hints.clear();
    } else {
        view_state
            .pane_buffers
            .retain(|pane_id, _| active_pane_ids.contains(pane_id));
        view_state
            .pane_mouse_protocol_hints
            .retain(|pane_id, _| active_pane_ids.contains(pane_id));
        view_state
            .pane_input_mode_hints
            .retain(|pane_id, _| active_pane_ids.contains(pane_id));
    }
    let retained_pane_ids = view_state
        .pane_buffers
        .keys()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();

    view_state.cached_layout_state = Some(AttachLayoutState {
        context_id: None,
        session_id,
        focused_pane_id,
        panes,
        layout_root,
        scene,
        zoomed,
    });
    view_state.mouse.last_focused_pane_id = Some(focused_pane_id);

    for pane_protocol in pane_mouse_protocols {
        if active_pane_ids.contains(&pane_protocol.pane_id) {
            view_state
                .pane_mouse_protocol_hints
                .insert(pane_protocol.pane_id, pane_protocol.protocol);
        }
    }
    for pane_mode in pane_input_modes {
        if active_pane_ids.contains(&pane_mode.pane_id) {
            view_state
                .pane_input_mode_hints
                .insert(pane_mode.pane_id, pane_mode.mode);
        }
    }

    if let Some(layout_state) = view_state.cached_layout_state.as_ref() {
        resize_attach_parsers_for_scene(&mut view_state.pane_buffers, &layout_state.scene);
    }
    let mut frame_needs_render = false;
    for chunk in chunks {
        if !session_changed && !full_resync && retained_pane_ids.contains(&chunk.pane_id) {
            continue;
        }
        let _ = apply_attach_output_bytes(
            view_state,
            chunk.pane_id,
            &chunk.data,
            &mut frame_needs_render,
        );
        if let Some(buffer) = view_state.pane_buffers.get_mut(&chunk.pane_id) {
            buffer.sync_update_in_progress = chunk.sync_update_active;
            buffer.expected_stream_start = Some(chunk.stream_end);
        }
    }
    view_state.dirty.layout_needs_refresh = false;
    view_state.dirty.full_pane_redraw = true;
    view_state.dirty.status_needs_redraw = true;
    Ok(())
}

async fn hydrate_attach_revealed_panes_from_snapshot(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    layout_state: &AttachLayoutState,
    pane_ids: &[Uuid],
) -> std::result::Result<(), ClientError> {
    if pane_ids.is_empty() {
        return Ok(());
    }

    let requested_pane_ids = pane_ids.iter().copied().collect::<BTreeSet<_>>();
    let AttachPaneSnapshotState {
        chunks,
        pane_mouse_protocols,
        pane_input_modes,
    } = client
        .attach_pane_snapshot(
            view_state.attached_id,
            pane_ids.to_vec(),
            ATTACH_SNAPSHOT_MAX_BYTES_PER_PANE,
        )
        .await?;

    for pane_id in pane_ids {
        view_state
            .pane_buffers
            .insert(*pane_id, PaneRenderBuffer::default());
    }
    resize_attach_parsers_for_scene(&mut view_state.pane_buffers, &layout_state.scene);

    let mut frame_needs_render = false;
    for chunk in chunks {
        if !requested_pane_ids.contains(&chunk.pane_id) {
            continue;
        }

        let _ = apply_attach_output_bytes(
            view_state,
            chunk.pane_id,
            &chunk.data,
            &mut frame_needs_render,
        );
        if let Some(buffer) = view_state.pane_buffers.get_mut(&chunk.pane_id) {
            buffer.sync_update_in_progress = chunk.sync_update_active;
            buffer.expected_stream_start = Some(chunk.stream_end);
        }
    }

    for pane_protocol in pane_mouse_protocols {
        if requested_pane_ids.contains(&pane_protocol.pane_id) {
            view_state
                .pane_mouse_protocol_hints
                .insert(pane_protocol.pane_id, pane_protocol.protocol);
        }
    }
    for pane_mode in pane_input_modes {
        if requested_pane_ids.contains(&pane_mode.pane_id) {
            view_state
                .pane_input_mode_hints
                .insert(pane_mode.pane_id, pane_mode.mode);
        }
    }

    for pane_id in pane_ids {
        view_state.dirty.pane_dirty_ids.insert(*pane_id);
    }

    Ok(())
}

pub fn attach_scene_revealed_pane_ids(
    previous: &bmux_ipc::AttachScene,
    next: &bmux_ipc::AttachScene,
) -> BTreeSet<Uuid> {
    bmux_attach_pipeline::reconcile::attach_scene_revealed_pane_ids(previous, next)
}

pub fn attach_layout_pane_id_set(layout_state: &AttachLayoutState) -> BTreeSet<Uuid> {
    bmux_attach_pipeline::reconcile::attach_layout_pane_id_set(layout_state)
}

pub fn attach_layout_requires_snapshot_hydration(
    previous: &AttachLayoutState,
    next: &AttachLayoutState,
) -> bool {
    bmux_attach_pipeline::reconcile::attach_layout_requires_snapshot_hydration(previous, next)
}

pub fn resize_attach_parsers_for_scene(
    pane_buffers: &mut std::collections::BTreeMap<Uuid, attach::state::PaneRenderBuffer>,
    scene: &bmux_ipc::AttachScene,
) {
    let (cols, rows) = terminal::size().unwrap_or((0, 0));
    resize_attach_parsers_for_scene_with_size(pane_buffers, scene, cols, rows);
}

pub fn resize_attach_parsers_for_scene_with_size(
    pane_buffers: &mut std::collections::BTreeMap<Uuid, attach::state::PaneRenderBuffer>,
    scene: &bmux_ipc::AttachScene,
    cols: u16,
    rows: u16,
) {
    bmux_attach_pipeline::reconcile::resize_attach_parsers_for_scene_with_size(
        pane_buffers,
        scene,
        cols,
        rows,
    );
}

#[allow(clippy::too_many_arguments)]
pub async fn handle_attach_loop_event(
    event: AttachLoopEvent,
    client: &mut StreamingBmuxClient,
    attach_input_processor: &mut InputProcessor,
    follow_target_id: Option<Uuid>,
    self_client_id: Option<Uuid>,
    global: bool,
    help_lines: &[String],
    view_state: &mut AttachViewState,
    theme_state: &mut AttachThemeState,
    display_capture: &mut DisplayCaptureFanout,
    kernel_client_factory: Option<&KernelClientFactory>,
) -> Result<AttachLoopControl> {
    match event {
        AttachLoopEvent::Server(server_event) => {
            handle_attach_server_event(
                client,
                server_event,
                follow_target_id,
                self_client_id,
                global,
                view_state,
            )
            .await
        }
        AttachLoopEvent::Terminal(terminal_event) => {
            handle_attach_terminal_event(
                client,
                terminal_event,
                attach_input_processor,
                help_lines,
                view_state,
                theme_state,
                display_capture,
                kernel_client_factory,
            )
            .await
        }
        AttachLoopEvent::ActionDispatch(dispatch_request) => {
            handle_attach_action_dispatch(
                client,
                dispatch_request,
                view_state,
                theme_state,
                kernel_client_factory,
            )
            .await
        }
    }
}

#[allow(clippy::too_many_lines)]
pub async fn handle_attach_server_event(
    client: &mut StreamingBmuxClient,
    server_event: bmux_client::ServerEvent,
    follow_target_id: Option<Uuid>,
    self_client_id: Option<Uuid>,
    _global: bool,
    view_state: &mut AttachViewState,
) -> Result<AttachLoopControl> {
    if let bmux_client::ServerEvent::SessionRemoved { id } = &server_event
        && *id == view_state.attached_id
    {
        let removed_session_id = view_state.attached_id;
        if recover_attach_after_session_removed(client, view_state).await? {
            view_state.set_transient_status(
                format!(
                    "session {} closed; switched to active session",
                    short_uuid(removed_session_id)
                ),
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
            return Ok(AttachLoopControl::Continue);
        }
        return Ok(AttachLoopControl::Break(AttachExitReason::StreamClosed));
    }

    match server_event {
        bmux_client::ServerEvent::FollowTargetChanged {
            follower_client_id,
            leader_client_id,
            context_id,
            session_id,
        } => {
            if Some(leader_client_id) != follow_target_id
                || Some(follower_client_id) != self_client_id
            {
                return Ok(AttachLoopControl::Continue);
            }
            let attach_info = if let Some(context_id) = context_id {
                open_attach_for_context(client, context_id)
                    .await
                    .map_err(map_attach_client_error)?
            } else if view_state.attached_context_id.is_none() {
                open_attach_for_session(client, session_id)
                    .await
                    .map_err(map_attach_client_error)?
            } else {
                return Ok(AttachLoopControl::Continue);
            };
            view_state.attached_id = attach_info.session_id;
            view_state.attached_context_id = attach_info.context_id.or(context_id);
            view_state.can_write = attach_info.can_write;
            update_attach_viewport(client, view_state.attached_id, view_state.status_position)
                .await?;
            hydrate_attach_state_from_snapshot(client, view_state)
                .await
                .map_err(map_attach_client_error)?;
            refresh_attach_status_catalog_best_effort(client, view_state).await;
            view_state.ui_mode = AttachUiMode::Normal;
            let status = attach_context_status_from_catalog(view_state);
            set_attach_context_status(
                view_state,
                status,
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
            if !view_state.can_write {
                // Route the read-only notice to the status line — raw
                // mode is active here and `println!` would overwrite
                // pane content.
                view_state.set_transient_status(
                    "read-only attach: input disabled".to_string(),
                    Instant::now(),
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
            }
        }
        bmux_client::ServerEvent::FollowTargetGone {
            former_leader_client_id,
            ..
        } if Some(former_leader_client_id) == follow_target_id => {
            // Surface the detach notice through the status line; raw
            // mode is active and `println!` would corrupt rendering.
            view_state.set_transient_status(
                "follow target disconnected; staying on current session".to_string(),
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        bmux_client::ServerEvent::ControlCatalogChanged {
            revision,
            full_resync,
            ..
        } => {
            if full_resync || revision > view_state.control_catalog_revision {
                if let Err(error) = refresh_attach_status_catalog(client, view_state).await {
                    view_state.set_transient_status(
                        format!("catalog refresh failed: {error:#}"),
                        Instant::now(),
                        ATTACH_TRANSIENT_STATUS_TTL,
                    );
                } else if let Err(error) =
                    reconcile_attached_session_from_catalog(client, view_state).await
                {
                    view_state.set_transient_status(
                        format!(
                            "catalog reconcile failed: {}",
                            map_attach_client_error(error)
                        ),
                        Instant::now(),
                        ATTACH_TRANSIENT_STATUS_TTL,
                    );
                }
            }
            view_state.dirty.status_needs_redraw = true;
        }
        bmux_client::ServerEvent::AttachViewChanged {
            context_id,
            session_id,
            components,
            ..
        } if attach_view_event_matches_target(view_state, context_id, session_id) => {
            apply_attach_view_change_components(&components, view_state);
        }
        bmux_client::ServerEvent::PaneExited {
            session_id,
            pane_id,
            reason,
        } if session_id == view_state.attached_id => {
            let message = reason.map_or_else(
                || format!("pane {} exited", short_uuid(pane_id)),
                |reason| format!("pane {} exited: {reason}", short_uuid(pane_id)),
            );
            view_state.set_transient_status(message, Instant::now(), ATTACH_TRANSIENT_STATUS_TTL);
            view_state.dirty.status_needs_redraw = true;
        }
        bmux_client::ServerEvent::PaneRestarted {
            session_id,
            pane_id,
        } if session_id == view_state.attached_id => {
            view_state.set_transient_status(
                format!("pane {} restarted", short_uuid(pane_id)),
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
            view_state.dirty.status_needs_redraw = true;
        }
        _ => {}
    }

    Ok(AttachLoopControl::Continue)
}

pub fn apply_attach_view_change_components(
    components: &[AttachViewComponent],
    view_state: &mut AttachViewState,
) {
    // Components are applied sequentially in server-provided order so future
    // fine-grained refresh behavior can build on earlier invalidation steps
    // without re-sorting or undoing prior effects.
    for component in components {
        match component {
            AttachViewComponent::Scene | AttachViewComponent::Layout => {
                view_state.dirty.layout_needs_refresh = true;
                view_state.dirty.full_pane_redraw = true;
                view_state.dirty.status_needs_redraw = true;
            }
            AttachViewComponent::SurfaceContent => {
                view_state.dirty.layout_needs_refresh = true;
                view_state.dirty.full_pane_redraw = true;
            }
            AttachViewComponent::Status => {
                view_state.dirty.status_needs_redraw = true;
            }
        }
    }
}

pub async fn recover_attach_after_session_removed(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
) -> std::result::Result<bool, ClientError> {
    refresh_attach_status_catalog_best_effort(client, view_state).await;

    if let Ok(Some(context)) = typed_current_context_attach(client).await
        && retarget_attach_to_context(client, view_state, context.id)
            .await
            .is_ok()
    {
        return Ok(true);
    }

    for context in view_state.cached_contexts.clone() {
        if Some(context.id) == view_state.attached_context_id {
            continue;
        }
        if retarget_attach_to_context(client, view_state, context.id)
            .await
            .is_ok()
        {
            return Ok(true);
        }
    }

    let previous_session_id = view_state.attached_id;
    for session in view_state.cached_sessions.clone() {
        if session.id == previous_session_id {
            continue;
        }
        let Ok(attach_info) = open_attach_for_session(client, session.id).await else {
            continue;
        };
        view_state.attached_id = attach_info.session_id;
        view_state.attached_context_id = attach_info.context_id;
        view_state.can_write = attach_info.can_write;
        update_attach_viewport(client, view_state.attached_id, view_state.status_position).await?;
        hydrate_attach_state_from_snapshot(client, view_state).await?;
        refresh_attach_status_catalog_best_effort(client, view_state).await;
        view_state.ui_mode = AttachUiMode::Normal;
        let status = attach_context_status_from_catalog(view_state);
        set_attach_context_status(
            view_state,
            status,
            Instant::now(),
            ATTACH_TRANSIENT_STATUS_TTL,
        );
        return Ok(true);
    }

    Ok(false)
}

pub fn attach_view_event_matches_target(
    view_state: &AttachViewState,
    event_context_id: Option<Uuid>,
    event_session_id: Uuid,
) -> bool {
    if let Some(attached_context_id) = view_state.attached_context_id {
        return event_context_id == Some(attached_context_id);
    }
    event_session_id == view_state.attached_id
}

#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)] // Attach event handling threads shared mutable runtime state through one hot path.
pub async fn handle_attach_terminal_event(
    client: &mut StreamingBmuxClient,
    terminal_event: Event,
    attach_input_processor: &mut InputProcessor,
    help_lines: &[String],
    view_state: &mut AttachViewState,
    theme_state: &mut AttachThemeState,
    display_capture: &mut DisplayCaptureFanout,
    kernel_client_factory: Option<&KernelClientFactory>,
) -> Result<AttachLoopControl> {
    if matches!(terminal_event, Event::Resize(_, _)) {
        update_attach_viewport(client, view_state.attached_id, view_state.status_position).await?;
    }

    if view_state.prompt.is_active() {
        match &terminal_event {
            Event::Key(key) if prompt_accepts_key_kind(key.kind) => {
                match view_state.prompt.handle_key_event(key) {
                    PromptKeyDisposition::Completed(completion) => {
                        if let Some(control) = handle_attach_prompt_completion(
                            client,
                            view_state,
                            theme_state,
                            completion,
                        )
                        .await?
                        {
                            return Ok(control);
                        }
                    }
                    PromptKeyDisposition::Preview(preview) => {
                        handle_attach_prompt_preview(client, view_state, theme_state, preview)
                            .await;
                    }
                    PromptKeyDisposition::Consumed => {
                        view_state.dirty.overlay_needs_redraw = true;
                    }
                    PromptKeyDisposition::NotActive => {}
                }
                return Ok(AttachLoopControl::Continue);
            }
            Event::Key(_) | Event::Mouse(_) | Event::Paste(_) => {
                return Ok(AttachLoopControl::Continue);
            }
            _ => {}
        }
    }

    if view_state.help_overlay_open
        && let Event::Key(key) = &terminal_event
        && handle_help_overlay_key_event(key, help_lines, view_state)
    {
        return Ok(AttachLoopControl::Continue);
    }

    if matches!(terminal_event, Event::Key(_)) {
        let focused_input_mode = focused_attach_pane_input_mode(view_state);
        attach_input_processor.set_pane_input_mode(
            focused_input_mode.application_cursor,
            focused_input_mode.application_keypad,
        );
    }

    for attach_action in
        attach_event_actions(&terminal_event, attach_input_processor, view_state.ui_mode)?
    {
        match attach_action {
            AttachEventAction::Detach => {
                return try_detach_or_continue(client, view_state).await;
            }
            AttachEventAction::Send(bytes) => {
                if view_state.help_overlay_open || view_state.prompt.is_active() {
                    continue;
                }
                if view_state.can_write {
                    // Fire-and-forget: send input without waiting for the
                    // round-trip acknowledgement.  Critical failures
                    // (session removed, pane exited) are detected via
                    // server-pushed events rather than per-keystroke
                    // responses.  Only transport-level send failures are
                    // treated as fatal here.
                    if let Err(error) = client
                        .send_one_way_attach_input(view_state.attached_id, bytes)
                        .await
                    {
                        return Err(map_attach_client_error(error));
                    }
                    display_capture.record_activity(bmux_ipc::DisplayActivityKind::Input);
                }
            }
            AttachEventAction::PluginCommand {
                plugin_id,
                command_name,
                args,
            } => {
                if view_state.help_overlay_open || view_state.prompt.is_active() {
                    continue;
                }
                if let Err(error) = handle_attach_plugin_command_action(
                    client,
                    &plugin_id,
                    &command_name,
                    &args,
                    view_state,
                    kernel_client_factory,
                )
                .await
                {
                    view_state.set_transient_status(
                        format!("plugin action failed: {}", map_attach_client_error(error)),
                        Instant::now(),
                        ATTACH_TRANSIENT_STATUS_TTL,
                    );
                }
                attach_input_processor.set_scroll_mode(view_state.scrollback_active);
            }
            AttachEventAction::Mouse(mouse_event) => {
                if let Err(error) = handle_attach_mouse_event(
                    client,
                    mouse_event,
                    view_state,
                    kernel_client_factory,
                )
                .await
                {
                    view_state.set_transient_status(
                        format!("mouse action failed: {}", map_attach_client_error(error)),
                        Instant::now(),
                        ATTACH_TRANSIENT_STATUS_TTL,
                    );
                }
                attach_input_processor.set_scroll_mode(view_state.scrollback_active);
            }
            AttachEventAction::Ui(action) => {
                if let RuntimeAction::SwitchProfile(profile_id) = &action {
                    match apply_attach_profile_switch(
                        profile_id,
                        attach_input_processor,
                        view_state,
                    ) {
                        Ok(()) => {
                            view_state.dirty.layout_needs_refresh = true;
                            view_state.dirty.full_pane_redraw = true;
                            view_state.dirty.status_needs_redraw = true;
                        }
                        Err(error) => {
                            view_state.set_transient_status(
                                format!("profile switch failed: {error}"),
                                Instant::now(),
                                ATTACH_TRANSIENT_STATUS_TTL,
                            );
                        }
                    }
                    continue;
                }
                if matches!(action, RuntimeAction::ShowHelp) {
                    view_state.help_overlay_open = !view_state.help_overlay_open;
                    if view_state.help_overlay_open {
                        view_state.dirty.overlay_needs_redraw = true;
                    } else {
                        view_state.help_overlay_scroll = 0;
                        view_state.dirty.full_pane_redraw = true;
                    }
                    view_state.dirty.status_needs_redraw = true;
                    continue;
                }
                if matches!(action, RuntimeAction::ThemePicker) {
                    open_theme_picker(view_state, theme_state);
                    continue;
                }
                if view_state.help_overlay_open {
                    if matches!(action, RuntimeAction::ExitMode)
                        || matches!(action, RuntimeAction::ForwardToPane(_))
                    {
                        view_state.help_overlay_open = false;
                        view_state.help_overlay_scroll = 0;
                        view_state.dirty.status_needs_redraw = true;
                        view_state.dirty.full_pane_redraw = true;
                    }
                    continue;
                }
                let prompt_only_action = matches!(
                    action,
                    RuntimeAction::Quit | RuntimeAction::CloseFocusedPane
                );
                if let Err(error) = handle_attach_ui_action(client, action, view_state).await {
                    let mapped = map_attach_client_error(error);
                    warn!(error = %mapped, "attach.action.ui_failed");
                    view_state.set_transient_status(
                        format!("attach action failed: {mapped}"),
                        Instant::now(),
                        ATTACH_TRANSIENT_STATUS_TTL,
                    );
                } else if prompt_only_action && view_state.prompt.is_active() {
                    view_state.dirty.overlay_needs_redraw = true;
                } else {
                    view_state.dirty.layout_needs_refresh = true;
                    view_state.dirty.full_pane_redraw = true;
                }
                attach_input_processor.set_scroll_mode(view_state.scrollback_active);
                view_state.dirty.status_needs_redraw = true;
            }
            AttachEventAction::Redraw => {
                view_state.dirty.status_needs_redraw = true;
                view_state.dirty.layout_needs_refresh = true;
                view_state.dirty.full_pane_redraw = true;
            }
            AttachEventAction::Ignore => {}
        }
        sync_attach_active_mode_from_processor(
            view_state,
            attach_input_processor.keymap(),
            attach_input_processor.active_mode_id(),
        );
    }

    Ok(AttachLoopControl::Continue)
}

pub const fn prompt_response_is_confirmed(response: &PromptResponse) -> bool {
    matches!(
        response,
        PromptResponse::Submitted(PromptValue::Confirm(true))
    )
}

#[allow(clippy::too_many_lines)] // Internal prompt completions are a compact action state machine.
pub async fn handle_attach_prompt_completion(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    theme_state: &mut AttachThemeState,
    completion: AttachPromptCompletion,
) -> std::result::Result<Option<AttachLoopControl>, ClientError> {
    match completion.origin {
        AttachPromptOrigin::External { response_tx } => {
            let _ = response_tx.send(completion.response);
        }
        AttachPromptOrigin::Internal(action) => match action {
            AttachInternalPromptAction::QuitSession => {
                if prompt_response_is_confirmed(&completion.response) {
                    let selector = attached_session_selector(view_state);
                    match typed_kill_session_attach(client, selector, false).await {
                        Ok(Ok(_)) => {
                            return Ok(Some(AttachLoopControl::Break(AttachExitReason::Quit)));
                        }
                        Ok(Err(err)) => {
                            let error = ClientError::ServerError {
                                code: bmux_ipc::ErrorCode::Internal,
                                message: format!("kill-session failed: {err:?}"),
                            };
                            let status = attach_quit_failure_status(&error);
                            view_state.set_transient_status(
                                status,
                                Instant::now(),
                                ATTACH_TRANSIENT_STATUS_TTL,
                            );
                        }
                        Err(error) => {
                            let status = attach_quit_failure_status(&error);
                            view_state.set_transient_status(
                                status,
                                Instant::now(),
                                ATTACH_TRANSIENT_STATUS_TTL,
                            );
                        }
                    }
                } else {
                    view_state.set_transient_status(
                        "quit canceled",
                        Instant::now(),
                        ATTACH_TRANSIENT_STATUS_TTL,
                    );
                }
            }
            AttachInternalPromptAction::ClosePane { pane_id } => {
                if prompt_response_is_confirmed(&completion.response) {
                    let _ack: bmux_windows_plugin_api::windows_commands::PaneAck =
                        invoke_windows_command(
                            client,
                            "focus-pane",
                            &windows_cmd_args::FocusPane { id: pane_id },
                        )
                        .await?;
                    let _ack: bmux_windows_plugin_api::windows_commands::PaneAck =
                        invoke_windows_command(
                            client,
                            "close-pane",
                            &windows_cmd_args::ClosePane { id: pane_id },
                        )
                        .await?;
                    view_state.set_transient_status(
                        "pane closed",
                        Instant::now(),
                        ATTACH_TRANSIENT_STATUS_TTL,
                    );
                } else {
                    view_state.set_transient_status(
                        "close pane canceled",
                        Instant::now(),
                        ATTACH_TRANSIENT_STATUS_TTL,
                    );
                }
            }
            AttachInternalPromptAction::ThemePicker => {
                if let PromptResponse::Submitted(PromptValue::Single(name)) = completion.response {
                    if theme_state.preview(&name) {
                        theme_state.commit_preview();
                        apply_plugin_theme_extensions(
                            client,
                            theme_state.active_theme.clone(),
                            &theme_state.theme_plugin_ids,
                            &theme_state.config_dir_candidates,
                        )
                        .await;
                        view_state.set_transient_status(
                            format!("theme: {}", theme_state.active_name),
                            Instant::now(),
                            ATTACH_TRANSIENT_STATUS_TTL,
                        );
                    }
                } else {
                    theme_state.cancel_preview();
                    apply_plugin_theme_extensions(
                        client,
                        theme_state.active_theme.clone(),
                        &theme_state.theme_plugin_ids,
                        &theme_state.config_dir_candidates,
                    )
                    .await;
                    view_state.set_transient_status(
                        "theme switch canceled",
                        Instant::now(),
                        ATTACH_TRANSIENT_STATUS_TTL,
                    );
                }
            }
        },
    }

    view_state.dirty.status_needs_redraw = true;
    view_state.dirty.full_pane_redraw = true;
    Ok(None)
}

async fn handle_attach_prompt_preview(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    theme_state: &mut AttachThemeState,
    preview: AttachPromptPreview,
) {
    match preview {
        AttachPromptPreview::Theme { name } => {
            if theme_state.preview(&name) {
                apply_plugin_theme_extensions(
                    client,
                    theme_state.active_theme.clone(),
                    &theme_state.theme_plugin_ids,
                    &theme_state.config_dir_candidates,
                )
                .await;
                view_state.dirty.status_needs_redraw = true;
                view_state.dirty.full_pane_redraw = true;
                view_state.dirty.overlay_needs_redraw = true;
            }
        }
    }
}

/// Handle an action dispatch request from async plugin code.
///
/// Parses the action string into a `RuntimeAction` and routes it through
/// the same dispatch path as keybinding-triggered actions.
async fn handle_attach_action_dispatch(
    client: &mut StreamingBmuxClient,
    dispatch_request: bmux_plugin_sdk::ActionDispatchRequest,
    view_state: &mut AttachViewState,
    theme_state: &mut AttachThemeState,
    kernel_client_factory: Option<&KernelClientFactory>,
) -> Result<AttachLoopControl> {
    let action_str = &dispatch_request.action;
    let action = match parse_action(action_str) {
        Ok(action) => action,
        Err(error) => {
            view_state.set_transient_status(
                format!("invalid dispatched action: {error}"),
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
            return Ok(AttachLoopControl::Continue);
        }
    };

    let event_action = runtime_action_to_attach_event_action(action);

    match event_action {
        AttachEventAction::Detach => {
            return try_detach_or_continue(client, view_state).await;
        }
        AttachEventAction::Send(bytes) => {
            if view_state.can_write
                && let Err(error) = client
                    .send_one_way_attach_input(view_state.attached_id, bytes)
                    .await
            {
                return Err(map_attach_client_error(error));
            }
        }
        AttachEventAction::PluginCommand {
            plugin_id,
            command_name,
            args,
        } => {
            if let Err(error) = handle_attach_plugin_command_action(
                client,
                &plugin_id,
                &command_name,
                &args,
                view_state,
                kernel_client_factory,
            )
            .await
            {
                view_state.set_transient_status(
                    format!(
                        "dispatched plugin action failed: {}",
                        map_attach_client_error(error)
                    ),
                    Instant::now(),
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
            }
        }
        AttachEventAction::Ui(ui_action) => {
            if matches!(ui_action, RuntimeAction::ThemePicker) {
                open_theme_picker(view_state, theme_state);
                view_state.dirty.status_needs_redraw = true;
                view_state.dirty.overlay_needs_redraw = true;
                return Ok(AttachLoopControl::Continue);
            }
            if let Err(error) = handle_attach_ui_action(client, ui_action, view_state).await {
                view_state.set_transient_status(
                    format!(
                        "dispatched action failed: {}",
                        map_attach_client_error(error)
                    ),
                    Instant::now(),
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
            } else {
                view_state.dirty.layout_needs_refresh = true;
                view_state.dirty.full_pane_redraw = true;
            }
            view_state.dirty.status_needs_redraw = true;
        }
        AttachEventAction::Redraw => {
            view_state.dirty.status_needs_redraw = true;
            view_state.dirty.layout_needs_refresh = true;
            view_state.dirty.full_pane_redraw = true;
        }
        AttachEventAction::Mouse(_) | AttachEventAction::Ignore => {}
    }

    Ok(AttachLoopControl::Continue)
}

async fn try_detach_or_continue(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
) -> Result<AttachLoopControl> {
    match client.detach().await {
        Ok(()) => Ok(AttachLoopControl::Break(AttachExitReason::Detached)),
        Err(error) => {
            view_state.set_transient_status(
                format!("detach blocked: {}", map_attach_client_error(error)),
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
            Ok(AttachLoopControl::Continue)
        }
    }
}

#[allow(clippy::too_many_lines)]
pub fn record_attach_mouse_event(mouse_event: MouseEvent, view_state: &mut AttachViewState) {
    view_state.mouse.last_position = Some((mouse_event.column, mouse_event.row));
    view_state.mouse.last_event_at = Some(Instant::now());
}

#[allow(clippy::too_many_lines)]
pub async fn handle_attach_mouse_event(
    client: &mut StreamingBmuxClient,
    mouse_event: MouseEvent,
    view_state: &mut AttachViewState,
    kernel_client_factory: Option<&KernelClientFactory>,
) -> std::result::Result<(), ClientError> {
    record_attach_mouse_event(mouse_event, view_state);

    if !view_state.mouse.config.enabled {
        return Ok(());
    }
    if view_state.help_overlay_open || view_state.prompt.is_active() {
        return Ok(());
    }

    if matches!(mouse_event.kind, MouseEventKind::Down(MouseButton::Left))
        && handle_attach_status_tab_click(client, view_state, mouse_event).await?
    {
        return Ok(());
    }

    if !view_state.can_write {
        return Ok(());
    }

    let target_pane = attach_scene_pane_at(view_state, mouse_event.column, mouse_event.row);
    let focused_pane = view_state
        .cached_layout_state
        .as_ref()
        .map(|layout| layout.focused_pane_id);
    let in_focused_pane = target_pane.is_some() && target_pane == focused_pane;

    if matches!(mouse_event.kind, MouseEventKind::ScrollUp)
        && handle_attach_mouse_gesture_action(
            client,
            view_state,
            "scroll_up",
            kernel_client_factory,
        )
        .await?
    {
        return Ok(());
    }
    if matches!(mouse_event.kind, MouseEventKind::ScrollDown)
        && handle_attach_mouse_gesture_action(
            client,
            view_state,
            "scroll_down",
            kernel_client_factory,
        )
        .await?
    {
        return Ok(());
    }

    if matches!(
        mouse_event.kind,
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
    ) {
        match view_state.mouse.config.effective_wheel_propagation() {
            bmux_config::MouseWheelPropagation::ForwardOnly => {
                let _ = maybe_forward_attach_mouse_event(
                    client,
                    view_state,
                    mouse_event,
                    target_pane,
                    in_focused_pane,
                    false,
                )
                .await?;
                return Ok(());
            }
            bmux_config::MouseWheelPropagation::ScrollbackOnly => {
                let _ = handle_attach_mouse_scrollback(view_state, mouse_event.kind);
                return Ok(());
            }
            bmux_config::MouseWheelPropagation::ForwardAndScrollback => {
                let _ = maybe_forward_attach_mouse_event(
                    client,
                    view_state,
                    mouse_event,
                    target_pane,
                    in_focused_pane,
                    false,
                )
                .await?;
                let _ = handle_attach_mouse_scrollback(view_state, mouse_event.kind);
                return Ok(());
            }
        }
    }

    match mouse_event.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let target = target_pane;
            view_state.mouse.hovered_pane_id = target;
            view_state.mouse.hover_started_at = Some(Instant::now());
            if !handle_attach_mouse_gesture_action(
                client,
                view_state,
                "click_left",
                kernel_client_factory,
            )
            .await?
            {
                match view_state.mouse.config.effective_click_propagation() {
                    bmux_config::MouseClickPropagation::FocusOnly => {
                        if let Some(pane_id) = target {
                            focus_attach_pane(client, view_state, pane_id).await?;
                        }
                    }
                    bmux_config::MouseClickPropagation::ForwardOnly => {
                        let _ = maybe_forward_attach_mouse_event(
                            client,
                            view_state,
                            mouse_event,
                            target,
                            in_focused_pane,
                            false,
                        )
                        .await?;
                    }
                    bmux_config::MouseClickPropagation::FocusAndForward => {
                        let _ = maybe_forward_attach_mouse_event(
                            client,
                            view_state,
                            mouse_event,
                            target,
                            in_focused_pane,
                            true,
                        )
                        .await?;
                    }
                }
            }
        }
        MouseEventKind::Down(_) | MouseEventKind::Up(_) | MouseEventKind::Drag(_) => {
            if should_forward_click_like_mouse(view_state) {
                let _ = maybe_forward_attach_mouse_event(
                    client,
                    view_state,
                    mouse_event,
                    target_pane,
                    in_focused_pane,
                    false,
                )
                .await?;
            }
        }
        MouseEventKind::Moved => {
            let _ = maybe_forward_attach_mouse_event(
                client,
                view_state,
                mouse_event,
                target_pane,
                in_focused_pane,
                false,
            )
            .await?;

            if view_state.mouse.config.focus_on_hover {
                let now = Instant::now();
                let target = target_pane;
                if target != view_state.mouse.hovered_pane_id {
                    view_state.mouse.hovered_pane_id = target;
                    view_state.mouse.hover_started_at = Some(now);
                    return Ok(());
                }

                let Some(pane_id) = target else {
                    view_state.mouse.hover_started_at = None;
                    return Ok(());
                };

                if view_state.mouse.last_focused_pane_id == Some(pane_id) {
                    return Ok(());
                }

                let Some(hover_started_at) = view_state.mouse.hover_started_at else {
                    view_state.mouse.hover_started_at = Some(now);
                    return Ok(());
                };

                if now.duration_since(hover_started_at)
                    >= Duration::from_millis(view_state.mouse.config.hover_delay_ms)
                {
                    if !handle_attach_mouse_gesture_action(
                        client,
                        view_state,
                        "hover_focus",
                        kernel_client_factory,
                    )
                    .await?
                    {
                        focus_attach_pane(client, view_state, pane_id).await?;
                    }
                    view_state.mouse.hover_started_at = Some(now);
                }
            }
        }
        MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => {
            let _ = maybe_forward_attach_mouse_event(
                client,
                view_state,
                mouse_event,
                target_pane,
                in_focused_pane,
                false,
            )
            .await?;
        }
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {}
    }

    Ok(())
}

pub const fn should_forward_click_like_mouse(view_state: &AttachViewState) -> bool {
    !matches!(
        view_state.mouse.config.effective_click_propagation(),
        bmux_config::MouseClickPropagation::FocusOnly
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttachPaneMouseProtocol {
    pub mode: vt100::MouseProtocolMode,
    pub encoding: vt100::MouseProtocolEncoding,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct AttachPaneInputMode {
    pub application_cursor: bool,
    pub application_keypad: bool,
}

pub const fn mouse_protocol_mode_to_ipc(
    mode: vt100::MouseProtocolMode,
) -> bmux_ipc::AttachMouseProtocolMode {
    match mode {
        vt100::MouseProtocolMode::None => bmux_ipc::AttachMouseProtocolMode::None,
        vt100::MouseProtocolMode::Press => bmux_ipc::AttachMouseProtocolMode::Press,
        vt100::MouseProtocolMode::PressRelease => bmux_ipc::AttachMouseProtocolMode::PressRelease,
        vt100::MouseProtocolMode::ButtonMotion => bmux_ipc::AttachMouseProtocolMode::ButtonMotion,
        vt100::MouseProtocolMode::AnyMotion => bmux_ipc::AttachMouseProtocolMode::AnyMotion,
    }
}

pub const fn mouse_protocol_encoding_to_ipc(
    encoding: vt100::MouseProtocolEncoding,
) -> bmux_ipc::AttachMouseProtocolEncoding {
    match encoding {
        vt100::MouseProtocolEncoding::Default => bmux_ipc::AttachMouseProtocolEncoding::Default,
        vt100::MouseProtocolEncoding::Utf8 => bmux_ipc::AttachMouseProtocolEncoding::Utf8,
        vt100::MouseProtocolEncoding::Sgr => bmux_ipc::AttachMouseProtocolEncoding::Sgr,
    }
}

pub fn attach_pane_mouse_protocol(
    view_state: &AttachViewState,
    pane_id: Uuid,
) -> Option<AttachPaneMouseProtocol> {
    let protocol = attach_mouse::pane_protocol(
        &view_state.pane_buffers,
        &view_state.pane_mouse_protocol_hints,
        pane_id,
    )?;
    Some(AttachPaneMouseProtocol {
        mode: protocol.mode,
        encoding: protocol.encoding,
    })
}

pub fn attach_pane_input_mode(
    view_state: &AttachViewState,
    pane_id: Uuid,
) -> Option<AttachPaneInputMode> {
    let parser_mode = view_state.pane_buffers.get(&pane_id).map(|buffer| {
        let screen = buffer.parser.screen();
        AttachPaneInputMode {
            application_cursor: screen.application_cursor(),
            application_keypad: screen.application_keypad(),
        }
    });

    let hint_mode =
        view_state
            .pane_input_mode_hints
            .get(&pane_id)
            .map(|hint| AttachPaneInputMode {
                application_cursor: hint.application_cursor,
                application_keypad: hint.application_keypad,
            });

    match (parser_mode, hint_mode) {
        (Some(parser), Some(hint)) => Some(AttachPaneInputMode {
            application_cursor: parser.application_cursor || hint.application_cursor,
            application_keypad: parser.application_keypad || hint.application_keypad,
        }),
        (Some(parser), None) => Some(parser),
        (None, Some(hint)) => Some(hint),
        (None, None) => None,
    }
}

pub fn focused_attach_pane_input_mode(view_state: &AttachViewState) -> AttachPaneInputMode {
    focused_attach_pane_id(view_state)
        .and_then(|pane_id| attach_pane_input_mode(view_state, pane_id))
        .unwrap_or_default()
}

#[cfg(test)]
pub const fn mouse_protocol_mode_reports_event(
    mode: vt100::MouseProtocolMode,
    kind: MouseEventKind,
) -> bool {
    attach_mouse::mode_reports_event(mode, mouse_event_kind_to_shared(kind))
}

#[cfg(test)]
pub fn encode_attach_mouse_for_protocol(
    mouse_event: MouseEvent,
    protocol: AttachPaneMouseProtocol,
) -> Option<Vec<u8>> {
    attach_mouse::encode_for_protocol(
        mouse_event_to_shared(mouse_event),
        attach_mouse::PaneProtocol {
            mode: protocol.mode,
            encoding: protocol.encoding,
        },
    )
}

pub async fn maybe_forward_attach_mouse_event(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    mouse_event: MouseEvent,
    target_pane: Option<Uuid>,
    in_focused_pane: bool,
    focus_before_forward: bool,
) -> std::result::Result<bool, ClientError> {
    let Some(target_pane) = target_pane else {
        return Ok(false);
    };

    if focus_before_forward && !in_focused_pane {
        focus_attach_pane(client, view_state, target_pane).await?;
    } else if !in_focused_pane {
        return Ok(false);
    }

    let Some(bytes) = attach_mouse_forward_bytes_for_target(
        view_state,
        mouse_event,
        Some(target_pane),
        in_focused_pane || focus_before_forward,
    ) else {
        return Ok(false);
    };

    let () = client.attach_input(view_state.attached_id, bytes).await?;
    Ok(true)
}

pub fn attach_mouse_forward_bytes_for_target(
    view_state: &AttachViewState,
    mouse_event: MouseEvent,
    target_pane: Option<Uuid>,
    in_focused_pane: bool,
) -> Option<Vec<u8>> {
    if !in_focused_pane {
        return None;
    }
    let target_pane = target_pane?;
    let protocol = attach_pane_mouse_protocol(view_state, target_pane)?;
    // Programs running inside a pane (nvim, tmux, etc.) expect mouse
    // coordinates relative to the pane's own virtual terminal (its PTY
    // interior), not the whole attach UI and not the outer surface bounds
    // (which include any decoration/border painted by a plugin). We
    // translate against `content_rect` — the authoritative PTY interior
    // published by the scene producer — so clicks on the first visible
    // content cell encode to SGR `(1, 1)` regardless of how thick or thin
    // the surrounding decoration is.
    let pane_content_rect = attach_scene_pane_content_rect(view_state, target_pane)?;
    let shared_event = mouse_event_to_shared(mouse_event);
    let local_event = attach_mouse::translate_event_to_pane_local(shared_event, pane_content_rect)?;
    attach_mouse::encode_for_protocol(
        local_event,
        attach_mouse::PaneProtocol {
            mode: protocol.mode,
            encoding: protocol.encoding,
        },
    )
}

fn attach_scene_pane_content_rect(
    view_state: &AttachViewState,
    pane_id: Uuid,
) -> Option<bmux_ipc::AttachRect> {
    let layout_state = view_state.cached_layout_state.as_ref()?;
    let mut best: Option<(
        bmux_ipc::AttachLayer,
        i32,
        usize,
        uuid::Uuid,
        bmux_ipc::AttachRect,
    )> = None;
    for (index, surface) in layout_state.scene.surfaces.iter().enumerate() {
        if surface.pane_id != Some(pane_id) {
            continue;
        }
        if !surface.visible || !surface.accepts_input {
            continue;
        }
        let candidate = (
            surface.layer,
            surface.z,
            index,
            surface.id,
            surface.content_rect,
        );
        if best.as_ref().is_none_or(|current| {
            (candidate.0, candidate.1, candidate.2) > (current.0, current.1, current.2)
        }) {
            best = Some(candidate);
        }
    }
    let (_, _, _, surface_id, scene_rect) = best?;

    // Prefer any render extension's published `content_rect` override
    // for this surface. Extensions (decoration renderer, overlays,
    // etc.) are the authoritative source of chrome insets; core
    // merely consults them. When no extension overrides, fall back
    // to the scene producer's content rect.
    for ext in bmux_plugin::registered_render_extensions() {
        if let Some(override_rect) = ext.content_rect_override(surface_id)
            && override_rect.w > 0
            && override_rect.h > 0
        {
            return Some(bmux_ipc::AttachRect {
                x: override_rect.x,
                y: override_rect.y,
                w: override_rect.w,
                h: override_rect.h,
            });
        }
    }

    Some(scene_rect)
}

#[cfg(test)]
pub fn encode_attach_mouse_sgr(mouse_event: MouseEvent) -> Option<Vec<u8>> {
    attach_mouse::encode_sgr(mouse_event_to_shared(mouse_event))
}

const fn mouse_button_to_shared(button: MouseButton) -> attach_mouse::Button {
    match button {
        MouseButton::Left => attach_mouse::Button::Left,
        MouseButton::Middle => attach_mouse::Button::Middle,
        MouseButton::Right => attach_mouse::Button::Right,
    }
}

const fn mouse_event_kind_to_shared(kind: MouseEventKind) -> attach_mouse::EventKind {
    match kind {
        MouseEventKind::Down(button) => {
            attach_mouse::EventKind::Down(mouse_button_to_shared(button))
        }
        MouseEventKind::Up(button) => attach_mouse::EventKind::Up(mouse_button_to_shared(button)),
        MouseEventKind::Drag(button) => {
            attach_mouse::EventKind::Drag(mouse_button_to_shared(button))
        }
        MouseEventKind::Moved => attach_mouse::EventKind::Moved,
        MouseEventKind::ScrollUp => attach_mouse::EventKind::ScrollUp,
        MouseEventKind::ScrollDown => attach_mouse::EventKind::ScrollDown,
        MouseEventKind::ScrollLeft => attach_mouse::EventKind::ScrollLeft,
        MouseEventKind::ScrollRight => attach_mouse::EventKind::ScrollRight,
    }
}

const fn key_modifiers_to_shared(modifiers: KeyModifiers) -> attach_mouse::Modifiers {
    attach_mouse::Modifiers {
        shift: modifiers.contains(KeyModifiers::SHIFT),
        alt: modifiers.contains(KeyModifiers::ALT),
        control: modifiers.contains(KeyModifiers::CONTROL),
    }
}

const fn mouse_event_to_shared(mouse_event: MouseEvent) -> attach_mouse::Event {
    attach_mouse::Event {
        kind: mouse_event_kind_to_shared(mouse_event.kind),
        column: mouse_event.column,
        row: mouse_event.row,
        modifiers: key_modifiers_to_shared(mouse_event.modifiers),
    }
}

pub async fn handle_attach_status_tab_click(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    mouse_event: MouseEvent,
) -> std::result::Result<bool, ClientError> {
    let (cols, rows) = terminal::size().unwrap_or((0, 0));
    if cols == 0 || rows == 0 {
        trace!("attach.status_click.ignored.empty_terminal");
        return Ok(false);
    }
    let Some(status_row) = status_row_for_position(view_state.status_position, rows) else {
        trace!("attach.status_click.ignored.status_off");
        return Ok(false);
    };
    if !status_row_matches_mouse(status_row, mouse_event.row, rows) {
        trace!(
            mouse_row = mouse_event.row,
            status_row, rows, "attach.status_click.ignored.row_mismatch"
        );
        return Ok(false);
    }

    let Some(status_line) = view_state.cached_status_line.as_ref() else {
        trace!("attach.status_click.ignored.no_cached_status");
        return Ok(false);
    };
    trace!(
        mouse_col = mouse_event.column,
        mouse_row = mouse_event.row,
        status_row,
        hitbox_count = status_line.tab_hitboxes.len(),
        "attach.status_click.inspect"
    );
    let Some(target_context_id) = status_line
        .tab_hitboxes
        .iter()
        .find(|hitbox| {
            mouse_event.column >= hitbox.start_col && mouse_event.column <= hitbox.end_col
        })
        .map(|hitbox| hitbox.context_id)
    else {
        trace!("attach.status_click.ignored.no_hitbox_match");
        return Ok(false);
    };

    debug!(target_context_id = %target_context_id, "attach.status_click.retarget");

    retarget_attach_to_context(client, view_state, target_context_id).await?;
    view_state.dirty.status_needs_redraw = true;
    view_state.dirty.layout_needs_refresh = true;
    view_state.dirty.full_pane_redraw = true;
    Ok(true)
}

pub const fn status_row_matches_mouse(status_row: u16, mouse_row: u16, rows: u16) -> bool {
    if mouse_row == status_row {
        return true;
    }
    if mouse_row > 0 && mouse_row.saturating_sub(1) == status_row {
        return true;
    }
    rows > 0 && mouse_row == rows && status_row == rows.saturating_sub(1)
}

pub async fn handle_attach_mouse_gesture_action(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    gesture: &str,
    kernel_client_factory: Option<&KernelClientFactory>,
) -> std::result::Result<bool, ClientError> {
    let Some(attach_action) = resolve_mouse_gesture_action(view_state, gesture) else {
        return Ok(false);
    };

    match attach_action {
        AttachEventAction::PluginCommand {
            plugin_id,
            command_name,
            args,
        } => {
            handle_attach_plugin_command_action(
                client,
                &plugin_id,
                &command_name,
                &args,
                view_state,
                kernel_client_factory,
            )
            .await?;
            Ok(true)
        }
        AttachEventAction::Ui(action) => {
            if let Err(error) = handle_attach_ui_action(client, action, view_state).await {
                view_state.set_transient_status(
                    format!("mouse action failed: {}", map_attach_client_error(error)),
                    Instant::now(),
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
            } else {
                view_state.dirty.status_needs_redraw = true;
                view_state.dirty.layout_needs_refresh = true;
                view_state.dirty.full_pane_redraw = true;
            }
            Ok(true)
        }
        AttachEventAction::Ignore => Ok(true),
        AttachEventAction::Detach
        | AttachEventAction::Send(_)
        | AttachEventAction::Mouse(_)
        | AttachEventAction::Redraw => Ok(false),
    }
}

pub fn resolve_mouse_gesture_action(
    view_state: &AttachViewState,
    gesture: &str,
) -> Option<AttachEventAction> {
    let action_name = view_state.mouse.config.gesture_actions.get(gesture)?;
    match crate::input::parse_runtime_action_name(action_name) {
        Ok(action) => Some(runtime_action_to_attach_event_action(action)),
        Err(error) => {
            warn!(
                gesture = %gesture,
                action_name = %action_name,
                error = %error,
                "attach.mouse_gesture.invalid_action"
            );
            None
        }
    }
}

pub fn handle_attach_mouse_scrollback(
    view_state: &mut AttachViewState,
    kind: MouseEventKind,
) -> bool {
    if !view_state.mouse.config.scroll_scrollback {
        return false;
    }

    #[allow(clippy::cast_possible_wrap)]
    // scroll_lines_per_tick is a small u16, always fits in isize
    let lines = view_state.mouse.config.scroll_lines_per_tick.max(1) as isize;
    match kind {
        MouseEventKind::ScrollUp => {
            if !view_state.scrollback_active && !enter_attach_scrollback(view_state) {
                return false;
            }
            step_attach_scrollback(view_state, -lines);
            view_state.dirty.full_pane_redraw = true;
            view_state.dirty.status_needs_redraw = true;
            true
        }
        MouseEventKind::ScrollDown => {
            if !view_state.scrollback_active {
                return false;
            }
            step_attach_scrollback(view_state, lines);
            if view_state.mouse.config.exit_scrollback_on_bottom
                && view_state.scrollback_offset == 0
                && !view_state.selection_active()
            {
                view_state.exit_scrollback();
            }
            view_state.dirty.full_pane_redraw = true;
            view_state.dirty.status_needs_redraw = true;
            true
        }
        _ => false,
    }
}

pub async fn focus_attach_pane(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    pane_id: Uuid,
) -> std::result::Result<(), ClientError> {
    if view_state.mouse.last_focused_pane_id == Some(pane_id) {
        return Ok(());
    }

    let _ack: bmux_windows_plugin_api::windows_commands::PaneAck = invoke_windows_command(
        client,
        "focus-pane",
        &windows_cmd_args::FocusPane { id: pane_id },
    )
    .await?;

    view_state.mouse.last_focused_pane_id = Some(pane_id);
    view_state.dirty.layout_needs_refresh = true;
    view_state.dirty.full_pane_redraw = true;
    view_state.dirty.status_needs_redraw = true;

    Ok(())
}

pub fn attach_scene_pane_at(view_state: &AttachViewState, column: u16, row: u16) -> Option<Uuid> {
    let layout_state = view_state.cached_layout_state.as_ref()?;
    attach_mouse::pane_at(&layout_state.scene, column, row)
}

pub fn restore_terminal_after_attach_ui() -> Result<()> {
    let mut stdout = io::stdout();
    // Safety net: restore terminal input flags in case the drop guard didn't run.
    #[cfg(feature = "kitty-keyboard")]
    let _ = queue!(stdout, crossterm::event::PopKeyboardEnhancementFlags);
    let _ = queue!(stdout, DisableMouseCapture);
    queue!(
        stdout,
        Show,
        Print("\x1b[0m"),
        MoveTo(0, 0),
        Clear(ClearType::All),
        MoveTo(0, 0)
    )
    .context("failed restoring terminal after attach ui")?;
    stdout
        .flush()
        .context("failed flushing terminal restoration")
}

pub fn attach_event_actions(
    event: &Event,
    attach_input_processor: &mut InputProcessor,
    ui_mode: AttachUiMode,
) -> Result<Vec<AttachEventAction>> {
    match event {
        Event::Key(key) => attach_key_event_actions(key, attach_input_processor, ui_mode),
        Event::Mouse(mouse) => Ok(vec![AttachEventAction::Mouse(*mouse)]),
        Event::Resize(_, _) => Ok(vec![AttachEventAction::Redraw]),
        Event::FocusGained | Event::FocusLost | Event::Paste(_) => {
            Ok(vec![AttachEventAction::Ignore])
        }
    }
}

#[allow(clippy::unnecessary_wraps)] // Result aligns with the broader action dispatch interface
pub fn attach_key_event_actions(
    key: &KeyEvent,
    attach_input_processor: &mut InputProcessor,
    _ui_mode: AttachUiMode,
) -> Result<Vec<AttachEventAction>> {
    // Trace every accepted key event at `trace` level so input-path
    // duplication (e.g. terminal-emitted Press+Repeat on a quick tap)
    // is diagnosable without spamming the log at default levels.
    // Enable with `RUST_LOG=bmux_cli::runtime::attach::input=trace`.
    let span = tracing::trace_span!(
        target: "bmux_cli::runtime::attach::input",
        "attach_key_event",
        kind = ?key.kind,
        code = ?key.code,
        modifiers = ?key.modifiers,
        emitted = tracing::field::Empty,
        dropped_by_repeat_filter = tracing::field::Empty,
    );
    let _enter = span.enter();

    // Release events are filtered out here and also inside
    // InputProcessor's crossterm adapter as a safety net.
    if key.kind == KeyEventKind::Release {
        span.record("emitted", 0usize);
        return Ok(vec![AttachEventAction::Ignore]);
    }

    let actions = attach_input_processor.process_terminal_event(Event::Key(*key));
    let pre_filter_count = actions.len();
    // Auto-repeat handling. Under kitty keyboard protocol (and on
    // platforms where the OS emits repeat events liberally), a single
    // quick tap can produce a `Press` followed by one or more `Repeat`
    // events before the key is released. State-mutating actions
    // (creating windows, closing panes, invoking plugin commands) must
    // fire exactly once per logical press; navigation-style actions
    // (focus, scroll, resize) benefit from repeat. `action_supports_repeat`
    // classifies each resolved action so mutating ones are silently
    // dropped on `Repeat` events.
    let is_repeat = key.kind == KeyEventKind::Repeat;
    let filtered: Vec<_> = actions
        .into_iter()
        .filter(|action| !is_repeat || action_supports_repeat(action))
        .map(runtime_action_to_attach_event_action)
        .collect();
    let dropped = pre_filter_count.saturating_sub(filtered.len());
    span.record("emitted", filtered.len());
    span.record("dropped_by_repeat_filter", dropped);
    if dropped > 0 {
        // When the repeat filter drops actions we elevate to `debug`
        // so operators investigating multi-press bugs see the drop
        // without enabling `trace`.
        tracing::debug!(
            target: "bmux_cli::runtime::attach::input",
            kind = ?key.kind,
            code = ?key.code,
            dropped,
            "repeat filter dropped repeat-unsafe actions"
        );
    }
    Ok(filtered)
}

/// Returns `true` when the action's semantics make sense under key
/// auto-repeat.
///
/// Mutating actions (new/close/kill, plugin commands by default,
/// detach, exit) return `false` — each press must create exactly one
/// mutation. Navigation and adjustment actions (focus, scroll,
/// resize) return `true` so hold-to-navigate / hold-to-resize behave
/// the same as pressing repeatedly.
///
/// For `RuntimeAction::PluginCommand`, repeat-eligibility is
/// delegated to the plugin's manifest: commands that declare
/// `accepts_repeat = true` in `plugin.toml` are repeat-eligible.
/// This is the generic mechanism that replaces the historical
/// exhaustive match over every domain `RuntimeAction` variant; new
/// repeat-eligible commands are added by plugins, not by core.
pub(super) fn action_supports_repeat(action: &RuntimeAction) -> bool {
    match action {
        // Navigation — safe to repeat.
        RuntimeAction::FocusNext
        | RuntimeAction::FocusPrev
        | RuntimeAction::FocusLeft
        | RuntimeAction::FocusRight
        | RuntimeAction::FocusUp
        | RuntimeAction::FocusDown
        | RuntimeAction::WindowNext
        | RuntimeAction::WindowPrev
        | RuntimeAction::ScrollUpLine
        | RuntimeAction::ScrollDownLine
        | RuntimeAction::ScrollUpPage
        | RuntimeAction::ScrollDownPage
        | RuntimeAction::MoveCursorLeft
        | RuntimeAction::MoveCursorRight
        | RuntimeAction::MoveCursorUp
        | RuntimeAction::MoveCursorDown
        | RuntimeAction::IncreaseSplit
        | RuntimeAction::DecreaseSplit
        | RuntimeAction::ResizeLeft
        | RuntimeAction::ResizeRight
        | RuntimeAction::ResizeUp
        | RuntimeAction::ResizeDown
        | RuntimeAction::ForwardToPane(_) => true,
        // Plugin commands: delegate to the manifest flag.
        RuntimeAction::PluginCommand {
            plugin_id,
            command_name,
            ..
        } => command_accepts_repeat(plugin_id, command_name),
        // Mutating and mode-switching — never repeat.
        RuntimeAction::Quit
        | RuntimeAction::Detach
        | RuntimeAction::ToggleSplitDirection
        | RuntimeAction::SplitFocusedVertical
        | RuntimeAction::SplitFocusedHorizontal
        | RuntimeAction::RestartFocusedPane
        | RuntimeAction::CloseFocusedPane
        | RuntimeAction::ZoomPane
        | RuntimeAction::ThemePicker
        | RuntimeAction::ShowHelp
        | RuntimeAction::EnterScrollMode
        | RuntimeAction::ExitScrollMode
        | RuntimeAction::ScrollTop
        | RuntimeAction::ScrollBottom
        | RuntimeAction::BeginSelection
        | RuntimeAction::CopyScrollback
        | RuntimeAction::ConfirmScrollback
        | RuntimeAction::EnterWindowMode
        | RuntimeAction::ExitMode
        | RuntimeAction::WindowGoto1
        | RuntimeAction::WindowGoto2
        | RuntimeAction::WindowGoto3
        | RuntimeAction::WindowGoto4
        | RuntimeAction::WindowGoto5
        | RuntimeAction::WindowGoto6
        | RuntimeAction::WindowGoto7
        | RuntimeAction::WindowGoto8
        | RuntimeAction::WindowGoto9
        | RuntimeAction::WindowClose
        | RuntimeAction::EnterMode(_)
        | RuntimeAction::SwitchProfile(_) => false,
    }
}

pub fn runtime_action_to_attach_event_action(action: RuntimeAction) -> AttachEventAction {
    match action {
        RuntimeAction::Detach => AttachEventAction::Detach,
        RuntimeAction::ForwardToPane(bytes) => AttachEventAction::Send(bytes),
        RuntimeAction::PluginCommand {
            plugin_id,
            command_name,
            args,
        } => AttachEventAction::PluginCommand {
            plugin_id,
            command_name,
            args,
        },
        RuntimeAction::EnterWindowMode
        | RuntimeAction::SplitFocusedVertical
        | RuntimeAction::SplitFocusedHorizontal
        | RuntimeAction::FocusNext
        | RuntimeAction::FocusPrev
        | RuntimeAction::FocusLeft
        | RuntimeAction::FocusRight
        | RuntimeAction::FocusUp
        | RuntimeAction::FocusDown
        | RuntimeAction::IncreaseSplit
        | RuntimeAction::DecreaseSplit
        | RuntimeAction::ResizeLeft
        | RuntimeAction::ResizeRight
        | RuntimeAction::ResizeUp
        | RuntimeAction::ResizeDown
        | RuntimeAction::CloseFocusedPane
        | RuntimeAction::ZoomPane
        | RuntimeAction::ExitMode
        | RuntimeAction::WindowPrev
        | RuntimeAction::WindowNext
        | RuntimeAction::WindowGoto1
        | RuntimeAction::WindowGoto2
        | RuntimeAction::WindowGoto3
        | RuntimeAction::WindowGoto4
        | RuntimeAction::WindowGoto5
        | RuntimeAction::WindowGoto6
        | RuntimeAction::WindowGoto7
        | RuntimeAction::WindowGoto8
        | RuntimeAction::WindowGoto9
        | RuntimeAction::WindowClose
        | RuntimeAction::Quit
        | RuntimeAction::ThemePicker
        | RuntimeAction::ShowHelp
        | RuntimeAction::ToggleSplitDirection
        | RuntimeAction::RestartFocusedPane
        | RuntimeAction::EnterMode(_)
        | RuntimeAction::SwitchProfile(_)
        | RuntimeAction::EnterScrollMode
        | RuntimeAction::ExitScrollMode
        | RuntimeAction::ScrollUpLine
        | RuntimeAction::ScrollDownLine
        | RuntimeAction::ScrollUpPage
        | RuntimeAction::ScrollDownPage
        | RuntimeAction::ScrollTop
        | RuntimeAction::ScrollBottom
        | RuntimeAction::BeginSelection
        | RuntimeAction::MoveCursorLeft
        | RuntimeAction::MoveCursorRight
        | RuntimeAction::MoveCursorUp
        | RuntimeAction::MoveCursorDown
        | RuntimeAction::CopyScrollback
        | RuntimeAction::ConfirmScrollback => AttachEventAction::Ui(action),
    }
}

pub fn is_attach_stream_closed_error(error: &ClientError) -> bool {
    matches!(
        error,
        ClientError::ServerError { code: bmux_ipc::ErrorCode::NotFound, message }
            if message.contains("session runtime not found")
    )
}

pub fn is_attach_not_attached_runtime_error(error: &ClientError) -> bool {
    matches!(
        error,
        ClientError::ServerError { message, .. }
            if message.contains("not attached to session runtime")
    )
}
#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;
    use crate::input::InputProcessor;
    use crate::runtime::attach::render::append_pane_output;
    use crate::runtime::attach::state::{
        AttachEventAction, AttachScrollbackCursor, AttachScrollbackPosition, AttachUiMode,
        AttachViewState, PaneRenderBuffer,
    };

    use bmux_client::{AttachLayoutState, AttachOpenInfo};
    use bmux_config::{BmuxConfig, MouseClickPropagation, MouseWheelPropagation};
    use bmux_ipc::{
        AttachFocusTarget, AttachRect, AttachScene, AttachSurface, AttachSurfaceKind,
        AttachViewComponent, PaneLayoutNode, PaneState, PaneSummary,
    };

    use crossterm::event::{
        Event as CrosstermEvent, KeyCode as CrosstermKeyCode, KeyEvent as CrosstermKeyEvent,
        KeyEventKind as CrosstermKeyEventKind, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    };
    use std::collections::{BTreeMap, BTreeSet};
    use uuid::Uuid;

    fn attach_view_state_with_scrollback_fixture() -> AttachViewState {
        let session_id = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let mut view_state = AttachViewState::new(AttachOpenInfo {
            context_id: None,
            session_id,
            can_write: true,
        });
        view_state.cached_layout_state = Some(AttachLayoutState {
            context_id: None,
            session_id,
            focused_pane_id: pane_id,
            panes: vec![PaneSummary {
                id: pane_id,
                index: 1,
                name: None,
                focused: true,
                state: PaneState::Running,
                state_reason: None,
            }],
            layout_root: PaneLayoutNode::Leaf { pane_id },
            scene: AttachScene {
                session_id,
                focus: AttachFocusTarget::Pane { pane_id },
                surfaces: vec![AttachSurface {
                    id: Uuid::new_v4(),
                    kind: AttachSurfaceKind::Pane,
                    layer: bmux_ipc::AttachLayer::Pane,
                    z: 0,
                    pane_id: Some(pane_id),
                    rect: AttachRect {
                        x: 0,
                        y: 0,
                        w: 9,
                        h: 6,
                    },
                    // Mirror the server-side scene contract: a 1-cell inset on each side.
                    // The PTY parser below is 4 rows x 20 cols (matching the historical
                    // `rect - 2` interior). Tests asserting cursor clamps to row 3 / col 2
                    // rely on `focused_attach_pane_inner_size` returning (7, 4).
                    content_rect: AttachRect {
                        x: 1,
                        y: 1,
                        w: 7,
                        h: 4,
                    },
                    interactive_regions: Vec::new(),
                    opaque: true,
                    visible: true,
                    accepts_input: true,
                    cursor_owner: true,
                }],
            },
            zoomed: false,
        });
        let buffer = view_state
            .pane_buffers
            .entry(pane_id)
            .or_insert_with(|| PaneRenderBuffer {
                parser: vt100::Parser::new(4, 20, 4_096),
                last_alternate_screen: false,
                prev_rows: Vec::new(),
                sync_update_in_progress: false,
                expected_stream_start: None,
            });
        append_pane_output(buffer, b"one\r\n  four\r\n     five\r\n  six\r\n\x1b[4;3H");
        view_state
    }

    #[test]
    fn apply_attach_output_marks_full_redraw_on_alt_screen_toggle() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        view_state.dirty.full_pane_redraw = false;
        view_state.force_cursor_move_next_frame = false;

        let pane_id = view_state
            .cached_layout_state
            .as_ref()
            .map(|layout| layout.focused_pane_id)
            .expect("focused pane id");

        let mut payload = Vec::new();
        payload.extend_from_slice(b"\x1b[?1049h");
        payload.extend(std::iter::repeat_n(
            b'x',
            super::ATTACH_OUTPUT_BATCH_MAX_BYTES * super::ATTACH_OUTPUT_DRAIN_MAX_ROUNDS + 64,
        ));
        payload.extend_from_slice(b"\x1b[?1049l\r\n$ ");

        let mut frame_needs_render = false;
        let had_data = super::apply_attach_output_bytes(
            &mut view_state,
            pane_id,
            &payload,
            &mut frame_needs_render,
        );

        assert!(had_data);
        assert!(frame_needs_render);
        assert!(view_state.dirty.pane_dirty_ids.contains(&pane_id));
        assert!(view_state.dirty.full_pane_redraw);
        assert!(view_state.force_cursor_move_next_frame);

        let buffer = view_state
            .pane_buffers
            .get(&pane_id)
            .expect("pane render buffer");
        assert!(!buffer.parser.screen().alternate_screen());
    }

    #[test]
    fn apply_attach_output_chunk_updates_continuity_state() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        let pane_id = view_state
            .cached_layout_state
            .as_ref()
            .map(|layout| layout.focused_pane_id)
            .expect("focused pane id");

        let mut frame_needs_render = false;
        let outcome = super::apply_attach_output_chunk(
            &mut view_state,
            pane_id,
            b"abc",
            super::AttachOutputChunkMeta {
                stream_start: 100,
                stream_end: 103,
                stream_gap: false,
                sync_update_active: true,
            },
            &mut frame_needs_render,
        );

        assert!(matches!(
            outcome,
            super::AttachChunkApplyOutcome::Applied { had_data: true }
        ));
        assert!(frame_needs_render);
        let buffer = view_state
            .pane_buffers
            .get(&pane_id)
            .expect("pane render buffer");
        assert_eq!(buffer.expected_stream_start, Some(103));
        assert!(buffer.sync_update_in_progress);
    }

    #[test]
    fn apply_attach_output_chunk_marks_gap_as_desync() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        let pane_id = view_state
            .cached_layout_state
            .as_ref()
            .map(|layout| layout.focused_pane_id)
            .expect("focused pane id");

        let mut frame_needs_render = false;
        let outcome = super::apply_attach_output_chunk(
            &mut view_state,
            pane_id,
            b"",
            super::AttachOutputChunkMeta {
                stream_start: 50,
                stream_end: 50,
                stream_gap: true,
                sync_update_active: false,
            },
            &mut frame_needs_render,
        );

        assert_eq!(outcome, super::AttachChunkApplyOutcome::Desync);
        assert!(!frame_needs_render);
    }

    #[test]
    fn apply_attach_output_chunk_ignores_stale_chunks() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        let pane_id = view_state
            .cached_layout_state
            .as_ref()
            .map(|layout| layout.focused_pane_id)
            .expect("focused pane id");
        view_state
            .pane_buffers
            .get_mut(&pane_id)
            .expect("pane render buffer")
            .expected_stream_start = Some(80);

        let mut frame_needs_render = false;
        let outcome = super::apply_attach_output_chunk(
            &mut view_state,
            pane_id,
            b"late",
            super::AttachOutputChunkMeta {
                stream_start: 70,
                stream_end: 80,
                stream_gap: false,
                sync_update_active: false,
            },
            &mut frame_needs_render,
        );

        assert_eq!(outcome, super::AttachChunkApplyOutcome::Stale);
        assert!(!frame_needs_render);
        let buffer = view_state
            .pane_buffers
            .get(&pane_id)
            .expect("pane render buffer");
        assert_eq!(buffer.expected_stream_start, Some(80));
    }

    #[test]
    fn apply_attach_output_chunk_detects_offset_mismatch() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        let pane_id = view_state
            .cached_layout_state
            .as_ref()
            .map(|layout| layout.focused_pane_id)
            .expect("focused pane id");
        view_state
            .pane_buffers
            .get_mut(&pane_id)
            .expect("pane render buffer")
            .expected_stream_start = Some(80);

        let mut frame_needs_render = false;
        let outcome = super::apply_attach_output_chunk(
            &mut view_state,
            pane_id,
            b"future",
            super::AttachOutputChunkMeta {
                stream_start: 81,
                stream_end: 87,
                stream_gap: false,
                sync_update_active: false,
            },
            &mut frame_needs_render,
        );

        assert_eq!(outcome, super::AttachChunkApplyOutcome::Desync);
        assert!(!frame_needs_render);
    }

    #[test]
    fn attach_view_change_components_mark_expected_dirty_flags() {
        let mut view_state = AttachViewState::new(AttachOpenInfo {
            context_id: None,
            session_id: uuid::Uuid::new_v4(),
            can_write: true,
        });
        view_state.dirty.status_needs_redraw = false;
        view_state.dirty.layout_needs_refresh = false;
        view_state.dirty.full_pane_redraw = false;

        apply_attach_view_change_components(&[AttachViewComponent::Status], &mut view_state);
        assert!(view_state.dirty.status_needs_redraw);
        assert!(!view_state.dirty.layout_needs_refresh);
        assert!(!view_state.dirty.full_pane_redraw);

        view_state.dirty.status_needs_redraw = false;
        apply_attach_view_change_components(&[AttachViewComponent::Layout], &mut view_state);
        assert!(view_state.dirty.status_needs_redraw);
        assert!(view_state.dirty.layout_needs_refresh);
        assert!(view_state.dirty.full_pane_redraw);

        view_state.dirty.status_needs_redraw = false;
        view_state.dirty.layout_needs_refresh = false;
        apply_attach_view_change_components(
            &[AttachViewComponent::Scene, AttachViewComponent::Layout],
            &mut view_state,
        );
        assert!(view_state.dirty.status_needs_redraw);
        assert!(view_state.dirty.layout_needs_refresh);
        assert!(view_state.dirty.full_pane_redraw);
    }

    #[test]
    fn attach_key_event_action_detaches_on_prefix_d() {
        let mut processor =
            InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()), false);
        let _ = attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('a'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            AttachUiMode::Normal,
        )
        .expect("attach key action should parse");

        let actions = attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('d'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], AttachEventAction::Detach));
    }

    #[test]
    fn attach_key_event_action_ctrl_d_forwards_to_pane() {
        let mut processor =
            InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()), false);
        let actions = attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('d'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert!(actions.is_empty());
    }

    // ── Repeat-event filtering ───────────────────────────────────────
    //
    // Quick tap of the `c` binding (which resolves to
    // `plugin:bmux.windows:new-window`) under kitty keyboard protocol
    // can surface as a Press plus one or more Repeat events. The
    // per-action `action_supports_repeat` classifier ensures the
    // mutating PluginCommand only fires on Press.

    #[test]
    fn action_supports_repeat_allows_navigation() {
        assert!(super::action_supports_repeat(&RuntimeAction::FocusNext));
        assert!(super::action_supports_repeat(&RuntimeAction::ResizeLeft));
        assert!(super::action_supports_repeat(&RuntimeAction::ScrollUpLine));
        assert!(super::action_supports_repeat(
            &RuntimeAction::ForwardToPane(b"x".to_vec())
        ));
    }

    #[test]
    fn action_supports_repeat_denies_mutating_actions() {
        assert!(!super::action_supports_repeat(
            &RuntimeAction::CloseFocusedPane
        ));
        assert!(!super::action_supports_repeat(&RuntimeAction::Quit));
        assert!(!super::action_supports_repeat(&RuntimeAction::Detach));
    }

    #[test]
    fn action_supports_repeat_denies_plugin_commands() {
        let action = RuntimeAction::PluginCommand {
            plugin_id: "bmux.windows".to_string(),
            command_name: "new-window".to_string(),
            args: vec![],
        };
        assert!(!super::action_supports_repeat(&action));
    }

    #[test]
    fn repeat_event_drops_plugin_command_action() {
        // Binding `c` resolves to plugin:bmux.windows:new-window. A
        // Repeat event for `c` must not emit a PluginCommand action.
        let mut processor =
            InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()), false);
        // First simulate a Press so the InputProcessor is primed.
        let press = attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('c'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            AttachUiMode::Normal,
        )
        .expect("press should parse");
        let press_has_plugin = press
            .iter()
            .any(|a| matches!(a, AttachEventAction::PluginCommand { .. }));
        assert!(press_has_plugin, "press should emit a PluginCommand");

        let repeat = attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('c'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Repeat,
            ),
            &mut processor,
            AttachUiMode::Normal,
        )
        .expect("repeat should parse");
        let repeat_has_plugin = repeat
            .iter()
            .any(|a| matches!(a, AttachEventAction::PluginCommand { .. }));
        assert!(!repeat_has_plugin, "repeat must not emit a PluginCommand");
    }

    #[test]
    fn repeat_event_preserves_navigation_action() {
        // Navigation-class runtime actions that survived the Repeat
        // filter must all satisfy `action_supports_repeat`. We don't
        // care about the specific chord here — just that the filter
        // never lets through a repeat-unsafe runtime action on Repeat.
        let mut processor =
            InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()), false);
        let repeat = attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Left,
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Repeat,
            ),
            &mut processor,
            AttachUiMode::Normal,
        )
        .expect("repeat should parse");
        for action in &repeat {
            assert!(
                !matches!(action, AttachEventAction::PluginCommand { .. }),
                "Repeat must never emit a PluginCommand",
            );
        }
    }

    #[test]
    fn attach_key_event_action_encodes_char_input() {
        let mut processor =
            InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()), false);
        let _ = attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('i'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        let actions = attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('x'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], AttachEventAction::Send(ref bytes) if bytes == b"x"));
    }

    #[test]
    fn attach_event_actions_maps_mouse_events() {
        let mut processor =
            InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()), false);
        let event = CrosstermEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 12,
            row: 8,
            modifiers: KeyModifiers::NONE,
        });

        let actions = attach_event_actions(&event, &mut processor, AttachUiMode::Normal)
            .expect("mouse event should map");

        assert!(matches!(
            actions.first(),
            Some(AttachEventAction::Mouse(mouse)) if mouse.column == 12 && mouse.row == 8
        ));
    }

    #[test]
    fn record_attach_mouse_event_tracks_position_and_timestamp() {
        let mut view_state = AttachViewState::new(AttachOpenInfo {
            context_id: None,
            session_id: Uuid::new_v4(),
            can_write: true,
        });
        let event = MouseEvent {
            kind: MouseEventKind::Moved,
            column: 3,
            row: 4,
            modifiers: KeyModifiers::NONE,
        };

        record_attach_mouse_event(event, &mut view_state);

        assert_eq!(view_state.mouse.last_position, Some((3, 4)));
        assert!(view_state.mouse.last_event_at.is_some());
    }

    #[test]
    fn encode_attach_mouse_sgr_encodes_button_down() {
        let encoded = encode_attach_mouse_sgr(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 4,
            modifiers: KeyModifiers::NONE,
        })
        .expect("mouse down should encode");

        assert_eq!(encoded, b"\x1b[<0;3;5M".to_vec());
    }

    #[test]
    fn encode_attach_mouse_sgr_encodes_release_with_modifier_bits() {
        let encoded = encode_attach_mouse_sgr(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Right),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::CONTROL | KeyModifiers::ALT,
        })
        .expect("mouse up should encode");

        assert_eq!(encoded, b"\x1b[<26;1;1m".to_vec());
    }

    #[test]
    fn encode_attach_mouse_sgr_encodes_scroll_and_move_events() {
        let scroll = encode_attach_mouse_sgr(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 10,
            row: 1,
            modifiers: KeyModifiers::SHIFT,
        })
        .expect("scroll should encode");
        let moved = encode_attach_mouse_sgr(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 10,
            row: 1,
            modifiers: KeyModifiers::SHIFT,
        })
        .expect("moved should encode");

        assert_eq!(scroll, b"\x1b[<69;11;2M".to_vec());
        assert_eq!(moved, b"\x1b[<39;11;2M".to_vec());
    }

    #[test]
    fn click_forwarding_policy_disables_click_forward_for_focus_only() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        view_state.mouse.config.click_propagation = MouseClickPropagation::FocusOnly;

        assert!(!should_forward_click_like_mouse(&view_state));
    }

    #[test]
    fn click_forwarding_policy_enables_click_forward_for_focus_and_forward() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        view_state.mouse.config.click_propagation = MouseClickPropagation::FocusAndForward;

        assert!(should_forward_click_like_mouse(&view_state));
    }

    #[test]
    fn wheel_policy_forward_and_scrollback_is_available() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        view_state.mouse.config.wheel_propagation = MouseWheelPropagation::ForwardAndScrollback;
        view_state.mouse.config.scroll_scrollback = true;

        assert_eq!(
            view_state.mouse.config.effective_wheel_propagation(),
            MouseWheelPropagation::ForwardAndScrollback
        );
    }

    #[test]
    fn attach_pane_mouse_protocol_reads_parser_state() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        let pane_id = focused_attach_pane_id(&view_state).expect("focused pane id");
        let buffer = view_state
            .pane_buffers
            .get_mut(&pane_id)
            .expect("pane render buffer");
        append_pane_output(buffer, b"\x1b[?1000h\x1b[?1006h");

        let protocol = attach_pane_mouse_protocol(&view_state, pane_id).expect("pane protocol");
        assert_eq!(protocol.mode, vt100::MouseProtocolMode::PressRelease);
        assert_eq!(protocol.encoding, vt100::MouseProtocolEncoding::Sgr);
    }

    #[test]
    fn attach_pane_mouse_protocol_uses_snapshot_hint_when_parser_mode_is_none() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        let pane_id = focused_attach_pane_id(&view_state).expect("focused pane id");

        view_state.pane_mouse_protocol_hints.insert(
            pane_id,
            bmux_ipc::AttachMouseProtocolState {
                mode: bmux_ipc::AttachMouseProtocolMode::AnyMotion,
                encoding: bmux_ipc::AttachMouseProtocolEncoding::Sgr,
            },
        );

        let protocol = attach_pane_mouse_protocol(&view_state, pane_id).expect("pane protocol");
        assert_eq!(protocol.mode, vt100::MouseProtocolMode::AnyMotion);
        assert_eq!(protocol.encoding, vt100::MouseProtocolEncoding::Sgr);
    }

    #[test]
    fn attach_pane_input_mode_reads_parser_state() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        let pane_id = focused_attach_pane_id(&view_state).expect("focused pane id");
        let buffer = view_state
            .pane_buffers
            .get_mut(&pane_id)
            .expect("pane render buffer");
        append_pane_output(buffer, b"\x1b[?1h\x1b=");

        let mode = attach_pane_input_mode(&view_state, pane_id).expect("pane mode");
        assert!(mode.application_cursor);
        assert!(mode.application_keypad);
    }

    #[test]
    fn attach_pane_input_mode_uses_snapshot_hint_when_parser_mode_is_default() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        let pane_id = focused_attach_pane_id(&view_state).expect("focused pane id");

        view_state.pane_input_mode_hints.insert(
            pane_id,
            bmux_ipc::AttachInputModeState {
                application_cursor: true,
                application_keypad: true,
            },
        );

        let mode = attach_pane_input_mode(&view_state, pane_id).expect("pane mode");
        assert!(mode.application_cursor);
        assert!(mode.application_keypad);
    }

    #[test]
    fn attach_layout_requires_snapshot_hydration_ignores_focus_only_scene_change() {
        let view_state = attach_view_state_with_scrollback_fixture();
        let previous = view_state.cached_layout_state.expect("layout state");
        let mut next = previous.clone();
        next.scene.surfaces[0].cursor_owner = false;

        assert_ne!(previous.scene, next.scene);
        assert!(!attach_layout_requires_snapshot_hydration(&previous, &next));
    }

    #[test]
    fn attach_scene_revealed_pane_ids_detects_zoom_focus_switch() {
        let view_state = attach_view_state_with_scrollback_fixture();
        let previous = view_state.cached_layout_state.expect("layout state");
        let previous_pane_id = previous.panes[0].id;
        let next_pane_id = Uuid::new_v4();
        let mut next = previous.clone();
        next.focused_pane_id = next_pane_id;
        next.scene.focus = AttachFocusTarget::Pane {
            pane_id: next_pane_id,
        };
        next.scene.surfaces[0].id = next_pane_id;
        next.scene.surfaces[0].pane_id = Some(next_pane_id);

        let revealed = attach_scene_revealed_pane_ids(&previous.scene, &next.scene);
        assert_eq!(revealed, BTreeSet::from([next_pane_id]));
        assert!(!revealed.contains(&previous_pane_id));
    }

    #[test]
    fn attach_scene_revealed_pane_ids_ignores_focus_metadata_only_changes() {
        let view_state = attach_view_state_with_scrollback_fixture();
        let previous = view_state.cached_layout_state.expect("layout state");
        let mut next = previous.clone();
        next.scene.surfaces[0].cursor_owner = false;

        let revealed = attach_scene_revealed_pane_ids(&previous.scene, &next.scene);
        assert!(revealed.is_empty());
    }

    #[test]
    fn attach_layout_requires_snapshot_hydration_on_layout_tree_change() {
        let view_state = attach_view_state_with_scrollback_fixture();
        let previous = view_state.cached_layout_state.expect("layout state");
        let existing_pane = previous.panes[0].id;
        let new_pane = Uuid::new_v4();
        let mut next = previous.clone();
        next.panes.push(PaneSummary {
            id: new_pane,
            index: 2,
            name: None,
            focused: false,
            state: PaneState::Running,
            state_reason: None,
        });
        next.layout_root = PaneLayoutNode::Split {
            direction: bmux_ipc::PaneSplitDirection::Vertical,
            ratio_percent: 50,
            first: Box::new(PaneLayoutNode::Leaf {
                pane_id: existing_pane,
            }),
            second: Box::new(PaneLayoutNode::Leaf { pane_id: new_pane }),
        };

        assert!(attach_layout_requires_snapshot_hydration(&previous, &next));
    }

    #[test]
    fn encode_attach_mouse_for_protocol_skips_when_mode_is_disabled() {
        let encoded = encode_attach_mouse_for_protocol(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            },
            AttachPaneMouseProtocol {
                mode: vt100::MouseProtocolMode::None,
                encoding: vt100::MouseProtocolEncoding::Sgr,
            },
        );
        assert!(encoded.is_none());
    }

    #[test]
    fn mouse_protocol_mode_reports_event_rejects_move_without_any_motion_mode() {
        assert!(!mouse_protocol_mode_reports_event(
            vt100::MouseProtocolMode::PressRelease,
            MouseEventKind::Moved,
        ));
        assert!(mouse_protocol_mode_reports_event(
            vt100::MouseProtocolMode::AnyMotion,
            MouseEventKind::Moved,
        ));
    }

    #[test]
    fn mouse_protocol_mode_reports_event_rejects_release_in_press_mode() {
        assert!(!mouse_protocol_mode_reports_event(
            vt100::MouseProtocolMode::Press,
            MouseEventKind::Up(MouseButton::Left),
        ));
        assert!(mouse_protocol_mode_reports_event(
            vt100::MouseProtocolMode::Press,
            MouseEventKind::Down(MouseButton::Left),
        ));
    }

    #[test]
    fn encode_attach_mouse_default_uses_csi_m_sequence() {
        let encoded = encode_attach_mouse_for_protocol(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            },
            AttachPaneMouseProtocol {
                mode: vt100::MouseProtocolMode::PressRelease,
                encoding: vt100::MouseProtocolEncoding::Default,
            },
        )
        .expect("default-encoded mouse event");

        assert_eq!(encoded, vec![0x1b, b'[', b'M', 32, 33, 33]);
    }

    #[test]
    fn encode_attach_mouse_default_rejects_wide_coordinates() {
        let encoded = encode_attach_mouse_for_protocol(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 223,
                row: 0,
                modifiers: KeyModifiers::NONE,
            },
            AttachPaneMouseProtocol {
                mode: vt100::MouseProtocolMode::PressRelease,
                encoding: vt100::MouseProtocolEncoding::Default,
            },
        );

        assert!(encoded.is_none());
    }

    #[test]
    fn encode_attach_mouse_utf8_supports_wide_coordinates() {
        let encoded = encode_attach_mouse_for_protocol(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 223,
                row: 0,
                modifiers: KeyModifiers::NONE,
            },
            AttachPaneMouseProtocol {
                mode: vt100::MouseProtocolMode::PressRelease,
                encoding: vt100::MouseProtocolEncoding::Utf8,
            },
        )
        .expect("utf8-encoded mouse event");

        assert_eq!(encoded, vec![0x1b, b'[', b'M', 32, 0xC4, 0x80, 33]);
    }

    #[test]
    fn attach_loop_mouse_moved_without_pane_mouse_mode_does_not_forward_bytes() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        let mut processor =
            InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()), false);
        let event = CrosstermEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 2,
            row: 2,
            modifiers: KeyModifiers::NONE,
        });

        let actions = attach_event_actions(&event, &mut processor, AttachUiMode::Normal)
            .expect("mouse event should map through attach loop");
        let mouse_event = match actions.as_slice() {
            [AttachEventAction::Mouse(mouse)] => *mouse,
            _ => panic!("unexpected attach actions for mouse event"),
        };

        let target_pane = attach_scene_pane_at(&view_state, mouse_event.column, mouse_event.row);
        let focused_pane = view_state
            .cached_layout_state
            .as_ref()
            .map(|layout| layout.focused_pane_id);
        let in_focused_pane = target_pane.is_some() && target_pane == focused_pane;

        let forwarded = attach_mouse_forward_bytes_for_target(
            &view_state,
            mouse_event,
            target_pane,
            in_focused_pane,
        );
        assert!(
            forwarded.is_none(),
            "mouse move should not forward when pane mouse mode is disabled"
        );

        let pane_id = focused_attach_pane_id(&view_state).expect("focused pane id");
        let buffer = view_state
            .pane_buffers
            .get_mut(&pane_id)
            .expect("pane render buffer");
        append_pane_output(buffer, b"\x1b[?1003h\x1b[?1006h");

        let forwarded = attach_mouse_forward_bytes_for_target(
            &view_state,
            mouse_event,
            target_pane,
            in_focused_pane,
        )
        .expect("mouse move should forward once pane enables any-motion mode");
        // Fixture has outer rect (0,0,9,6) with content_rect inset by 1 on each side,
        // so column=2 row=2 (absolute) translates to pane-local (1,1) → SGR (2,2).
        assert_eq!(forwarded, b"\x1b[<35;2;2M".to_vec());
    }

    #[test]
    fn attach_mouse_forward_translates_coordinates_to_pane_local() {
        // Regression for "clicks land at end of line": a pane rendered in
        // the top-right of the attach UI has a non-zero origin. Clicks
        // must be translated into that pane's own coordinate space before
        // being forwarded; otherwise the program inside the pane receives
        // a column far past its own width and clamps the cursor to EOL.
        let session_id = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let mut view_state = AttachViewState::new(AttachOpenInfo {
            context_id: None,
            session_id,
            can_write: true,
        });
        let rect = AttachRect {
            x: 91,
            y: 1,
            w: 90,
            h: 40,
        };
        view_state.cached_layout_state = Some(AttachLayoutState {
            context_id: None,
            session_id,
            focused_pane_id: pane_id,
            panes: vec![PaneSummary {
                id: pane_id,
                index: 1,
                name: None,
                focused: true,
                state: PaneState::Running,
                state_reason: None,
            }],
            layout_root: PaneLayoutNode::Leaf { pane_id },
            scene: AttachScene {
                session_id,
                focus: AttachFocusTarget::Pane { pane_id },
                surfaces: vec![AttachSurface {
                    id: Uuid::new_v4(),
                    kind: AttachSurfaceKind::Pane,
                    layer: bmux_ipc::AttachLayer::Pane,
                    z: 0,
                    pane_id: Some(pane_id),
                    rect,
                    content_rect: rect,
                    interactive_regions: Vec::new(),
                    opaque: true,
                    visible: true,
                    accepts_input: true,
                    cursor_owner: true,
                }],
            },
            zoomed: false,
        });
        let buffer = view_state
            .pane_buffers
            .entry(pane_id)
            .or_insert_with(|| PaneRenderBuffer {
                parser: vt100::Parser::new(40, 90, 4_096),
                last_alternate_screen: false,
                prev_rows: Vec::new(),
                sync_update_in_progress: false,
                expected_stream_start: None,
            });
        // Enable SGR + press/release so the pane protocol reports clicks.
        append_pane_output(buffer, b"\x1b[?1000h\x1b[?1006h");

        // Click at the pane's first visible cell (absolute 91, 1) should
        // emit pane-local (1, 1), not absolute (92, 2).
        let first_cell = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 91,
            row: 1,
            modifiers: KeyModifiers::NONE,
        };
        let forwarded =
            attach_mouse_forward_bytes_for_target(&view_state, first_cell, Some(pane_id), true)
                .expect("forward click at pane origin");
        assert_eq!(forwarded, b"\x1b[<0;1;1M".to_vec());

        // Click further into the pane: (100, 5) → local (9, 4) → encoded (10, 5).
        let middle = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 100,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        let forwarded =
            attach_mouse_forward_bytes_for_target(&view_state, middle, Some(pane_id), true)
                .expect("forward click inside pane");
        assert_eq!(forwarded, b"\x1b[<0;10;5M".to_vec());

        // Click outside the pane rect should not forward (belt-and-suspenders;
        // upstream callers are already expected to filter by pane_at).
        let outside = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 1,
            modifiers: KeyModifiers::NONE,
        };
        let forwarded =
            attach_mouse_forward_bytes_for_target(&view_state, outside, Some(pane_id), true);
        assert!(
            forwarded.is_none(),
            "clicks outside the pane rect must not be forwarded"
        );
    }

    /// Regression test for the "one down, one right" border off-by-one bug.
    ///
    /// When a pane has a 1-cell decoration/border, the surface's `rect`
    /// covers the outer bounds but the PTY (and nvim, tmux, etc. running
    /// inside it) only sees the interior `content_rect` which starts at
    /// `rect.x + 1, rect.y + 1`. A click at the visual top-left content
    /// cell must encode to pane-local `(1, 1)` — SGR `\x1b[<0;1;1M`. If
    /// the translator uses the outer `rect` instead of `content_rect`, the
    /// click appears one column right and one row down to the program
    /// inside the pane.
    #[test]
    fn attach_mouse_forward_uses_content_rect_not_outer_rect() {
        let session_id = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let mut view_state = AttachViewState::new(AttachOpenInfo {
            context_id: None,
            session_id,
            can_write: true,
        });
        // Outer rect of the pane surface; content rect is inset by 1 on
        // each side — matching what the server scene producer now emits.
        let outer = AttachRect {
            x: 91,
            y: 1,
            w: 90,
            h: 40,
        };
        let content = AttachRect {
            x: outer.x + 1,
            y: outer.y + 1,
            w: outer.w - 2,
            h: outer.h - 2,
        };
        view_state.cached_layout_state = Some(AttachLayoutState {
            context_id: None,
            session_id,
            focused_pane_id: pane_id,
            panes: vec![PaneSummary {
                id: pane_id,
                index: 1,
                name: None,
                focused: true,
                state: PaneState::Running,
                state_reason: None,
            }],
            layout_root: PaneLayoutNode::Leaf { pane_id },
            scene: AttachScene {
                session_id,
                focus: AttachFocusTarget::Pane { pane_id },
                surfaces: vec![AttachSurface {
                    id: Uuid::new_v4(),
                    kind: AttachSurfaceKind::Pane,
                    layer: bmux_ipc::AttachLayer::Pane,
                    z: 0,
                    pane_id: Some(pane_id),
                    rect: outer,
                    content_rect: content,
                    interactive_regions: Vec::new(),
                    opaque: true,
                    visible: true,
                    accepts_input: true,
                    cursor_owner: true,
                }],
            },
            zoomed: false,
        });
        let buffer = view_state
            .pane_buffers
            .entry(pane_id)
            .or_insert_with(|| PaneRenderBuffer {
                parser: vt100::Parser::new(38, 88, 4_096),
                last_alternate_screen: false,
                prev_rows: Vec::new(),
                sync_update_in_progress: false,
                expected_stream_start: None,
            });
        append_pane_output(buffer, b"\x1b[?1000h\x1b[?1006h");

        // Click at the visual top-left content cell: absolute
        // (content.x, content.y) = (92, 2). Pane-local = (0, 0) →
        // encoded SGR = (1, 1).
        let first_content_cell = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: content.x,
            row: content.y,
            modifiers: KeyModifiers::NONE,
        };
        let forwarded = attach_mouse_forward_bytes_for_target(
            &view_state,
            first_content_cell,
            Some(pane_id),
            true,
        )
        .expect("click on the first content cell should forward");
        assert_eq!(
            forwarded,
            b"\x1b[<0;1;1M".to_vec(),
            "click at the visual top-left content cell must encode as SGR (1, 1)"
        );

        // Click on the top border cell (outer.y, outside content_rect):
        // should not forward PTY bytes because the click is on decoration,
        // not content.
        let border_click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: outer.x + 5,
            row: outer.y,
            modifiers: KeyModifiers::NONE,
        };
        let forwarded =
            attach_mouse_forward_bytes_for_target(&view_state, border_click, Some(pane_id), true);
        assert!(
            forwarded.is_none(),
            "clicks on the border (outside content_rect) must not forward PTY bytes"
        );
    }

    /// A render extension can publish a tighter `content_rect` than
    /// the scene producer (e.g., a plugin that paints a 2-cell
    /// border). When the extension returns a non-zero override, the
    /// mouse translator must prefer it over the scene producer's
    /// value so clicks on the first visual content cell under that
    /// thicker border still encode to SGR `(1, 1)`.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn attach_mouse_forward_honors_render_extension_content_rect() {
        use bmux_plugin::{AttachRenderExtension, ExtensionRect};
        use std::io;
        use std::sync::Arc;

        // Test-only render extension that reports a fixed
        // `content_rect` override for a known surface id.
        struct FixedInsetExtension {
            surface_id: Uuid,
            override_rect: ExtensionRect,
        }

        impl AttachRenderExtension for FixedInsetExtension {
            #[allow(clippy::unnecessary_literal_bound)]
            fn name(&self) -> &str {
                "test.fixed_inset"
            }

            fn apply_surface(
                &self,
                _stdout: &mut dyn io::Write,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
            ) -> io::Result<bool> {
                Ok(false)
            }

            fn content_rect_override(&self, surface_id: Uuid) -> Option<ExtensionRect> {
                if surface_id == self.surface_id {
                    Some(self.override_rect)
                } else {
                    None
                }
            }
        }

        let session_id = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let surface_id = Uuid::new_v4();
        let mut view_state = AttachViewState::new(AttachOpenInfo {
            context_id: None,
            session_id,
            can_write: true,
        });
        // Scene producer publishes a 1-cell inset.
        let outer = AttachRect {
            x: 0,
            y: 0,
            w: 40,
            h: 10,
        };
        let scene_content = AttachRect {
            x: 1,
            y: 1,
            w: 38,
            h: 8,
        };
        view_state.cached_layout_state = Some(AttachLayoutState {
            context_id: None,
            session_id,
            focused_pane_id: pane_id,
            panes: vec![PaneSummary {
                id: pane_id,
                index: 1,
                name: None,
                focused: true,
                state: PaneState::Running,
                state_reason: None,
            }],
            layout_root: PaneLayoutNode::Leaf { pane_id },
            scene: AttachScene {
                session_id,
                focus: AttachFocusTarget::Pane { pane_id },
                surfaces: vec![AttachSurface {
                    id: surface_id,
                    kind: AttachSurfaceKind::Pane,
                    layer: bmux_ipc::AttachLayer::Pane,
                    z: 0,
                    pane_id: Some(pane_id),
                    rect: outer,
                    content_rect: scene_content,
                    interactive_regions: Vec::new(),
                    opaque: true,
                    visible: true,
                    accepts_input: true,
                    cursor_owner: true,
                }],
            },
            zoomed: false,
        });

        // Register a render extension that overrides `content_rect`
        // to a tighter 2-cell inset.
        let plugin_content = ExtensionRect {
            x: 2,
            y: 2,
            w: 36,
            h: 6,
        };
        bmux_plugin::register_render_extension(Arc::new(FixedInsetExtension {
            surface_id,
            override_rect: plugin_content,
        }));

        let buffer = view_state
            .pane_buffers
            .entry(pane_id)
            .or_insert_with(|| PaneRenderBuffer {
                parser: vt100::Parser::new(6, 36, 4_096),
                last_alternate_screen: false,
                prev_rows: Vec::new(),
                sync_update_in_progress: false,
                expected_stream_start: None,
            });
        append_pane_output(buffer, b"\x1b[?1000h\x1b[?1006h");

        // Click on absolute (plugin_content.x, plugin_content.y) — the
        // first visible content cell when the extension's 2-cell
        // border is painted. Should encode to pane-local (0, 0) →
        // SGR (1, 1).
        let first_cell = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: plugin_content.x,
            row: plugin_content.y,
            modifiers: KeyModifiers::NONE,
        };
        let forwarded =
            attach_mouse_forward_bytes_for_target(&view_state, first_cell, Some(pane_id), true)
                .expect("click on extension's first content cell should forward");
        assert_eq!(
            forwarded,
            b"\x1b[<0;1;1M".to_vec(),
            "extension's content_rect_override must take precedence over the scene producer's"
        );

        // Click at (scene_content.x, scene_content.y) — the scene
        // producer's first content cell, but UNDER the extension's
        // 2-cell border. Should NOT forward bytes.
        let scene_cell = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: scene_content.x,
            row: scene_content.y,
            modifiers: KeyModifiers::NONE,
        };
        let forwarded =
            attach_mouse_forward_bytes_for_target(&view_state, scene_cell, Some(pane_id), true);
        assert!(
            forwarded.is_none(),
            "clicks under the extension's wider border must not forward bytes"
        );
    }

    #[test]
    fn resolve_mouse_gesture_action_parses_plugin_command() {
        let mut view_state = AttachViewState::new(AttachOpenInfo {
            context_id: None,
            session_id: Uuid::new_v4(),
            can_write: true,
        });
        view_state.mouse.config.gesture_actions.insert(
            "click_left".to_string(),
            "plugin:bmux.windows:new-window".to_string(),
        );

        let resolved = resolve_mouse_gesture_action(&view_state, "click_left");
        assert!(matches!(
            resolved,
            Some(AttachEventAction::PluginCommand {
                plugin_id,
                command_name,
                args,
            }) if plugin_id == "bmux.windows" && command_name == "new-window" && args.is_empty()
        ));
    }

    #[test]
    fn attach_scene_pane_at_prefers_topmost_surface() {
        let session_id = Uuid::new_v4();
        let background_pane = Uuid::new_v4();
        let floating_pane = Uuid::new_v4();
        let mut view_state = AttachViewState::new(AttachOpenInfo {
            context_id: None,
            session_id,
            can_write: true,
        });
        view_state.cached_layout_state = Some(AttachLayoutState {
            context_id: None,
            session_id,
            focused_pane_id: background_pane,
            panes: Vec::new(),
            layout_root: PaneLayoutNode::Leaf {
                pane_id: background_pane,
            },
            scene: AttachScene {
                session_id,
                focus: AttachFocusTarget::Pane {
                    pane_id: background_pane,
                },
                surfaces: vec![
                    AttachSurface {
                        id: Uuid::new_v4(),
                        kind: AttachSurfaceKind::Pane,
                        layer: bmux_ipc::AttachLayer::Pane,
                        z: 1,
                        rect: AttachRect {
                            x: 0,
                            y: 0,
                            w: 20,
                            h: 10,
                        },
                        content_rect: AttachRect {
                            x: 0,
                            y: 0,
                            w: 20,
                            h: 10,
                        },
                        interactive_regions: Vec::new(),
                        opaque: true,
                        visible: true,
                        accepts_input: true,
                        cursor_owner: true,
                        pane_id: Some(background_pane),
                    },
                    AttachSurface {
                        id: Uuid::new_v4(),
                        kind: AttachSurfaceKind::FloatingPane,
                        layer: bmux_ipc::AttachLayer::FloatingPane,
                        z: 10,
                        rect: AttachRect {
                            x: 2,
                            y: 2,
                            w: 8,
                            h: 5,
                        },
                        content_rect: AttachRect {
                            x: 2,
                            y: 2,
                            w: 8,
                            h: 5,
                        },
                        interactive_regions: Vec::new(),
                        opaque: true,
                        visible: true,
                        accepts_input: true,
                        cursor_owner: false,
                        pane_id: Some(floating_pane),
                    },
                ],
            },
            zoomed: false,
        });

        assert_eq!(attach_scene_pane_at(&view_state, 4, 4), Some(floating_pane));
        assert_eq!(
            attach_scene_pane_at(&view_state, 1, 1),
            Some(background_pane)
        );
        assert_eq!(attach_scene_pane_at(&view_state, 30, 30), None);
    }

    #[test]
    #[cfg(feature = "bundled-plugin-windows")]
    #[allow(clippy::too_many_lines)]
    fn attach_key_event_action_maps_prefixed_runtime_defaults() {
        let mut processor =
            InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()), false);

        let new_window = attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('c'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert!(matches!(
            new_window.first(),
            Some(AttachEventAction::PluginCommand { plugin_id, command_name, args })
                if plugin_id == "bmux.windows" && command_name == "new-window" && args.is_empty()
        ));

        let next_window = attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('s'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert!(matches!(
            next_window.first(),
            Some(AttachEventAction::PluginCommand { plugin_id, command_name, args })
                if plugin_id == "bmux.windows" && command_name == "next-window" && args.is_empty()
        ));

        let previous_window = attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('h'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert!(matches!(
            previous_window.first(),
            Some(AttachEventAction::PluginCommand { plugin_id, command_name, args })
                if plugin_id == "bmux.windows" && command_name == "prev-window" && args.is_empty()
        ));

        let last_window = attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('l'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert!(matches!(
            last_window.first(),
            Some(AttachEventAction::PluginCommand { plugin_id, command_name, args })
                if plugin_id == "bmux.windows" && command_name == "last-window" && args.is_empty()
        ));

        let split_vertical = attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('%'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert!(matches!(
            split_vertical.first(),
            Some(AttachEventAction::Ui(
                crate::input::RuntimeAction::SplitFocusedVertical
            ))
        ));

        let quit = attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('q'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert!(matches!(
            quit.first(),
            Some(AttachEventAction::Ui(crate::input::RuntimeAction::Quit))
        ));

        let new_session = attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('C'),
                KeyModifiers::SHIFT,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert!(matches!(
            new_session.first(),
            Some(AttachEventAction::PluginCommand {
                plugin_id,
                command_name,
                ..
            }) if plugin_id == "bmux.sessions" && command_name == "new-session"
        ));
    }

    #[test]
    fn attach_key_event_action_routes_ctrl_t_as_focus_prev_pane_by_default() {
        let mut processor =
            InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()), false);
        let actions = attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('t'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            AttachUiMode::Normal,
        )
        .expect("attach key action should parse");

        assert!(matches!(
            actions.first(),
            Some(AttachEventAction::Ui(
                crate::input::RuntimeAction::FocusPrev
            ))
        ));
    }

    #[test]
    fn attach_key_event_action_routes_h_to_focus_left_in_normal_mode() {
        let mut processor =
            InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()), false);

        let normal_actions = attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('h'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert!(matches!(
            normal_actions.first(),
            Some(AttachEventAction::Ui(
                crate::input::RuntimeAction::FocusLeft
            ))
        ));

        let _ = processor;
    }

    #[test]
    fn global_plugin_command_with_args_maps_to_plugin_action() {
        let mut config = BmuxConfig::default();
        config.keybindings.global.insert(
            "alt+1".to_string(),
            "plugin:bmux.windows:goto-window 1".to_string(),
        );
        let mut processor = InputProcessor::new(attach_keymap_from_config(&config), false);

        let actions = attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('1'),
                KeyModifiers::ALT,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert!(
            matches!(
                actions.first(),
                Some(AttachEventAction::PluginCommand {
                    plugin_id,
                    command_name,
                    args,
                }) if plugin_id == "bmux.windows"
                    && command_name == "goto-window"
                    && args == &["1".to_string()]
            ),
            "global alt+1 should map to PluginCommand with args"
        );
    }

    #[test]
    fn attach_key_event_action_routes_enter_scroll_mode_to_ui() {
        let mut processor =
            InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()), false);

        let _ = attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('a'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        let actions = attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('['),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            AttachUiMode::Normal,
        )
        .expect("attach key action should parse");

        assert!(matches!(
            actions.first(),
            Some(AttachEventAction::Ui(
                crate::input::RuntimeAction::EnterScrollMode
            ))
        ));
    }

    #[test]
    fn attach_key_event_action_routes_alt_h_as_focus_left() {
        let mut processor =
            InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()), false);

        let actions = attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('h'),
                KeyModifiers::ALT,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert!(matches!(
            actions.first(),
            Some(AttachEventAction::Ui(
                crate::input::RuntimeAction::FocusLeft
            ))
        ));
    }

    #[test]
    fn attach_key_event_action_routes_n_to_pane_in_normal_mode() {
        let mut processor =
            InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()), false);

        let normal_actions = attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('n'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            AttachUiMode::Normal,
        )
        .expect("attach key action should parse");
        assert!(normal_actions.is_empty());
    }

    #[test]
    fn attach_keybindings_allow_global_override_of_default_session_key() {
        let mut config = BmuxConfig::default();
        config.keybindings.global.insert(
            "ctrl+t".to_string(),
            "plugin:bmux.sessions:new-session".to_string(),
        );

        let mut processor = InputProcessor::new(attach_keymap_from_config(&config), false);
        let actions = attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('t'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            AttachUiMode::Normal,
        )
        .expect("attach key action should parse");

        assert!(matches!(
            actions.first(),
            Some(AttachEventAction::PluginCommand {
                plugin_id,
                command_name,
                ..
            }) if plugin_id == "bmux.sessions" && command_name == "new-session"
        ));
    }

    #[test]
    fn attach_mode_hint_reflects_remapped_normal_mode_keys() {
        let mut config = BmuxConfig::default();
        config
            .keybindings
            .modes
            .get_mut("normal")
            .expect("normal mode")
            .bindings
            .insert("z".to_string(), "detach".to_string());
        config
            .keybindings
            .modes
            .get_mut("normal")
            .expect("normal mode")
            .bindings
            .insert("d".to_string(), "quit".to_string());

        let keymap = attach_keymap_from_config(&config);
        let hint = attach_mode_hint("normal", AttachUiMode::Normal, &keymap);
        assert!(hint.contains("z detach"));
        assert!(hint.contains("d quit"));
    }

    #[test]
    fn attach_mode_hint_includes_session_navigation_overrides() {
        let mut config = BmuxConfig::default();
        config
            .keybindings
            .global
            .insert("alt+h".to_string(), "new_session".to_string());
        config
            .keybindings
            .global
            .insert("alt+l".to_string(), "detach".to_string());
        config
            .keybindings
            .global
            .insert("q".to_string(), "quit".to_string());

        let keymap = attach_keymap_from_config(&config);
        let hint = attach_mode_hint("normal", AttachUiMode::Normal, &keymap);
        assert!(hint.contains("Ctrl-A d quit") || hint.contains("q quit"));
        assert!(hint.contains("detach"));
    }

    #[test]
    fn adjust_attach_scrollback_offset_clamps_within_bounds() {
        assert_eq!(adjust_attach_scrollback_offset(0, -1, 4), 1);
        assert_eq!(adjust_attach_scrollback_offset(3, -10, 4), 4);
        assert_eq!(adjust_attach_scrollback_offset(4, 1, 4), 3);
        assert_eq!(adjust_attach_scrollback_offset(1, 50, 4), 0);
    }

    #[test]
    fn adjust_scrollback_cursor_component_clamps_within_bounds() {
        assert_eq!(adjust_scrollback_cursor_component(0, -1, 5), 0);
        assert_eq!(adjust_scrollback_cursor_component(2, -1, 5), 1);
        assert_eq!(adjust_scrollback_cursor_component(2, 10, 5), 5);
    }

    #[test]
    fn enter_attach_scrollback_initializes_cursor_from_live_position() {
        let mut view_state = attach_view_state_with_scrollback_fixture();

        assert!(enter_attach_scrollback(&mut view_state));
        assert!(view_state.scrollback_active);
        assert_eq!(view_state.scrollback_offset, 0);
        assert_eq!(
            view_state.scrollback_cursor,
            Some(AttachScrollbackCursor { row: 3, col: 2 })
        );
    }

    #[test]
    fn move_attach_scrollback_cursor_vertical_scrolls_at_viewport_edges() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        assert!(enter_attach_scrollback(&mut view_state));

        move_attach_scrollback_cursor_vertical(&mut view_state, -1);
        assert_eq!(
            view_state.scrollback_cursor,
            Some(AttachScrollbackCursor { row: 2, col: 2 })
        );
        assert_eq!(view_state.scrollback_offset, 0);

        move_attach_scrollback_cursor_vertical(&mut view_state, -3);
        assert_eq!(
            view_state.scrollback_cursor,
            Some(AttachScrollbackCursor { row: 0, col: 2 })
        );
        assert_eq!(view_state.scrollback_offset, 1);

        move_attach_scrollback_cursor_vertical(&mut view_state, 1);
        assert_eq!(
            view_state.scrollback_cursor,
            Some(AttachScrollbackCursor { row: 1, col: 2 })
        );
        assert_eq!(view_state.scrollback_offset, 1);
    }

    #[test]
    fn move_attach_scrollback_cursor_horizontal_updates_column() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        assert!(enter_attach_scrollback(&mut view_state));

        move_attach_scrollback_cursor_horizontal(&mut view_state, 3);
        assert_eq!(
            view_state.scrollback_cursor,
            Some(AttachScrollbackCursor { row: 3, col: 5 })
        );

        move_attach_scrollback_cursor_horizontal(&mut view_state, -10);
        assert_eq!(
            view_state.scrollback_cursor,
            Some(AttachScrollbackCursor { row: 3, col: 0 })
        );
    }

    #[test]
    fn begin_attach_selection_uses_absolute_cursor_position() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        assert!(enter_attach_scrollback(&mut view_state));
        view_state.scrollback_offset = 2;

        assert!(begin_attach_selection(&mut view_state));
        assert_eq!(
            view_state.selection_anchor,
            Some(AttachScrollbackPosition { row: 5, col: 2 })
        );
    }

    #[test]
    fn clear_attach_selection_removes_anchor() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        assert!(enter_attach_scrollback(&mut view_state));
        assert!(begin_attach_selection(&mut view_state));

        clear_attach_selection(&mut view_state, false);
        assert_eq!(view_state.selection_anchor, None);
    }

    #[test]
    fn selected_attach_text_extracts_multiline_range() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        assert!(enter_attach_scrollback(&mut view_state));
        view_state.selection_anchor = Some(AttachScrollbackPosition { row: 2, col: 2 });
        view_state.scrollback_cursor = Some(AttachScrollbackCursor { row: 3, col: 8 });
        view_state.scrollback_offset = 0;

        assert_eq!(
            selected_attach_text(&mut view_state),
            Some("e\n  four".to_string())
        );
    }

    #[test]
    fn confirm_attach_scrollback_exits_when_no_selection() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        assert!(enter_attach_scrollback(&mut view_state));

        confirm_attach_scrollback(&mut view_state);
        assert!(!view_state.scrollback_active);
    }

    #[test]
    fn mouse_scroll_up_enters_scrollback_and_steps_by_configured_lines() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        view_state.mouse.config.scroll_lines_per_tick = 1;
        view_state.mouse.config.scroll_scrollback = true;

        assert!(handle_attach_mouse_scrollback(
            &mut view_state,
            MouseEventKind::ScrollUp,
        ));
        assert!(view_state.scrollback_active);
        assert_eq!(view_state.scrollback_offset, 1);
    }

    #[test]
    fn mouse_scroll_down_exits_scrollback_at_bottom_when_enabled() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        view_state.mouse.config.scroll_lines_per_tick = 1;
        view_state.mouse.config.scroll_scrollback = true;
        view_state.mouse.config.exit_scrollback_on_bottom = true;
        assert!(enter_attach_scrollback(&mut view_state));
        view_state.scrollback_offset = 1;

        assert!(handle_attach_mouse_scrollback(
            &mut view_state,
            MouseEventKind::ScrollDown,
        ));
        assert!(!view_state.scrollback_active);
        assert_eq!(view_state.scrollback_offset, 0);
    }

    #[test]
    fn focused_attach_pane_inner_size_reads_content_rect_not_outer_rect() {
        // Regression guard: `focused_attach_pane_inner_size` MUST read the scene's
        // authoritative `content_rect`, not recompute `rect - 2` locally. If someone
        // "fixes" this back to subtracting a fixed inset from `rect`, this test fails.
        //
        // The fixture uses an asymmetric inset (outer 20x10 with content 15x4 at offset
        // 2,3) so a `rect - 2` regression would return (18, 8) — clearly wrong — instead
        // of (15, 4).
        let session_id = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let mut view_state = AttachViewState::new(AttachOpenInfo {
            context_id: None,
            session_id,
            can_write: true,
        });
        view_state.cached_layout_state = Some(AttachLayoutState {
            context_id: None,
            session_id,
            focused_pane_id: pane_id,
            panes: vec![PaneSummary {
                id: pane_id,
                index: 1,
                name: None,
                focused: true,
                state: PaneState::Running,
                state_reason: None,
            }],
            layout_root: PaneLayoutNode::Leaf { pane_id },
            scene: AttachScene {
                session_id,
                focus: AttachFocusTarget::Pane { pane_id },
                surfaces: vec![AttachSurface {
                    id: Uuid::new_v4(),
                    kind: AttachSurfaceKind::Pane,
                    layer: bmux_ipc::AttachLayer::Pane,
                    z: 0,
                    pane_id: Some(pane_id),
                    rect: AttachRect {
                        x: 0,
                        y: 0,
                        w: 20,
                        h: 10,
                    },
                    content_rect: AttachRect {
                        x: 2,
                        y: 3,
                        w: 15,
                        h: 4,
                    },
                    interactive_regions: Vec::new(),
                    opaque: true,
                    visible: true,
                    accepts_input: true,
                    cursor_owner: true,
                }],
            },
            zoomed: false,
        });

        assert_eq!(
            focused_attach_pane_inner_size(&view_state),
            Some((15, 4)),
            "inner size must equal the scene's content_rect dims, not rect - 2"
        );
    }

    #[test]
    fn attach_scrollback_hint_uses_default_bindings() {
        let keymap = attach_keymap_from_config(&BmuxConfig::default());
        let hint = attach_scrollback_hint(&keymap);

        assert!(hint.contains("select"));
        assert!(hint.contains("copy"));
        assert!(hint.contains("page"));
        assert!(hint.contains("top/bottom"));
        assert!(hint.contains("exit scroll"));
    }

    #[test]
    fn attach_keybindings_keep_focus_next_pane_binding() {
        let (runtime, _global, _scroll) = filtered_attach_keybindings(&BmuxConfig::default());
        assert_eq!(runtime.get("o"), Some(&"focus_next_pane".to_string()));
    }

    #[test]
    fn attach_key_event_action_maps_show_help_to_ui() {
        let config = BmuxConfig::default();
        let keymap = attach_keymap_from_config(&config);
        let mut processor = InputProcessor::new(keymap, false);

        let _ = attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('a'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            AttachUiMode::Normal,
        )
        .expect("attach key action should parse");

        let help_question = attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('?'),
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            AttachUiMode::Normal,
        )
        .expect("attach key action should parse");

        let _ = attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('a'),
                KeyModifiers::CONTROL,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            AttachUiMode::Normal,
        )
        .expect("attach key action should parse");

        let help_shift_slash = attach_key_event_actions(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Char('/'),
                KeyModifiers::SHIFT,
                CrosstermKeyEventKind::Press,
            ),
            &mut processor,
            AttachUiMode::Normal,
        )
        .expect("attach key action should parse");

        assert!(matches!(
            help_question.first().or_else(|| help_shift_slash.first()),
            Some(AttachEventAction::Ui(crate::input::RuntimeAction::ShowHelp))
        ));
    }

    #[test]
    fn effective_attach_keybindings_include_scope_and_canonical_action_names() {
        let entries = effective_attach_keybindings(&BmuxConfig::default());
        assert!(entries.iter().any(|entry| {
            entry.scope == AttachKeybindingScope::Runtime
                && entry.chord == "o"
                && entry.action_name == "focus_next_pane"
                && entry.action == crate::input::RuntimeAction::FocusNext
        }));
        assert!(entries.iter().any(|entry| {
            entry.scope == AttachKeybindingScope::Global
                && entry.chord == "alt+h"
                && entry.action_name == "focus_left_pane"
                && entry.action == crate::input::RuntimeAction::FocusLeft
        }));
    }

    #[test]
    fn adjust_help_overlay_scroll_clamps_to_bounds() {
        assert_eq!(adjust_help_overlay_scroll(0, -10, 20, 5), 0);
        assert_eq!(adjust_help_overlay_scroll(0, 3, 20, 5), 3);
        assert_eq!(adjust_help_overlay_scroll(17, 10, 20, 5), 15);
        assert_eq!(adjust_help_overlay_scroll(4, -2, 20, 5), 2);
        assert_eq!(adjust_help_overlay_scroll(0, 4, 0, 5), 0);
    }

    #[test]
    fn help_overlay_repeat_navigation_is_handled() {
        let mut view_state = AttachViewState::new(bmux_client::AttachOpenInfo {
            context_id: None,
            session_id: uuid::Uuid::new_v4(),
            can_write: true,
        });
        view_state.help_overlay_open = true;
        view_state.dirty.full_pane_redraw = false;
        view_state.dirty.overlay_needs_redraw = false;
        let lines = (0..200)
            .map(|idx| format!("line {idx}"))
            .collect::<Vec<_>>();

        let handled = handle_help_overlay_key_event(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Down,
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Repeat,
            ),
            &lines,
            &mut view_state,
        );
        assert!(handled);
        assert!(view_state.help_overlay_scroll > 0);
        assert!(view_state.dirty.overlay_needs_redraw);
        assert!(!view_state.dirty.full_pane_redraw);
    }

    #[test]
    fn help_overlay_release_is_ignored() {
        let mut view_state = AttachViewState::new(bmux_client::AttachOpenInfo {
            context_id: None,
            session_id: uuid::Uuid::new_v4(),
            can_write: true,
        });
        view_state.help_overlay_open = true;
        view_state.help_overlay_scroll = 5;
        let lines = (0..200)
            .map(|idx| format!("line {idx}"))
            .collect::<Vec<_>>();

        let handled = handle_help_overlay_key_event(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Down,
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Release,
            ),
            &lines,
            &mut view_state,
        );
        assert!(!handled);
        assert_eq!(view_state.help_overlay_scroll, 5);
    }

    #[test]
    fn help_overlay_close_marks_full_redraw() {
        let mut view_state = AttachViewState::new(bmux_client::AttachOpenInfo {
            context_id: None,
            session_id: uuid::Uuid::new_v4(),
            can_write: true,
        });
        view_state.help_overlay_open = true;
        view_state.help_overlay_scroll = 3;
        view_state.dirty.status_needs_redraw = false;
        view_state.dirty.full_pane_redraw = false;
        view_state.dirty.overlay_needs_redraw = false;

        let handled = handle_help_overlay_key_event(
            &CrosstermKeyEvent::new_with_kind(
                CrosstermKeyCode::Esc,
                KeyModifiers::NONE,
                CrosstermKeyEventKind::Press,
            ),
            &[],
            &mut view_state,
        );

        assert!(handled);
        assert!(!view_state.help_overlay_open);
        assert_eq!(view_state.help_overlay_scroll, 0);
        assert!(view_state.dirty.status_needs_redraw);
        assert!(view_state.dirty.full_pane_redraw);
    }

    #[test]
    fn build_attach_help_lines_groups_entries_by_category() {
        let lines = build_attach_help_lines(&BmuxConfig::default());
        assert_eq!(lines.first().map(String::as_str), Some("Attach Help"));
        assert!(lines[1].contains("Modal keybindings are active"));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("restart an exited pane in place"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("opens a confirmation prompt before closing"))
        );
        assert!(lines.iter().any(|line| line == "-- Session --"));
        assert!(lines.iter().any(|line| line == "-- Pane --"));
        assert!(lines.iter().any(|line| line == "-- Mode --"));
    }

    #[test]
    fn attach_exit_message_suppresses_normal_detach_and_formats_stream_close() {
        assert_eq!(attach_exit_message(AttachExitReason::Detached), None);
        assert_eq!(attach_exit_message(AttachExitReason::Quit), None);
        assert_eq!(
            attach_exit_message(AttachExitReason::StreamClosed),
            Some("attach ended unexpectedly: server stream closed")
        );
    }

    #[test]
    fn resize_attach_parsers_applies_layout_size_before_snapshot_bytes() {
        let pane_id = uuid::Uuid::new_v4();
        let scene = bmux_ipc::AttachScene {
            session_id: uuid::Uuid::new_v4(),
            focus: bmux_ipc::AttachFocusTarget::Pane { pane_id },
            surfaces: vec![bmux_ipc::AttachSurface {
                id: pane_id,
                kind: bmux_ipc::AttachSurfaceKind::Pane,
                layer: bmux_ipc::AttachLayer::Pane,
                z: 0,
                rect: bmux_ipc::AttachRect {
                    x: 0,
                    y: 1,
                    w: 120,
                    h: 49,
                },
                // Content rect reflects the server's 1-cell border inset.
                content_rect: bmux_ipc::AttachRect {
                    x: 1,
                    y: 2,
                    w: 118,
                    h: 47,
                },
                interactive_regions: Vec::new(),
                opaque: true,
                visible: true,
                accepts_input: true,
                cursor_owner: true,
                pane_id: Some(pane_id),
            }],
        };
        let mut pane_buffers = BTreeMap::new();
        pane_buffers.insert(pane_id, PaneRenderBuffer::default());

        resize_attach_parsers_for_scene_with_size(&mut pane_buffers, &scene, 120, 50);

        let buffer = pane_buffers
            .get_mut(&pane_id)
            .expect("pane buffer should exist");
        append_pane_output(&mut *buffer, b"\x1b[999;999H");
        let (row, col) = buffer.parser.screen().cursor_position();

        assert_eq!(row, 46, "cursor row should clamp to pane inner height");
        assert_eq!(col, 117, "cursor col should clamp to pane inner width");
    }

    #[test]
    fn keymap_compiles_when_user_config_uses_arrow_aliases() {
        // Regression test: user config uses "shift+left" while defaults use
        // "shift+arrow_left". Both parse to the same keystroke. Without chord
        // canonicalization this produces a "duplicate runtime key binding chord"
        // error that prevents the entire keymap from loading.
        let mut config = BmuxConfig::default();
        config
            .keybindings
            .runtime
            .insert("shift+left".to_string(), "resize_left".to_string());
        config
            .keybindings
            .runtime
            .insert("left".to_string(), "focus_left_pane".to_string());

        // This must not panic or return Err.
        let _keymap = attach_keymap_from_config(&config);
    }

    #[test]
    fn apply_attach_profile_switch_rolls_back_on_resolution_failure() {
        let temp_path = std::env::temp_dir().join(format!(
            "bmux-switch-profile-rollback-{}-{}.toml",
            std::process::id(),
            Uuid::new_v4()
        ));
        let initial_config = r#"
[composition]
active_profile = "good"
layer_order = ["defaults", "profile:active", "config"]

[composition.profiles.good.patch.general]
server_timeout = 1234
"#;
        std::fs::write(&temp_path, initial_config).expect("write temp config");

        let mut processor =
            InputProcessor::new(attach_keymap_from_config(&BmuxConfig::default()), false);
        let mut view_state = AttachViewState::new(AttachOpenInfo {
            context_id: None,
            session_id: Uuid::new_v4(),
            can_write: true,
        });
        let original_mode = processor.active_mode_id().map(ToString::to_string);

        let error = apply_attach_profile_switch_with_path(
            "missing_profile",
            &mut processor,
            &mut view_state,
            &temp_path,
        )
        .expect_err("missing profile should fail and rollback");
        assert!(error.to_string().contains("rolled back profile switch"));

        let after = std::fs::read_to_string(&temp_path).expect("read temp config");
        assert_eq!(after, initial_config);
        assert_eq!(processor.active_mode_id(), original_mode.as_deref());
    }
}
