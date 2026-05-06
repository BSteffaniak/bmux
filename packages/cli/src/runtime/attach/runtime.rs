use anyhow::{Context, Result};
use bmux_appearance::{RUNTIME_APPEARANCE_STATE_KIND, RuntimeAppearance};
use bmux_attach_layout_protocol::attach_layout_protocol::{
    AttachLayoutSnapshot, AttachSurfaceSummary, STATE_KIND as ATTACH_LAYOUT_STATE_KIND,
};
use bmux_attach_layout_protocol::{
    AttachLayer as SurfaceLayer, AttachMouseProtocolEncoding, AttachMouseProtocolMode, AttachRect,
    AttachScene, AttachSurface, AttachSurfaceKind,
};
use bmux_attach_pipeline::mouse as attach_mouse;
use bmux_attach_pipeline::reconcile::{
    apply_attach_output_chunk_with, attach_scene_damage_between,
};
use bmux_attach_pipeline::{
    AttachChunkApplyOutcome, AttachOutputChunkMeta, DamageCoalescingPolicy, DamageRect,
    PaneScrollbackWindow, RetainedOpacity, RetainedRepaintSurface, RetainedSurface,
    RetainedSurfacePayload, frame_damage_from_retained_repaint_plan, merge_retained_damages,
    retained_damage_from_absolute_rects, retained_frame_damage_from_frame_damage,
    retained_layer_order, retained_surfaces_from_attach_scene, update_protocol_hints_from_state,
};
use bmux_attach_view_protocol::AttachViewComponent;
use bmux_client::{
    AttachLayoutState, AttachPaneSnapshotState, AttachSnapshotState, ClientError,
    StreamingBmuxClient,
};
use bmux_config::{BmuxConfig, ConfigPaths, PaneRestoreMethod, ResolvedTimeout, StatusPosition};
use bmux_context_state::ContextSelector;
use bmux_ipc::InvokeServiceKind;
use bmux_keybind::{action_to_config_name, parse_action};
use bmux_pane_runtime_plugin_api::pane_runtime_events as pane_events;
use bmux_permissions_plugin_api::session_policy_state;
use bmux_plugin_sdk::{
    COMMAND_OUTCOME_STATUS_MESSAGE_KEY, HostScope, PluginCommandOutcome, ServiceKind,
    ServiceRequest,
    perf_telemetry::{PhaseChannel, emit as emit_phase_timing},
};
use bmux_recording_plugin_api::recording_state;
use bmux_recording_protocol::{DisplayActivityKind, RecordingCaptureTarget};
use bmux_session_models::SessionSelector;
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
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, Instant};
use tracing::{debug, trace, warn};
use uuid::Uuid;

use super::super::prompt::{self, PromptOption, PromptRequest, PromptResponse, PromptValue};
use super::super::{
    ATTACH_SCROLLBACK_UNAVAILABLE_STATUS, ATTACH_SELECTION_CLEARED_STATUS,
    ATTACH_SELECTION_COPIED_STATUS, ATTACH_SELECTION_EMPTY_STATUS, ATTACH_SELECTION_STARTED_STATUS,
    ATTACH_TRANSIENT_STATUS_TTL, ATTACH_WELCOME_STATUS_TTL, BmuxClient, HELP_OVERLAY_SURFACE_ID,
    InputProcessor, KernelClientFactory, Keymap, RuntimeAction, action_dispatch, attach,
    attach_quit_failure_status, available_capability_providers, available_service_descriptors,
    command_accepts_repeat, effective_enabled_plugins, enter_host_kernel_connection,
    host_kernel_bridge, load_plugin, map_attach_client_error, merged_runtime_keybindings,
    parse_session_selector, parse_uuid_value, plugin_command_policy_hints, plugin_host_metadata,
    recording, resolve_plugin_search_paths, run_plugin_keybinding_command_with_active_bindings,
    scan_available_plugins,
};
use super::adapters::{AttachClock, SystemAttachClock};
use super::cursor::apply_attach_cursor_state;
use super::events::{AttachLoopControl, AttachLoopEvent, AttachTerminalEvent};
use super::input::{
    TerminalGeometry, TerminalInputEvent, TerminalKeyEvent, TerminalMouseButton,
    TerminalMouseEvent, TerminalMousePhase,
};
use super::prompt_ui::{
    AttachCloseFallbackTarget, AttachInternalPromptAction, AttachPromptCompletion,
    AttachPromptOrigin, AttachPromptOverlayRender, PromptKeyDisposition, prompt_accepts_key_kind,
};
use super::render::{
    AttachRenderTrace, AttachRenderTraceOp, AttachSceneRenderStats, frame_damage_overlay_rects,
    frame_damage_overlay_render_ops, opaque_row_text, queue_render_ops,
    render_attach_scene_with_stats_and_trace, visible_scene_pane_ids,
};
use super::state::{
    AttachDirtySource, AttachEventAction, AttachExitReason, AttachMouseFloatingDrag,
    AttachMouseResizeAxisDrag, AttachMouseResizeDrag, AttachMouseSelectionDrag, AttachMouseTabDrag,
    AttachScrollbackCursor, AttachScrollbackPosition, AttachTabDropPlacement, AttachTabDropTarget,
    AttachUiEffect, AttachUiMode, AttachUiReduction, AttachViewState, PaneRenderBuffer,
};
use crate::pane_runtime_client::{
    BmuxPaneRuntimeClientExt, PaneGridWindowRequest, attach_pane_grid_delta_state_streaming,
    attach_pane_grid_snapshot_state_streaming, attach_pane_grid_window_state_streaming,
};
use crate::status::{AttachStatusLine, AttachTab, build_attach_status_line};
use bmux_plugin::{BorderGlyphs, ExtensionRect, RenderDamage, RenderOp, RenderStyle};

const ATTACH_OUTPUT_BATCH_MAX_BYTES: usize = 8 * 1024;
const ATTACH_STRUCTURED_SNAPSHOT_MAX_BYTES_PER_PANE: usize = 0;
const ATTACH_GRID_SNAPSHOT_FALLBACK_ROWS_PER_PANE: usize = 1;
const ATTACH_GRID_DELTA_MAX_BATCHES_PER_PANE: usize = 128;
const ATTACH_OUTPUT_DRAIN_MAX_ROUNDS: usize = 8;
const COMMAND_OUTCOME_SELECTED_CONTEXT_ID_KEY: &str = "bmux.contexts.selected_context_id";
const ATTACH_PLUGIN_STATUS_MESSAGE_MAX_CHARS: usize = 180;
const ATTACH_TAB_DRAG_THRESHOLD_CELLS: u16 = 1;
/// Maximum wall-clock time the drain loop may spend waiting for an in-
/// progress output burst to complete (e.g. when the server indicates
/// `output_still_pending` or the inner application is mid-synchronized-
/// update).  Each IPC round-trip (~50-200 µs on a local Unix socket)
/// naturally yields CPU time to the PTY reader thread, so no explicit
/// sleep/yield is needed between rounds.
const ATTACH_OUTPUT_DRAIN_TIME_BUDGET: Duration = Duration::from_millis(4);
const ATTACH_OVERRENDER_MIN_FRAMES: u64 = 10;
const ATTACH_OVERRENDER_WINDOW_RATIO_PERCENT: u64 = 20;
const ATTACH_OVERRENDER_CACHED_SKIP_RATIO_PERCENT: u64 = 90;
const ATTACH_OVERRENDER_MIN_ROWS_EXAMINED: u64 = 10;
const ATTACH_OVERRENDER_NON_FULL_FRAME_CELL_RATIO_PERCENT: u64 = 50;
const ATTACH_OVERRENDER_EXTENSION_FULL_SURFACE_RATIO_PERCENT: u64 = 80;
const ATTACH_OVERRENDER_EXTENSION_IMPERATIVE_OR_MISS_RATIO_PERCENT: u64 = 80;
const ATTACH_OVERRENDER_SLOW_TERMINAL_WRITE_MS_PER_KIB: u64 = 8;
const STATUS_SURFACE_ID: Uuid = Uuid::from_u128(3);
const DAMAGE_OVERLAY_SURFACE_ID: Uuid = Uuid::from_u128(4);

fn emit_attach_phase_timing(payload: &serde_json::Value) {
    emit_phase_timing(PhaseChannel::Attach, payload);
}

#[allow(clippy::too_many_arguments)]
fn emit_attach_plugin_command_timing(
    plugin_id: &str,
    command_name: &str,
    status: &str,
    before_context_id: Option<Uuid>,
    after_context_id: Option<Uuid>,
    attached_context_id: Option<Uuid>,
    attached_session_id: Uuid,
    before_us: u128,
    policy_us: u128,
    run_us: u128,
    outcome_us: u128,
    after_us: u128,
    retarget_us: u128,
    total_us: u128,
) {
    emit_attach_phase_timing(&serde_json::json!({
        "phase": "attach.plugin_command",
        "plugin_id": plugin_id,
        "command_name": command_name,
        "status": status,
        "before_context_id": before_context_id,
        "after_context_id": after_context_id,
        "attached_context_id": attached_context_id,
        "attached_session_id": attached_session_id,
        "before_us": before_us,
        "policy_us": policy_us,
        "run_us": run_us,
        "outcome_us": outcome_us,
        "after_us": after_us,
        "retarget_us": retarget_us,
        "total_us": total_us,
    }));
}

use bmux_clients_plugin_api::{clients_state, clients_state::ClientSummary};
use bmux_contexts_plugin_api::{
    contexts_state, contexts_state::ContextSummary as PluginContextSummary,
};
use bmux_control_catalog_plugin_api::control_catalog_state::{
    self, ContextRow, ContextSessionBinding, SessionRow,
};
use bmux_performance_plugin_api::performance_state;
use bmux_plugin_sdk::{TypedServiceClientError, TypedServiceEndpoint};
use bmux_sessions_plugin_api::{sessions_commands, sessions_state};
use bmux_windows_plugin_api::{windows_commands, windows_state};

fn typed_client_error(error: &TypedServiceClientError) -> ClientError {
    ClientError::ServerError {
        code: bmux_ipc::ErrorCode::Internal,
        message: error.to_string(),
    }
}

#[must_use]
fn ipc_to_windows_selector(selector: SessionSelector) -> windows_commands::Selector {
    match selector {
        SessionSelector::ById(id) => windows_commands::Selector {
            id: Some(id),
            name: None,
            index: None,
        },
        SessionSelector::ByName(name) => windows_commands::Selector {
            id: None,
            name: Some(name),
            index: None,
        },
    }
}

#[must_use]
const fn pane_id_windows_selector(id: Uuid) -> windows_commands::Selector {
    windows_commands::Selector {
        id: Some(id),
        name: None,
        index: None,
    }
}

#[must_use]
fn ipc_to_session_selector(selector: SessionSelector) -> sessions_state::SessionSelector {
    match selector {
        SessionSelector::ById(id) => sessions_state::SessionSelector {
            id: Some(id),
            name: None,
        },
        SessionSelector::ByName(name) => sessions_state::SessionSelector {
            id: None,
            name: Some(name),
        },
    }
}

/// Typed dispatch wrapper for `sessions-commands:kill-session`.
async fn typed_kill_session_attach(
    client: &mut StreamingBmuxClient,
    selector: SessionSelector,
    force_local: bool,
) -> std::result::Result<
    std::result::Result<
        bmux_sessions_plugin_api::sessions_commands::SessionAck,
        bmux_sessions_plugin_api::sessions_commands::KillSessionError,
    >,
    ClientError,
> {
    let args = sessions_commands::client::KillSessionRequest {
        selector: ipc_to_session_selector(selector),
        force_local,
    };
    let payload = bmux_codec::to_vec(&args).map_err(|error| ClientError::ServerError {
        code: bmux_ipc::ErrorCode::Internal,
        message: format!("encoding kill-session args: {error}"),
    })?;
    let response_bytes = client
        .invoke_service_raw(
            sessions_commands::client::KillSessionEndpoint::CAPABILITY.as_str(),
            sessions_commands::client::KillSessionEndpoint::KIND,
            sessions_commands::client::KillSessionEndpoint::INTERFACE_ID.as_str(),
            sessions_commands::client::KillSessionEndpoint::OPERATION.as_str(),
            payload,
        )
        .await?;
    bmux_codec::from_bytes(&response_bytes).map_err(|error| ClientError::ServerError {
        code: bmux_ipc::ErrorCode::Internal,
        message: format!("decoding kill-session response: {error}"),
    })
}

/// Typed dispatch wrapper for `contexts-state:list-contexts`.
async fn typed_list_contexts_attach(
    client: &mut StreamingBmuxClient,
) -> std::result::Result<Vec<PluginContextSummary>, ClientError> {
    contexts_state::client::list_contexts(client)
        .await
        .map_err(|error| typed_client_error(&error))
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
/// through the normal subscription path. This matches the control-catalog
/// plugin snapshot pull-on-connect pattern.
async fn typed_list_windows_attach(
    client: &mut StreamingBmuxClient,
) -> std::result::Result<Vec<windows_state::WindowEntry>, ClientError> {
    windows_state::client::list_windows(client, None)
        .await
        .map_err(|error| typed_client_error(&error))
}

/// Pull the theme plugin's retained runtime appearance on attach startup.
///
/// Server-side state forwarders only propagate future state changes to
/// streaming clients; a new attach must seed its local process event bus by
/// querying the plugin-owned state surface once.
async fn typed_active_runtime_appearance_attach(
    client: &mut StreamingBmuxClient,
) -> std::result::Result<RuntimeAppearance, ClientError> {
    let payload = bmux_codec::to_vec(&()).map_err(|error| ClientError::ServerError {
        code: bmux_ipc::ErrorCode::Internal,
        message: format!("encoding active-appearance args: {error}"),
    })?;
    let response_bytes = client
        .invoke_service_raw(
            "bmux.theme.read",
            InvokeServiceKind::Query,
            "theme-state",
            "active-appearance",
            payload,
        )
        .await?;
    bmux_codec::from_bytes(&response_bytes).map_err(|error| ClientError::ServerError {
        code: bmux_ipc::ErrorCode::Internal,
        message: format!("decoding active runtime appearance response: {error}"),
    })
}

/// Pull the decoration plugin's retained scene on attach startup.
///
/// Decoration scenes are retained in the server process, while each attach
/// process owns a fresh client-side renderer cache. A reconnect can miss the
/// original scene publication if nothing changes after the new client attaches,
/// so hydrate the local cache by querying the plugin-owned snapshot surface.
async fn typed_decoration_scene_snapshot_attach(
    client: &mut StreamingBmuxClient,
) -> std::result::Result<bmux_scene_protocol::scene_protocol::DecorationScene, ClientError> {
    let payload = bmux_codec::to_vec(&()).map_err(|error| ClientError::ServerError {
        code: bmux_ipc::ErrorCode::Internal,
        message: format!("encoding decoration scene snapshot args: {error}"),
    })?;
    let response_bytes = client
        .invoke_service_raw(
            "bmux.decoration.read",
            InvokeServiceKind::Query,
            "decoration-state",
            "scene-snapshot",
            payload,
        )
        .await?;
    bmux_codec::from_bytes(&response_bytes).map_err(|error| ClientError::ServerError {
        code: bmux_ipc::ErrorCode::Internal,
        message: format!("decoding decoration scene snapshot response: {error}"),
    })
}

async fn hydrate_decoration_scene_snapshot_attach(
    client: &mut StreamingBmuxClient,
    expect_surfaces: bool,
) -> std::result::Result<bmux_scene_protocol::scene_protocol::DecorationScene, ClientError> {
    const HYDRATE_ATTEMPTS_WITH_LAYOUT: usize = 6;
    const HYDRATE_RETRY_DELAY: Duration = Duration::from_millis(25);

    let attempts = if expect_surfaces {
        HYDRATE_ATTEMPTS_WITH_LAYOUT
    } else {
        1
    };
    for attempt in 0..attempts {
        let scene = typed_decoration_scene_snapshot_attach(client).await?;
        if !expect_surfaces || !scene.surfaces.is_empty() || attempt + 1 == attempts {
            return Ok(scene);
        }
        tokio::time::sleep(HYDRATE_RETRY_DELAY).await;
    }

    typed_decoration_scene_snapshot_attach(client).await
}

const fn empty_decoration_scene() -> bmux_scene_protocol::scene_protocol::DecorationScene {
    bmux_scene_protocol::scene_protocol::DecorationScene {
        revision: 0,
        surfaces: BTreeMap::new(),
        animation: None,
    }
}

fn publish_decoration_scene_locally(scene: bmux_scene_protocol::scene_protocol::DecorationScene) {
    let _ = bmux_plugin::global_event_bus()
        .register_state_channel::<bmux_scene_protocol::scene_protocol::DecorationScene>(
            bmux_scene_protocol::scene_protocol::STATE_KIND,
            empty_decoration_scene(),
        );
    let _ = bmux_plugin::global_event_bus().publish_state(
        &bmux_scene_protocol::scene_protocol::STATE_KIND,
        scene.clone(),
    );
    #[cfg(feature = "bundled-plugin-decoration")]
    {
        let _ = bmux_decoration_plugin_renderer::push_scene(scene);
    }
}

/// Typed dispatch wrapper for `contexts-state:current-context`.
async fn typed_current_context_attach(
    client: &mut StreamingBmuxClient,
) -> std::result::Result<Option<PluginContextSummary>, ClientError> {
    contexts_state::client::current_context(client)
        .await
        .map_err(|error| typed_client_error(&error))
}

/// Typed dispatch wrapper for `clients-state:list-clients` on a plain
/// [`BmuxClient`] (used before streaming upgrade).
async fn typed_list_clients_bmux(
    client: &mut BmuxClient,
) -> std::result::Result<Vec<ClientSummary>, ClientError> {
    clients_state::client::list_clients(client)
        .await
        .map_err(|error| typed_client_error(&error))
}

/// Typed dispatch wrapper for `contexts-state:list-contexts` on a
/// plain [`BmuxClient`].
async fn typed_list_contexts_bmux(
    client: &mut BmuxClient,
) -> std::result::Result<Vec<PluginContextSummary>, ClientError> {
    contexts_state::client::list_contexts(client)
        .await
        .map_err(|error| typed_client_error(&error))
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
            windows_commands::client::FocusPaneEndpoint::CAPABILITY.as_str(),
            InvokeServiceKind::Command,
            windows_commands::INTERFACE_ID.as_str(),
            operation,
            payload,
        )
        .await?;
    bmux_codec::from_bytes::<Resp>(&response_bytes).map_err(|error| ClientError::ServerError {
        code: bmux_ipc::ErrorCode::Internal,
        message: format!("decoding {operation} response: {error}"),
    })
}

#[derive(Default)]
pub struct DisplayCaptureFanout {
    writers: BTreeMap<Uuid, recording::DisplayCaptureWriter>,
}

impl DisplayCaptureFanout {
    fn open_target(&mut self, target: &RecordingCaptureTarget, client_id: Uuid) {
        if self.writers.contains_key(&target.recording_id) {
            return;
        }
        match recording::DisplayCaptureWriter::open(
            target.recording_id,
            Path::new(&target.path),
            client_id,
            target.rolling_window_secs,
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

    fn record_activity(&mut self, kind: DisplayActivityKind) {
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
    fn record_images(&mut self, images: &[bmux_attach_image_protocol::AttachPaneImage]) {
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

            toggled_alternate =
                super::render::append_pane_output(buffer, data) || toggled_alternate;
            update_protocol_hints_from_state(
                pane_mouse_protocol_hints,
                pane_input_mode_hints,
                pane_id,
                buffer.protocol_tracker.protocol_state(),
            );
            true
        },
    );

    if outcome == (AttachChunkApplyOutcome::Applied { had_data: true }) {
        view_state
            .dirty
            .mark_pane_dirty(pane_id, AttachDirtySource::PaneOutput);
        *frame_needs_render = true;
    }

    if toggled_alternate {
        view_state
            .dirty
            .mark_full_frame(AttachDirtySource::AlternateScreenTransition);
        view_state.force_cursor_move_next_frame = true;
    }

    outcome
}

fn bytes_contain_sequence(bytes: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && bytes.windows(needle.len()).any(|window| window == needle)
}

fn chunk_may_disable_mouse_protocol(bytes: &[u8]) -> bool {
    const MOUSE_DISABLE_SEQUENCES: &[&[u8]] = &[
        b"\x1b[?9l",
        b"\x1b[?1000l",
        b"\x1b[?1002l",
        b"\x1b[?1003l",
        b"\x1b[?1005l",
        b"\x1b[?1006l",
        b"\x1b[?1015l",
    ];
    MOUSE_DISABLE_SEQUENCES
        .iter()
        .any(|sequence| bytes_contain_sequence(bytes, sequence))
}

fn apply_attach_output_chunk_protocol_only(
    view_state: &mut AttachViewState,
    pane_id: Uuid,
    bytes: &[u8],
    meta: AttachOutputChunkMeta,
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

            let previous_mouse_hint = pane_mouse_protocol_hints.get(&pane_id).copied();
            let was_alternate = buffer.protocol_tracker.alternate_screen();
            let protocol_outcome = buffer.protocol_tracker.process(data);
            let is_alternate = buffer.protocol_tracker.alternate_screen();
            toggled_alternate = toggled_alternate
                || protocol_outcome.toggled_alternate
                || was_alternate != is_alternate;
            update_protocol_hints_from_state(
                pane_mouse_protocol_hints,
                pane_input_mode_hints,
                pane_id,
                buffer.protocol_tracker.protocol_state(),
            );
            if pane_mouse_protocol_hints
                .get(&pane_id)
                .is_some_and(|hint| hint.mode == AttachMouseProtocolMode::None)
                && !chunk_may_disable_mouse_protocol(data)
                && let Some(previous) = previous_mouse_hint
            {
                pane_mouse_protocol_hints.insert(pane_id, previous);
            }
            true
        },
    );

    if toggled_alternate {
        view_state
            .dirty
            .mark_full_frame(AttachDirtySource::AlternateScreenTransition);
        view_state.force_cursor_move_next_frame = true;
    }

    outcome
}

async fn recover_attach_output_desync_for_pane(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    pane_id: Uuid,
) -> std::result::Result<(), ClientError> {
    if let Some(layout_state) = view_state.cached_layout_state.clone()
        && attach_layout_pane_id_set(&layout_state).contains(&pane_id)
        && hydrate_attach_revealed_panes_from_snapshot(
            client,
            view_state,
            &layout_state,
            &[pane_id],
        )
        .await
        .is_ok()
    {
        view_state
            .dirty
            .mark_full_frame(AttachDirtySource::SnapshotHydration);
        return Ok(());
    }

    hydrate_attach_state_from_snapshot_mode(client, view_state, SnapshotHydrationMode::FullResync)
        .await
}

#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_excessive_bools)] // Independent frame facts keep telemetry aggregation explicit.
pub struct AttachFrameRenderStats {
    pub frame_bytes: usize,
    pub terminal_write_ms: u64,
    pub damage_rects: usize,
    pub damage_area_cells: u64,
    pub full_surface_fallbacks: usize,
    pub full_frame_fallback: bool,
    pub scene_render: AttachSceneRenderStats,
    pub status_rendered: bool,
    pub overlay_rendered: bool,
    pub synchronized_update: bool,
    pub dirty_event_count: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct AttachRenderInefficiencyFlags {
    bits: u16,
}

impl AttachRenderInefficiencyFlags {
    const DIRTY_NO_VISIBLE_ROW_CHANGE: u16 = 1 << 0;
    const HIGH_CACHED_SKIP_RATIO: u16 = 1 << 1;
    const LARGE_PARTIAL_FRAME: u16 = 1 << 2;
    const EXTENSION_FULL_SURFACE_EXCESSIVE: u16 = 1 << 3;
    const FULL_FRAME_FALLBACK: u16 = 1 << 4;
    const EXTENSION_IMPERATIVE_OR_CACHE_MISS: u16 = 1 << 5;
    const STATUS_OVERLAY_ONLY_EMITS_PANE_WORK: u16 = 1 << 6;
    const SLOW_TERMINAL_WRITE_PER_KIB: u16 = 1 << 7;

    const fn is_empty(self) -> bool {
        self.bits == 0
    }

    const fn contains(self, flag: u16) -> bool {
        self.bits & flag != 0
    }

    const fn with(mut self, flag: u16, enabled: bool) -> Self {
        if enabled {
            self.bits |= flag;
        }
        self
    }
}

const fn percent_at_least(part: u64, total: u64, threshold_percent: u64) -> bool {
    total > 0 && part.saturating_mul(100) >= total.saturating_mul(threshold_percent)
}

fn classify_attach_render_inefficiency(
    stats: &AttachFrameRenderStats,
) -> AttachRenderInefficiencyFlags {
    let emitted_units = stats
        .scene_render
        .pane_rows_emitted
        .saturating_add(stats.scene_render.pane_row_segments_emitted);
    let dirty_no_visible_row_change = stats.dirty_event_count > 0
        && stats.scene_render.pane_rows_examined > 0
        && emitted_units == 0;
    let high_cached_skip_ratio = stats.scene_render.pane_rows_examined
        >= ATTACH_OVERRENDER_MIN_ROWS_EXAMINED
        && percent_at_least(
            stats.scene_render.pane_rows_cached_skipped,
            stats.scene_render.pane_rows_examined,
            ATTACH_OVERRENDER_CACHED_SKIP_RATIO_PERCENT,
        );
    let large_partial_frame = !stats.full_frame_fallback
        && percent_at_least(
            stats.scene_render.pane_cells_emitted,
            stats.scene_render.viewport_cells,
            ATTACH_OVERRENDER_NON_FULL_FRAME_CELL_RATIO_PERCENT,
        );
    let extension_full_surface_excessive = percent_at_least(
        stats.scene_render.extension_full_surface_calls,
        stats.scene_render.extension_render_calls,
        ATTACH_OVERRENDER_EXTENSION_FULL_SURFACE_RATIO_PERCENT,
    );
    let extension_cache_misses = stats
        .scene_render
        .extension_render_calls
        .saturating_sub(stats.scene_render.extension_cache_hits);
    let extension_imperative_or_cache_miss = percent_at_least(
        stats
            .scene_render
            .extension_imperative_calls
            .saturating_add(extension_cache_misses),
        stats.scene_render.extension_render_calls,
        ATTACH_OVERRENDER_EXTENSION_IMPERATIVE_OR_MISS_RATIO_PERCENT,
    );
    let status_overlay_only_emits_pane_work = !stats.full_frame_fallback
        && stats.damage_rects == 0
        && stats.full_surface_fallbacks == 0
        && (stats.status_rendered || stats.overlay_rendered)
        && stats.scene_render.pane_cells_emitted > 0;
    let slow_terminal_write_per_kib = stats.frame_bytes > 0
        && stats.terminal_write_ms.saturating_mul(1024)
            >= u64::try_from(stats.frame_bytes)
                .unwrap_or(u64::MAX)
                .saturating_mul(ATTACH_OVERRENDER_SLOW_TERMINAL_WRITE_MS_PER_KIB);

    AttachRenderInefficiencyFlags::default()
        .with(
            AttachRenderInefficiencyFlags::DIRTY_NO_VISIBLE_ROW_CHANGE,
            dirty_no_visible_row_change,
        )
        .with(
            AttachRenderInefficiencyFlags::HIGH_CACHED_SKIP_RATIO,
            high_cached_skip_ratio,
        )
        .with(
            AttachRenderInefficiencyFlags::LARGE_PARTIAL_FRAME,
            large_partial_frame,
        )
        .with(
            AttachRenderInefficiencyFlags::EXTENSION_FULL_SURFACE_EXCESSIVE,
            extension_full_surface_excessive,
        )
        .with(
            AttachRenderInefficiencyFlags::FULL_FRAME_FALLBACK,
            stats.full_frame_fallback,
        )
        .with(
            AttachRenderInefficiencyFlags::EXTENSION_IMPERATIVE_OR_CACHE_MISS,
            extension_imperative_or_cache_miss,
        )
        .with(
            AttachRenderInefficiencyFlags::STATUS_OVERLAY_ONLY_EMITS_PANE_WORK,
            status_overlay_only_emits_pane_work,
        )
        .with(
            AttachRenderInefficiencyFlags::SLOW_TERMINAL_WRITE_PER_KIB,
            slow_terminal_write_per_kib,
        )
}

#[derive(Debug, Clone, Copy)]
enum AttachWakeSource {
    Server,
    Terminal,
    Prompt,
    ActionDispatch,
    Appearance,
    Scene,
    WindowList,
}

#[derive(Debug, Clone, Copy)]
struct AttachDirtyReasons {
    bits: u8,
}

impl AttachDirtyReasons {
    const STATUS: u8 = 1 << 0;
    const FULL_PANE: u8 = 1 << 1;
    const OVERLAY: u8 = 1 << 2;
    const PANE_DIRTY: u8 = 1 << 3;
    const LAYOUT: u8 = 1 << 4;
    const SCENE_HYDRATED: u8 = 1 << 5;
    const EXTENSION: u8 = 1 << 6;

    fn from_view_state(view_state: &AttachViewState, layout: bool, scene_hydrated: bool) -> Self {
        let mut bits = 0;
        if view_state.dirty.status_needs_redraw {
            bits |= Self::STATUS;
        }
        if view_state.dirty.full_pane_redraw {
            bits |= Self::FULL_PANE;
        }
        if view_state.dirty.overlay_needs_redraw {
            bits |= Self::OVERLAY;
        }
        if !view_state.dirty.pane_dirty_ids.is_empty() {
            bits |= Self::PANE_DIRTY;
        }
        if layout {
            bits |= Self::LAYOUT;
        }
        if scene_hydrated {
            bits |= Self::SCENE_HYDRATED;
        }
        if view_state.dirty.extension_needs_redraw {
            bits |= Self::EXTENSION;
        }
        Self { bits }
    }

    const fn contains(self, flag: u8) -> bool {
        self.bits & flag != 0
    }
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
    frame_bytes_max: u64,
    terminal_write_ms_max: u64,
    damage_rects_max: u64,
    damage_area_cells_max: u64,
    full_surface_fallbacks: u64,
    full_frame_fallbacks: u64,
    pane_rows_examined: u64,
    pane_rows_emitted: u64,
    pane_row_segments_emitted: u64,
    pane_rows_cached_skipped: u64,
    pane_rows_sync_deferred: u64,
    pane_cells_emitted: u64,
    extension_render_calls: u64,
    extension_render_op_calls: u64,
    extension_imperative_calls: u64,
    extension_cache_hits: u64,
    extension_full_surface_calls: u64,
    status_rendered_frames: u64,
    overlay_rendered_frames: u64,
    synchronized_update_frames: u64,
    dirty_events: u64,
    overrender_flagged_frames: u64,
    dirty_no_visible_row_change_frames: u64,
    high_cached_skip_ratio_frames: u64,
    large_partial_frame_frames: u64,
    extension_full_surface_excessive_frames: u64,
    full_frame_fallback_flagged_frames: u64,
    extension_imperative_or_cache_miss_frames: u64,
    status_overlay_only_emits_pane_work_frames: u64,
    slow_terminal_write_per_kib_frames: u64,
    event_burst_drained_events: u64,
    event_burst_max_events: u64,
    wake_server_events: u64,
    wake_terminal_events: u64,
    wake_prompt_events: u64,
    wake_action_dispatch_events: u64,
    wake_appearance_events: u64,
    wake_scene_events: u64,
    wake_window_list_events: u64,
    dirty_status_frames: u64,
    dirty_full_pane_frames: u64,
    dirty_overlay_frames: u64,
    dirty_pane_frames: u64,
    dirty_layout_frames: u64,
    dirty_scene_hydrated_frames: u64,
    dirty_extension_frames: u64,
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
            frame_bytes_max: 0,
            terminal_write_ms_max: 0,
            damage_rects_max: 0,
            damage_area_cells_max: 0,
            full_surface_fallbacks: 0,
            full_frame_fallbacks: 0,
            pane_rows_examined: 0,
            pane_rows_emitted: 0,
            pane_row_segments_emitted: 0,
            pane_rows_cached_skipped: 0,
            pane_rows_sync_deferred: 0,
            pane_cells_emitted: 0,
            extension_render_calls: 0,
            extension_render_op_calls: 0,
            extension_imperative_calls: 0,
            extension_cache_hits: 0,
            extension_full_surface_calls: 0,
            status_rendered_frames: 0,
            overlay_rendered_frames: 0,
            synchronized_update_frames: 0,
            dirty_events: 0,
            overrender_flagged_frames: 0,
            dirty_no_visible_row_change_frames: 0,
            high_cached_skip_ratio_frames: 0,
            large_partial_frame_frames: 0,
            extension_full_surface_excessive_frames: 0,
            full_frame_fallback_flagged_frames: 0,
            extension_imperative_or_cache_miss_frames: 0,
            status_overlay_only_emits_pane_work_frames: 0,
            slow_terminal_write_per_kib_frames: 0,
            event_burst_drained_events: 0,
            event_burst_max_events: 0,
            wake_server_events: 0,
            wake_terminal_events: 0,
            wake_prompt_events: 0,
            wake_action_dispatch_events: 0,
            wake_appearance_events: 0,
            wake_scene_events: 0,
            wake_window_list_events: 0,
            dirty_status_frames: 0,
            dirty_full_pane_frames: 0,
            dirty_overlay_frames: 0,
            dirty_pane_frames: 0,
            dirty_layout_frames: 0,
            dirty_scene_hydrated_frames: 0,
            dirty_extension_frames: 0,
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

    fn record_render_frame(&mut self, elapsed_ms: u64, stats: AttachFrameRenderStats) {
        let inefficiency_flags = classify_attach_render_inefficiency(&stats);
        self.render_frames = self.render_frames.saturating_add(1);
        self.render_ms_sum = self.render_ms_sum.saturating_add(elapsed_ms);
        self.render_ms_max = self.render_ms_max.max(elapsed_ms);
        self.frame_bytes_max = self
            .frame_bytes_max
            .max(u64::try_from(stats.frame_bytes).unwrap_or(u64::MAX));
        self.terminal_write_ms_max = self.terminal_write_ms_max.max(stats.terminal_write_ms);
        self.damage_rects_max = self
            .damage_rects_max
            .max(u64::try_from(stats.damage_rects).unwrap_or(u64::MAX));
        self.damage_area_cells_max = self.damage_area_cells_max.max(stats.damage_area_cells);
        self.full_surface_fallbacks = self
            .full_surface_fallbacks
            .saturating_add(u64::try_from(stats.full_surface_fallbacks).unwrap_or(u64::MAX));
        if stats.full_frame_fallback {
            self.full_frame_fallbacks = self.full_frame_fallbacks.saturating_add(1);
        }
        self.pane_rows_examined = self
            .pane_rows_examined
            .saturating_add(stats.scene_render.pane_rows_examined);
        self.pane_rows_emitted = self
            .pane_rows_emitted
            .saturating_add(stats.scene_render.pane_rows_emitted);
        self.pane_row_segments_emitted = self
            .pane_row_segments_emitted
            .saturating_add(stats.scene_render.pane_row_segments_emitted);
        self.pane_rows_cached_skipped = self
            .pane_rows_cached_skipped
            .saturating_add(stats.scene_render.pane_rows_cached_skipped);
        self.pane_rows_sync_deferred = self
            .pane_rows_sync_deferred
            .saturating_add(stats.scene_render.pane_rows_sync_deferred);
        self.pane_cells_emitted = self
            .pane_cells_emitted
            .saturating_add(stats.scene_render.pane_cells_emitted);
        self.extension_render_calls = self
            .extension_render_calls
            .saturating_add(stats.scene_render.extension_render_calls);
        self.extension_render_op_calls = self
            .extension_render_op_calls
            .saturating_add(stats.scene_render.extension_render_op_calls);
        self.extension_imperative_calls = self
            .extension_imperative_calls
            .saturating_add(stats.scene_render.extension_imperative_calls);
        self.extension_cache_hits = self
            .extension_cache_hits
            .saturating_add(stats.scene_render.extension_cache_hits);
        self.extension_full_surface_calls = self
            .extension_full_surface_calls
            .saturating_add(stats.scene_render.extension_full_surface_calls);
        if stats.status_rendered {
            self.status_rendered_frames = self.status_rendered_frames.saturating_add(1);
        }
        if stats.overlay_rendered {
            self.overlay_rendered_frames = self.overlay_rendered_frames.saturating_add(1);
        }
        if stats.synchronized_update {
            self.synchronized_update_frames = self.synchronized_update_frames.saturating_add(1);
        }
        self.dirty_events = self
            .dirty_events
            .saturating_add(u64::try_from(stats.dirty_event_count).unwrap_or(u64::MAX));
        self.record_render_inefficiency_flags(inefficiency_flags);
    }

    const fn record_render_inefficiency_flags(
        &mut self,
        inefficiency_flags: AttachRenderInefficiencyFlags,
    ) {
        if !inefficiency_flags.is_empty() {
            self.overrender_flagged_frames = self.overrender_flagged_frames.saturating_add(1);
        }
        if inefficiency_flags.contains(AttachRenderInefficiencyFlags::DIRTY_NO_VISIBLE_ROW_CHANGE) {
            self.dirty_no_visible_row_change_frames =
                self.dirty_no_visible_row_change_frames.saturating_add(1);
        }
        if inefficiency_flags.contains(AttachRenderInefficiencyFlags::HIGH_CACHED_SKIP_RATIO) {
            self.high_cached_skip_ratio_frames =
                self.high_cached_skip_ratio_frames.saturating_add(1);
        }
        if inefficiency_flags.contains(AttachRenderInefficiencyFlags::LARGE_PARTIAL_FRAME) {
            self.large_partial_frame_frames = self.large_partial_frame_frames.saturating_add(1);
        }
        if inefficiency_flags
            .contains(AttachRenderInefficiencyFlags::EXTENSION_FULL_SURFACE_EXCESSIVE)
        {
            self.extension_full_surface_excessive_frames = self
                .extension_full_surface_excessive_frames
                .saturating_add(1);
        }
        if inefficiency_flags.contains(AttachRenderInefficiencyFlags::FULL_FRAME_FALLBACK) {
            self.full_frame_fallback_flagged_frames =
                self.full_frame_fallback_flagged_frames.saturating_add(1);
        }
        if inefficiency_flags
            .contains(AttachRenderInefficiencyFlags::EXTENSION_IMPERATIVE_OR_CACHE_MISS)
        {
            self.extension_imperative_or_cache_miss_frames = self
                .extension_imperative_or_cache_miss_frames
                .saturating_add(1);
        }
        if inefficiency_flags
            .contains(AttachRenderInefficiencyFlags::STATUS_OVERLAY_ONLY_EMITS_PANE_WORK)
        {
            self.status_overlay_only_emits_pane_work_frames = self
                .status_overlay_only_emits_pane_work_frames
                .saturating_add(1);
        }
        if inefficiency_flags.contains(AttachRenderInefficiencyFlags::SLOW_TERMINAL_WRITE_PER_KIB) {
            self.slow_terminal_write_per_kib_frames =
                self.slow_terminal_write_per_kib_frames.saturating_add(1);
        }
    }

    fn record_event_burst(&mut self, drained_events: usize) {
        let drained_events = u64::try_from(drained_events).unwrap_or(u64::MAX);
        self.event_burst_drained_events = self
            .event_burst_drained_events
            .saturating_add(drained_events);
        self.event_burst_max_events = self.event_burst_max_events.max(drained_events);
    }

    const fn record_wake(&mut self, source: AttachWakeSource) {
        match source {
            AttachWakeSource::Server => {
                self.wake_server_events = self.wake_server_events.saturating_add(1);
            }
            AttachWakeSource::Terminal => {
                self.wake_terminal_events = self.wake_terminal_events.saturating_add(1);
            }
            AttachWakeSource::Prompt => {
                self.wake_prompt_events = self.wake_prompt_events.saturating_add(1);
            }
            AttachWakeSource::ActionDispatch => {
                self.wake_action_dispatch_events =
                    self.wake_action_dispatch_events.saturating_add(1);
            }
            AttachWakeSource::Appearance => {
                self.wake_appearance_events = self.wake_appearance_events.saturating_add(1);
            }
            AttachWakeSource::Scene => {
                self.wake_scene_events = self.wake_scene_events.saturating_add(1);
            }
            AttachWakeSource::WindowList => {
                self.wake_window_list_events = self.wake_window_list_events.saturating_add(1);
            }
        }
    }

    const fn record_dirty_reasons(&mut self, reasons: AttachDirtyReasons) {
        if reasons.contains(AttachDirtyReasons::STATUS) {
            self.dirty_status_frames = self.dirty_status_frames.saturating_add(1);
        }
        if reasons.contains(AttachDirtyReasons::FULL_PANE) {
            self.dirty_full_pane_frames = self.dirty_full_pane_frames.saturating_add(1);
        }
        if reasons.contains(AttachDirtyReasons::OVERLAY) {
            self.dirty_overlay_frames = self.dirty_overlay_frames.saturating_add(1);
        }
        if reasons.contains(AttachDirtyReasons::PANE_DIRTY) {
            self.dirty_pane_frames = self.dirty_pane_frames.saturating_add(1);
        }
        if reasons.contains(AttachDirtyReasons::LAYOUT) {
            self.dirty_layout_frames = self.dirty_layout_frames.saturating_add(1);
        }
        if reasons.contains(AttachDirtyReasons::SCENE_HYDRATED) {
            self.dirty_scene_hydrated_frames = self.dirty_scene_hydrated_frames.saturating_add(1);
        }
        if reasons.contains(AttachDirtyReasons::EXTENSION) {
            self.dirty_extension_frames = self.dirty_extension_frames.saturating_add(1);
        }
    }

    fn reset(&mut self) {
        *self = Self::new();
    }
}

#[allow(clippy::cast_possible_truncation)]
fn duration_millis_u64(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
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

fn insert_attach_render_work_payload(
    object: &mut serde_json::Map<String, serde_json::Value>,
    window: &AttachPerfWindow,
) {
    object.insert(
        "pane_rows_examined".to_string(),
        window.pane_rows_examined.into(),
    );
    object.insert(
        "pane_rows_emitted".to_string(),
        window.pane_rows_emitted.into(),
    );
    object.insert(
        "pane_row_segments_emitted".to_string(),
        window.pane_row_segments_emitted.into(),
    );
    object.insert(
        "pane_rows_cached_skipped".to_string(),
        window.pane_rows_cached_skipped.into(),
    );
    object.insert(
        "pane_rows_sync_deferred".to_string(),
        window.pane_rows_sync_deferred.into(),
    );
    object.insert(
        "pane_cells_emitted".to_string(),
        window.pane_cells_emitted.into(),
    );
    object.insert(
        "extension_render_calls".to_string(),
        window.extension_render_calls.into(),
    );
    object.insert(
        "extension_render_op_calls".to_string(),
        window.extension_render_op_calls.into(),
    );
    object.insert(
        "extension_imperative_calls".to_string(),
        window.extension_imperative_calls.into(),
    );
    object.insert(
        "extension_cache_hits".to_string(),
        window.extension_cache_hits.into(),
    );
    object.insert(
        "extension_full_surface_calls".to_string(),
        window.extension_full_surface_calls.into(),
    );
    object.insert(
        "status_rendered_frames".to_string(),
        window.status_rendered_frames.into(),
    );
    object.insert(
        "overlay_rendered_frames".to_string(),
        window.overlay_rendered_frames.into(),
    );
    object.insert(
        "synchronized_update_frames".to_string(),
        window.synchronized_update_frames.into(),
    );
    object.insert("dirty_events".to_string(), window.dirty_events.into());
    object.insert(
        "overrender_flagged_frames".to_string(),
        window.overrender_flagged_frames.into(),
    );
    object.insert(
        "dirty_no_visible_row_change_frames".to_string(),
        window.dirty_no_visible_row_change_frames.into(),
    );
    object.insert(
        "high_cached_skip_ratio_frames".to_string(),
        window.high_cached_skip_ratio_frames.into(),
    );
    object.insert(
        "large_partial_frame_frames".to_string(),
        window.large_partial_frame_frames.into(),
    );
    object.insert(
        "extension_full_surface_excessive_frames".to_string(),
        window.extension_full_surface_excessive_frames.into(),
    );
    object.insert(
        "full_frame_fallback_flagged_frames".to_string(),
        window.full_frame_fallback_flagged_frames.into(),
    );
    object.insert(
        "extension_imperative_or_cache_miss_frames".to_string(),
        window.extension_imperative_or_cache_miss_frames.into(),
    );
    object.insert(
        "status_overlay_only_emits_pane_work_frames".to_string(),
        window.status_overlay_only_emits_pane_work_frames.into(),
    );
    object.insert(
        "slow_terminal_write_per_kib_frames".to_string(),
        window.slow_terminal_write_per_kib_frames.into(),
    );
}

fn insert_attach_perf_detailed_payload(
    object: &mut serde_json::Map<String, serde_json::Value>,
    window: &AttachPerfWindow,
) {
    object.insert(
        "drain_ipc_ms_sum".to_string(),
        window.drain_ipc_ms_sum.into(),
    );
    object.insert(
        "drain_ipc_ms_max".to_string(),
        window.drain_ipc_ms_max.into(),
    );
    object.insert("render_ms_sum".to_string(), window.render_ms_sum.into());
    object.insert("render_ms_max".to_string(), window.render_ms_max.into());
    object.insert(
        "event_burst_drained_events".to_string(),
        window.event_burst_drained_events.into(),
    );
    object.insert(
        "event_burst_max_events".to_string(),
        window.event_burst_max_events.into(),
    );
    object.insert(
        "wake_server_events".to_string(),
        window.wake_server_events.into(),
    );
    object.insert(
        "wake_terminal_events".to_string(),
        window.wake_terminal_events.into(),
    );
    object.insert(
        "wake_prompt_events".to_string(),
        window.wake_prompt_events.into(),
    );
    object.insert(
        "wake_action_dispatch_events".to_string(),
        window.wake_action_dispatch_events.into(),
    );
    object.insert(
        "wake_appearance_events".to_string(),
        window.wake_appearance_events.into(),
    );
    object.insert(
        "wake_scene_events".to_string(),
        window.wake_scene_events.into(),
    );
    object.insert(
        "wake_window_list_events".to_string(),
        window.wake_window_list_events.into(),
    );
    object.insert(
        "dirty_status_frames".to_string(),
        window.dirty_status_frames.into(),
    );
    object.insert(
        "dirty_full_pane_frames".to_string(),
        window.dirty_full_pane_frames.into(),
    );
    object.insert(
        "dirty_overlay_frames".to_string(),
        window.dirty_overlay_frames.into(),
    );
    object.insert(
        "dirty_pane_frames".to_string(),
        window.dirty_pane_frames.into(),
    );
    object.insert(
        "dirty_layout_frames".to_string(),
        window.dirty_layout_frames.into(),
    );
    object.insert(
        "dirty_scene_hydrated_frames".to_string(),
        window.dirty_scene_hydrated_frames.into(),
    );
    object.insert(
        "dirty_extension_frames".to_string(),
        window.dirty_extension_frames.into(),
    );
    insert_attach_render_work_payload(object, window);
    if let Some(avg) = window.drain_ipc_ms_sum.checked_div(window.drain_ipc_calls) {
        object.insert("drain_ipc_ms_avg".to_string(), avg.into());
    }
    if let Some(avg) = window.render_ms_sum.checked_div(window.render_frames) {
        object.insert("render_ms_avg".to_string(), avg.into());
    }
}

fn attach_perf_window_payload(
    window: &AttachPerfWindow,
    elapsed: Duration,
    detailed: bool,
    trace: bool,
) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "window_elapsed_ms": duration_millis_u64(elapsed),
        "drain_rounds": window.drain_rounds,
        "drain_ipc_calls": window.drain_ipc_calls,
        "drain_bytes": window.drain_bytes,
        "render_frames": window.render_frames,
        "frame_bytes_max": window.frame_bytes_max,
        "terminal_write_ms_max": window.terminal_write_ms_max,
        "damage_rects_max": window.damage_rects_max,
        "damage_area_cells_max": window.damage_area_cells_max,
        "full_surface_fallbacks": window.full_surface_fallbacks,
        "full_frame_fallbacks": window.full_frame_fallbacks,
        "event_burst_drained_events": window.event_burst_drained_events,
        "event_burst_max_events": window.event_burst_max_events,
    });
    if detailed && let Some(object) = payload.as_object_mut() {
        insert_attach_perf_detailed_payload(object, window);
    }
    if trace && let Some(object) = payload.as_object_mut() {
        object.insert(
            "drain_rounds_with_data".to_string(),
            window.drain_rounds_with_data.into(),
        );
        object.insert(
            "drain_sync_active_rounds".to_string(),
            window.drain_sync_active_rounds.into(),
        );
        object.insert(
            "drain_budget_hits".to_string(),
            window.drain_budget_hits.into(),
        );
    }
    payload
}

async fn maybe_emit_attach_perf_window(
    perf_emitter: &mut recording::PerfEventEmitter,
    client: &mut StreamingBmuxClient,
    session_id: Uuid,
    window: &mut AttachPerfWindow,
) -> Result<()> {
    let elapsed = window.started_at.elapsed();
    if elapsed < Duration::from_millis(perf_emitter.window_ms()) {
        return Ok(());
    }

    if window.render_frames >= ATTACH_OVERRENDER_MIN_FRAMES
        && percent_at_least(
            window.overrender_flagged_frames,
            window.render_frames,
            ATTACH_OVERRENDER_WINDOW_RATIO_PERCENT,
        )
    {
        tracing::warn!(
            session_id = %session_id,
            render_frames = window.render_frames,
            overrender_flagged_frames = window.overrender_flagged_frames,
            dirty_no_visible_row_change_frames = window.dirty_no_visible_row_change_frames,
            high_cached_skip_ratio_frames = window.high_cached_skip_ratio_frames,
            large_partial_frame_frames = window.large_partial_frame_frames,
            extension_full_surface_excessive_frames = window.extension_full_surface_excessive_frames,
            full_frame_fallback_flagged_frames = window.full_frame_fallback_flagged_frames,
            extension_imperative_or_cache_miss_frames = window.extension_imperative_or_cache_miss_frames,
            status_overlay_only_emits_pane_work_frames = window.status_overlay_only_emits_pane_work_frames,
            slow_terminal_write_per_kib_frames = window.slow_terminal_write_per_kib_frames,
            "attach.render.overrender.window"
        );
    }

    tracing::info!(
        session_id = %session_id,
        window_elapsed_ms = duration_millis_u64(elapsed),
        drain_rounds = window.drain_rounds,
        drain_ipc_calls = window.drain_ipc_calls,
        drain_bytes = window.drain_bytes,
        drain_ipc_ms_max = window.drain_ipc_ms_max,
        render_frames = window.render_frames,
        render_ms_max = window.render_ms_max,
        frame_bytes_max = window.frame_bytes_max,
        terminal_write_ms_max = window.terminal_write_ms_max,
        damage_rects_max = window.damage_rects_max,
        damage_area_cells_max = window.damage_area_cells_max,
        full_surface_fallbacks = window.full_surface_fallbacks,
        full_frame_fallbacks = window.full_frame_fallbacks,
        drain_budget_hits = window.drain_budget_hits,
        event_burst_drained_events = window.event_burst_drained_events,
        event_burst_max_events = window.event_burst_max_events,
        wake_server_events = window.wake_server_events,
        wake_terminal_events = window.wake_terminal_events,
        wake_prompt_events = window.wake_prompt_events,
        wake_action_dispatch_events = window.wake_action_dispatch_events,
        wake_appearance_events = window.wake_appearance_events,
        wake_scene_events = window.wake_scene_events,
        wake_window_list_events = window.wake_window_list_events,
        dirty_status_frames = window.dirty_status_frames,
        dirty_full_pane_frames = window.dirty_full_pane_frames,
        dirty_overlay_frames = window.dirty_overlay_frames,
        dirty_pane_frames = window.dirty_pane_frames,
        dirty_layout_frames = window.dirty_layout_frames,
        dirty_scene_hydrated_frames = window.dirty_scene_hydrated_frames,
        dirty_extension_frames = window.dirty_extension_frames,
        "attach.metrics.window"
    );

    if !perf_emitter.enabled() {
        window.reset();
        return Ok(());
    }

    let payload = attach_perf_window_payload(
        window,
        elapsed,
        perf_emitter.level_at_least(recording::PerfCaptureLevel::Detailed),
        perf_emitter.level_at_least(recording::PerfCaptureLevel::Trace),
    );
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

    tracing::info!(
        target = target.unwrap_or(""),
        follow = follow.unwrap_or(""),
        global,
        has_kernel_client_factory = kernel_client_factory.is_some(),
        "attach.runtime.start"
    );

    // Install client-side render extensions for any bundled plugins
    // that ship one. Each crate's `install` is idempotent; process-
    // wide state is initialised on first call. Extensions populate
    // themselves lazily as scene events arrive.
    install_bundled_render_extensions();

    let follow_target_id = match follow {
        Some(follow_target) => Some(parse_uuid_value(follow_target, "follow target client id")?),
        None => None,
    };

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
    if let Ok(settings) = performance_state::client::get_settings(&mut client).await {
        perf_emitter.update_settings(recording::PerfCaptureSettings::from_plugin_settings(
            &settings,
        ));
    }
    let mut perf_window = AttachPerfWindow::new();
    let attach_started_at = Instant::now();
    let mut rendered_frame_count = 0_u64;
    let mut first_frame_emitted = false;
    let mut interactive_ready_emitted = false;
    let (initial_appearance, appearance_rx_raw) = bmux_plugin::global_event_bus()
        .subscribe_state::<RuntimeAppearance>(&RUNTIME_APPEARANCE_STATE_KIND)
        .unwrap_or_else(|_| {
            let _ = bmux_plugin::global_event_bus().register_state_channel::<RuntimeAppearance>(
                RUNTIME_APPEARANCE_STATE_KIND,
                RuntimeAppearance::default(),
            );
            bmux_plugin::global_event_bus()
                .subscribe_state::<RuntimeAppearance>(&RUNTIME_APPEARANCE_STATE_KIND)
                .expect("runtime appearance state channel was just registered")
        });
    let mut appearance_rx = Some(appearance_rx_raw);
    let mut runtime_appearance = (*initial_appearance).clone();

    if let Some(leader_client_id) = follow_target_id {
        client
            .subscribe_events()
            .await
            .map_err(map_attach_client_error)?;
        clients_state::client::current_client(&mut client)
            .await?
            .map_err(|err| anyhow::anyhow!("clients-state current-client failed: {err:?}"))?;
        bmux_clients_plugin_api::clients_commands::client::set_following(
            &mut client,
            Some(leader_client_id),
            global,
        )
        .await?
        .map_err(|err| anyhow::anyhow!("clients-commands set-following failed: {err:?}"))?;
    }

    tracing::info!("attach.runtime.whoami_start");
    let self_client_id = clients_state::client::current_client(&mut client)
        .await?
        .map_err(|err| anyhow::anyhow!("clients-state current-client failed: {err:?}"))?
        .id;
    crate::runtime::set_logging_client_id(self_client_id.to_string());
    tracing::info!(client_id = %self_client_id, "attach.client_log.initialized");

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

    let capture_targets = match recording_state::client::capture_targets(&mut client).await {
        Ok(targets) => targets.into_iter().map(Into::into).collect(),
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

    if let Ok(appearance) = typed_active_runtime_appearance_attach(&mut client).await {
        runtime_appearance = appearance.clone();
        let _ = bmux_plugin::global_event_bus()
            .publish_state(&RUNTIME_APPEARANCE_STATE_KIND, appearance);
    }

    let mut display_capture = DisplayCaptureFanout::default();
    for target in &capture_targets {
        display_capture.open_target(target, self_client_id);
    }

    let mut view_state = AttachViewState::new(attach_info);
    view_state.self_client_id = Some(self_client_id);
    view_state.mouse.config = attach_config.attach_mouse_config();
    view_state.mouse.tab_drag_enabled = status_tab_drag_enabled(&attach_config.status_bar);
    view_state.status_position = if attach_config.status_bar.enabled {
        attach_config.appearance.status_position
    } else {
        StatusPosition::Off
    };
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
    let initial_layout_pane_ids =
        notify_extensions_of_layout(&mut client, None, view_state.cached_layout_state.as_ref())
            .await;
    if let Ok(scene) =
        hydrate_decoration_scene_snapshot_attach(&mut client, !initial_layout_pane_ids.is_empty())
            .await
    {
        publish_decoration_scene_locally(scene);
        view_state
            .dirty
            .mark_extension_dirty(AttachDirtySource::SceneChanged);
    }
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

    // Subscribe to the generic retained scene-protocol state so the attach
    // runtime can mark the frame dirty whenever a new scene arrives. Render
    // extensions subscribe to the same channel independently. When no plugin
    // has registered the channel the subscription returns `Err` and we run
    // without scene updates — extensions that aren't loaded simply have
    // nothing to draw.
    let (mut last_scene_revision, mut scene_event_rx) =
        match bmux_plugin::global_event_bus()
            .subscribe_state::<bmux_scene_protocol::scene_protocol::DecorationScene>(
                &bmux_scene_protocol::scene_protocol::STATE_KIND,
            ) {
            Ok((initial, rx)) => (initial.revision, Some(rx)),
            Err(_) => (0, None),
        };

    // Subscribe to the windows-plugin `windows-list` state channel so
    // the attach tab bar can render the authoritative plugin-owned
    // window order without polling. Seeds `view_state.cached_window_list`
    // synchronously with the current value (if any), then drives live
    // updates through a `tokio::select!` arm below. When the plugin
    // has not registered the channel (plugin absent / not yet
    // activated), the tab bar falls back to `cached_contexts` in raw
    // server order — baseline behavior per AGENTS.md.
    let (mut last_window_list_revision, mut window_list_rx) =
        match bmux_plugin::global_event_bus()
            .subscribe_state::<bmux_windows_plugin_api::windows_list::WindowListSnapshot>(
            &bmux_windows_plugin_api::windows_list::STATE_KIND,
        ) {
            Ok((initial, rx)) => {
                let revision = initial.revision;
                view_state.cached_window_list = Some(initial);
                (revision, Some(rx))
            }
            Err(_) => (0, None),
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
        last_window_list_revision = snapshot.revision;
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
                perf_window.record_wake(AttachWakeSource::Server);

                let handling = handle_attach_stream_server_event(
                    server_event,
                    &mut client,
                    &mut attach_input_processor,
                    follow_target_id,
                    self_client_id,
                    global,
                    &attach_help_lines,
                    &mut view_state,
                    &mut display_capture,
                    kernel_client_factory.as_ref(),
                    &attach_config,
                    &mut perf_emitter,
                    &mut pane_output_pending,
                    &mut last_scene_revision,
                )
                .await?;
                if handling.image_fetch_requested {
                    #[cfg(any(
                        feature = "image-sixel",
                        feature = "image-kitty",
                        feature = "image-iterm2"
                    ))]
                    {
                        image_fetch_pending = true;
                    }
                }
                match handling.control {
                    AttachLoopControl::Continue => {}
                    AttachLoopControl::Break(reason) => {
                        exit_reason = reason;
                        break;
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
                let terminal_event = AttachTerminalEvent::from_crossterm(
                    result.context("failed reading terminal event")?,
                    SystemAttachClock.now(),
                    current_attach_terminal_geometry(),
                );
                perf_window.record_wake(AttachWakeSource::Terminal);

                if let Some(TerminalInputEvent::Resize { cols, rows }) = &terminal_event.normalized {
                    display_capture.record_resize(*cols, *rows);
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
                    perf_window.record_wake(AttachWakeSource::Prompt);
                    view_state.prompt.enqueue_external(prompt_request);
                    view_state
                        .dirty
                        .mark_status_dirty(AttachDirtySource::PromptOverlay);
                    view_state
                        .dirty
                        .mark_overlay_dirty(AttachDirtySource::PromptOverlay);
                }
            }

            dispatch_request = action_dispatch_rx.recv() => {
                if let Some(dispatch_request) = dispatch_request {
                    perf_window.record_wake(AttachWakeSource::ActionDispatch);
                    match handle_attach_loop_event(
                        AttachLoopEvent::ActionDispatch(dispatch_request),
                        &mut client,
                        &mut attach_input_processor,
                        follow_target_id,
                        Some(self_client_id),
                        global,
                        &attach_help_lines,
                        &mut view_state,
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

            appearance_result = async {
                match &mut appearance_rx {
                    Some(rx) => rx.changed().await.ok().map(|()| rx.borrow().clone()),
                    None => std::future::pending().await,
                }
            } => {
                if let Some(appearance) = appearance_result {
                    perf_window.record_wake(AttachWakeSource::Appearance);
                    if runtime_appearance != *appearance {
                        runtime_appearance = (*appearance).clone();
                        view_state.cached_status_line = None;
                        view_state
                            .dirty
                            .mark_status_dirty(AttachDirtySource::AppearanceChanged);
                        view_state
                            .dirty
                            .mark_full_frame(AttachDirtySource::AppearanceChanged);
                        view_state
                            .dirty
                            .mark_overlay_dirty(AttachDirtySource::AppearanceChanged);
                    }
                } else {
                    appearance_rx = None;
                }
            },

            // Scene state pushed on the local event bus. The PluginBusEvent handler
            // above publishes incoming scenes onto the same retained channel; render
            // extensions subscribe independently. Core observes changes only to mark
            // the frame dirty so the renderer consults extensions on the next pass.
            scene_result = async {
                match &mut scene_event_rx {
                    Some(rx) => rx.changed().await.ok().map(|()| rx.borrow().clone()),
                    None => std::future::pending().await,
                }
            } => {
                if let Some(scene) = scene_result {
                    perf_window.record_wake(AttachWakeSource::Scene);
                    if scene.revision != last_scene_revision {
                        last_scene_revision = scene.revision;
                        view_state
                            .dirty
                            .mark_extension_dirty(AttachDirtySource::SceneChanged);
                    }
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
                    perf_window.record_wake(AttachWakeSource::WindowList);
                    if snapshot.revision != last_window_list_revision {
                        last_window_list_revision = snapshot.revision;
                        view_state.cached_window_list = Some(snapshot);
                        view_state
                            .dirty
                            .mark_status_dirty(AttachDirtySource::StatusChanged);
                    }
                } else {
                    // Channel closed — drop subscription; fallback
                    // path in build_attach_tabs_from_catalog covers
                    // the plugin-absent case.
                    window_list_rx = None;
                }
            }

        }

        let mut burst_requested_exit = false;
        let mut burst_drained_events = 0_usize;
        for _ in 0..attach_config.behavior.event_coalescing.max_events_per_frame {
            let server_event = match client.event_receiver().try_recv() {
                Ok(server_event) => server_event,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    exit_reason = AttachExitReason::StreamClosed;
                    burst_requested_exit = true;
                    break;
                }
            };
            burst_drained_events = burst_drained_events.saturating_add(1);
            perf_window.record_wake(AttachWakeSource::Server);
            let handling = handle_attach_stream_server_event(
                server_event,
                &mut client,
                &mut attach_input_processor,
                follow_target_id,
                self_client_id,
                global,
                &attach_help_lines,
                &mut view_state,
                &mut display_capture,
                kernel_client_factory.as_ref(),
                &attach_config,
                &mut perf_emitter,
                &mut pane_output_pending,
                &mut last_scene_revision,
            )
            .await?;
            if handling.image_fetch_requested {
                #[cfg(any(
                    feature = "image-sixel",
                    feature = "image-kitty",
                    feature = "image-iterm2"
                ))]
                {
                    image_fetch_pending = true;
                }
            }
            match handling.control {
                AttachLoopControl::Continue => {}
                AttachLoopControl::Break(reason) => {
                    exit_reason = reason;
                    burst_requested_exit = true;
                    break;
                }
            }
        }
        perf_window.record_event_burst(burst_drained_events);
        if burst_requested_exit {
            break;
        }

        // ── Post-event processing: layout, output fetch, render ──────

        let _ = view_state.clear_expired_transient_status(Instant::now());

        let dirty_layout_frame =
            view_state.dirty.layout_needs_refresh || view_state.cached_layout_state.is_none();
        let mut frame_needs_render = view_state.dirty.needs_render();

        let mut scene_hydrated = false;

        if dirty_layout_frame {
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
                match previous_layout.as_ref() {
                    None => {
                        view_state
                            .dirty
                            .mark_full_frame(AttachDirtySource::LayoutChanged);
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
                                    let revealed =
                                        revealed_pane_ids.into_iter().collect::<Vec<_>>();
                                    if hydrate_attach_revealed_panes_from_snapshot(
                                        &mut client,
                                        &mut view_state,
                                        &layout_state,
                                        &revealed,
                                    )
                                    .await
                                    .is_err()
                                    {
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
                                let damage_policy = DamageCoalescingPolicy {
                                    max_rects: attach_config.behavior.damage.max_rects,
                                    max_area_percent: attach_config
                                        .behavior
                                        .damage
                                        .max_area_percent,
                                };
                                let scene_damage = attach_scene_damage_between(
                                    &previous.scene,
                                    &layout_state.scene,
                                    damage_policy,
                                );
                                if scene_damage.is_empty() {
                                    view_state
                                        .dirty
                                        .mark_extension_dirty(AttachDirtySource::LayoutChanged);
                                } else {
                                    view_state.dirty.merge_precise_damage(
                                        &scene_damage,
                                        AttachDirtySource::LayoutChanged,
                                    );
                                }
                            }
                        } else if previous.focused_pane_id != layout_state.focused_pane_id {
                            view_state.dirty.mark_pane_dirty(
                                previous.focused_pane_id,
                                AttachDirtySource::FocusChanged,
                            );
                            view_state.dirty.mark_pane_dirty(
                                layout_state.focused_pane_id,
                                AttachDirtySource::FocusChanged,
                            );
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
            perf_window.record_dirty_reasons(AttachDirtyReasons::from_view_state(
                &view_state,
                dirty_layout_frame,
                true,
            ));
            let help_scroll = view_state.help_overlay_scroll;
            let render_started_at = SystemAttachClock.now();
            let render_geometry = current_attach_terminal_geometry();
            let frame_stats = render_attach_frame(
                &mut client,
                &mut view_state,
                &layout_state,
                &attach_config.status_bar,
                &runtime_appearance,
                follow_target_id,
                global,
                &attach_keymap,
                &attach_help_lines,
                help_scroll,
                &attach_config.behavior.damage,
                attach_config.logs.client.slow_terminal_write_ms,
                &mut display_capture,
                render_started_at,
                render_geometry,
                None,
            )?;
            let render_ms = duration_millis_u64(render_started_at.elapsed());
            if render_ms >= attach_config.logs.client.slow_frame_ms {
                tracing::warn!(
                    frame_index = rendered_frame_count.saturating_add(1),
                    render_ms,
                    threshold_ms = attach_config.logs.client.slow_frame_ms,
                    "attach.render.slow_frame"
                );
            }
            perf_window.record_render_frame(render_ms, frame_stats);
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

        resize_attach_grids_for_scene(&mut view_state.pane_buffers, &layout_state.scene);

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
            let mut drained_any_data = false;
            let mut recovered_output_desync = false;
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
                    let pane_id = chunk.pane_id;
                    let sync_update_active = chunk.sync_update_active;
                    match apply_attach_output_chunk(
                        &mut view_state,
                        pane_id,
                        &chunk.data,
                        AttachOutputChunkMeta {
                            stream_start: chunk.stream_start,
                            stream_end: chunk.stream_end,
                            stream_gap: chunk.stream_gap,
                            sync_update_active,
                        },
                        &mut frame_needs_render,
                    ) {
                        AttachChunkApplyOutcome::Applied {
                            had_data: chunk_had_data,
                        } => {
                            had_data |= chunk_had_data;
                            drained_any_data |= chunk_had_data;
                            any_sync_active |= sync_update_active;
                        }
                        AttachChunkApplyOutcome::Stale => {}
                        AttachChunkApplyOutcome::Desync => {
                            desynced_pane_id = Some(pane_id);
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
                    recovered_output_desync = true;
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
            if !recovered_output_desync && !drained_any_data {
                let structured_delta_applied = apply_attach_structured_grid_deltas(
                    &mut client,
                    &mut view_state,
                    pane_ids.clone(),
                )
                .await?;
                frame_needs_render |= structured_delta_applied;
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

        perf_window.record_dirty_reasons(AttachDirtyReasons::from_view_state(
            &view_state,
            dirty_layout_frame,
            false,
        ));
        let help_scroll = view_state.help_overlay_scroll;
        let render_started_at = SystemAttachClock.now();
        let render_geometry = current_attach_terminal_geometry();
        let frame_stats = render_attach_frame(
            &mut client,
            &mut view_state,
            &layout_state,
            &attach_config.status_bar,
            &runtime_appearance,
            follow_target_id,
            global,
            &attach_keymap,
            &attach_help_lines,
            help_scroll,
            &attach_config.behavior.damage,
            attach_config.logs.client.slow_terminal_write_ms,
            &mut display_capture,
            render_started_at,
            render_geometry,
            None,
        )?;
        let render_ms = duration_millis_u64(render_started_at.elapsed());
        if render_ms >= attach_config.logs.client.slow_frame_ms {
            tracing::warn!(
                frame_index = rendered_frame_count.saturating_add(1),
                render_ms,
                threshold_ms = attach_config.logs.client.slow_frame_ms,
                "attach.render.slow_frame"
            );
        }
        perf_window.record_render_frame(render_ms, frame_stats);
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

    // Stop the crossterm event stream before terminal restoration and async
    // runtime shutdown. Leaving it alive until the end of the function can keep
    // its terminal reader task around after a normal detach under pseudo-TTY
    // harnesses such as `script(1)`.
    drop(terminal_stream);

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

    tracing::info!(
        exit_reason = attach_exit_reason_label(exit_reason),
        attach_runtime_ms = duration_millis_u64(attach_started_at.elapsed()),
        rendered_frames = rendered_frame_count,
        "attach.runtime.end"
    );

    drop(raw_mode_guard);
    restore_terminal_after_attach_ui()?;

    if exit_reason != AttachExitReason::Detached {
        let _ = client.detach().await;
    }
    if follow_target_id.is_some() {
        let _ = bmux_clients_plugin_api::clients_commands::client::set_following(
            &mut client,
            None,
            false,
        )
        .await;
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

struct AttachServerEventHandling {
    control: AttachLoopControl,
    image_fetch_requested: bool,
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn handle_attach_stream_server_event(
    server_event: bmux_client::ServerEvent,
    client: &mut StreamingBmuxClient,
    attach_input_processor: &mut InputProcessor,
    follow_target_id: Option<Uuid>,
    self_client_id: Uuid,
    global: bool,
    attach_help_lines: &[String],
    view_state: &mut AttachViewState,
    display_capture: &mut DisplayCaptureFanout,
    kernel_client_factory: Option<&KernelClientFactory>,
    attach_config: &BmuxConfig,
    perf_emitter: &mut recording::PerfEventEmitter,
    pane_output_pending: &mut bool,
    last_scene_revision: &mut u64,
) -> Result<AttachServerEventHandling> {
    let mut image_fetch_requested = false;

    if let bmux_client::ServerEvent::PluginBusEvent {
        ref kind,
        ref payload,
    } = server_event
    {
        if kind.as_str() == bmux_performance_plugin_api::EVENT_KIND.as_str() {
            match serde_json::from_slice::<bmux_performance_plugin_api::PerformanceEvent>(payload) {
                Ok(bmux_performance_plugin_api::PerformanceEvent::SettingsUpdated { settings }) => {
                    perf_emitter.update_settings(
                        recording::PerfCaptureSettings::from_plugin_settings(&settings),
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        kind = %kind,
                        error = %error,
                        "decoding forwarded performance event payload",
                    );
                }
            }
        } else if kind.as_str() == bmux_recording_plugin_api::recording_events::EVENT_KIND.as_str()
        {
            match serde_json::from_slice::<
                bmux_recording_plugin_api::recording_events::RecordingEvent,
            >(payload)
            {
                Ok(bmux_recording_plugin_api::recording_events::RecordingEvent::Started {
                    target,
                }) => {
                    let target = RecordingCaptureTarget {
                        recording_id: target.recording_id,
                        path: target.path,
                        rolling_window_secs: target.rolling_window_secs,
                    };
                    display_capture.open_target(&target, self_client_id);
                }
                Ok(bmux_recording_plugin_api::recording_events::RecordingEvent::Stopped {
                    recording_id,
                }) => display_capture.close_recording(recording_id),
                Ok(bmux_recording_plugin_api::recording_events::RecordingEvent::CutStarted {
                    ..
                }) => {
                    view_state.set_transient_status(
                        "recording cut started".to_string(),
                        Instant::now(),
                        ATTACH_TRANSIENT_STATUS_TTL,
                    );
                    view_state
                        .dirty
                        .mark_status_dirty(AttachDirtySource::PluginCommand);
                }
                Ok(bmux_recording_plugin_api::recording_events::RecordingEvent::CutCompleted {
                    summary,
                }) => {
                    view_state.set_transient_status(
                        format!("recording cut created: {}", summary.path),
                        Instant::now(),
                        ATTACH_TRANSIENT_STATUS_TTL,
                    );
                    view_state
                        .dirty
                        .mark_status_dirty(AttachDirtySource::PluginCommand);
                }
                Ok(bmux_recording_plugin_api::recording_events::RecordingEvent::CutFailed {
                    reason,
                }) => {
                    view_state.set_transient_status(
                        format!("recording cut failed: {reason}"),
                        Instant::now(),
                        ATTACH_TRANSIENT_STATUS_TTL,
                    );
                    view_state
                        .dirty
                        .mark_status_dirty(AttachDirtySource::PluginCommand);
                }
                Ok(
                    bmux_recording_plugin_api::recording_events::RecordingEvent::ExportStarted {
                        output_path,
                        ..
                    },
                ) => {
                    view_state.set_transient_status(
                        format!("recording GIF export started: {output_path}"),
                        Instant::now(),
                        ATTACH_TRANSIENT_STATUS_TTL,
                    );
                    view_state
                        .dirty
                        .mark_status_dirty(AttachDirtySource::PluginCommand);
                }
                Ok(
                    bmux_recording_plugin_api::recording_events::RecordingEvent::ExportCompleted {
                        output_path,
                        ..
                    },
                ) => {
                    view_state.set_transient_status(
                        format!("recording GIF exported: {output_path}"),
                        Instant::now(),
                        ATTACH_TRANSIENT_STATUS_TTL,
                    );
                    view_state
                        .dirty
                        .mark_status_dirty(AttachDirtySource::PluginCommand);
                }
                Ok(bmux_recording_plugin_api::recording_events::RecordingEvent::ExportFailed {
                    reason,
                    ..
                }) => {
                    view_state.set_transient_status(
                        format!("recording GIF export failed: {reason}"),
                        Instant::now(),
                        ATTACH_TRANSIENT_STATUS_TTL,
                    );
                    view_state
                        .dirty
                        .mark_status_dirty(AttachDirtySource::PluginCommand);
                }
                Err(error) => {
                    tracing::warn!(
                        kind = %kind,
                        error = %error,
                        "decoding forwarded recording event payload",
                    );
                }
            }
        } else if kind.as_str() == pane_events::EVENT_KIND.as_str() {
            match serde_json::from_slice::<pane_events::PaneEvent>(payload) {
                Ok(event) => {
                    let outcome =
                        handle_pane_runtime_plugin_event(event, view_state, pane_output_pending);
                    image_fetch_requested |= outcome.image_fetch_requested;
                }
                Err(error) => {
                    tracing::warn!(
                        kind = %kind,
                        error = %error,
                        "decoding forwarded pane-runtime event payload",
                    );
                }
            }
        } else if kind.as_str() == bmux_sessions_plugin_api::sessions_events::EVENT_KIND.as_str() {
            match serde_json::from_slice::<bmux_sessions_plugin_api::sessions_events::SessionEvent>(
                payload,
            ) {
                Ok(bmux_sessions_plugin_api::sessions_events::SessionEvent::Removed {
                    session_id,
                }) => {
                    if let Some(control) =
                        handle_attached_session_removed(client, view_state, session_id).await?
                    {
                        return Ok(AttachServerEventHandling {
                            control,
                            image_fetch_requested,
                        });
                    }
                }
                Ok(bmux_sessions_plugin_api::sessions_events::SessionEvent::Renamed { .. }) => {
                    refresh_attach_status_catalog_best_effort(client, view_state).await;
                    view_state
                        .dirty
                        .mark_status_dirty(AttachDirtySource::StatusChanged);
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(
                        kind = %kind,
                        error = %error,
                        "decoding forwarded sessions event payload",
                    );
                }
            }
        } else if kind.as_str() == bmux_clients_plugin_api::clients_events::EVENT_KIND.as_str() {
            match serde_json::from_slice::<bmux_clients_plugin_api::clients_events::ClientEvent>(
                payload,
            ) {
                Ok(event) => {
                    handle_clients_plugin_event(
                        client,
                        follow_target_id,
                        Some(self_client_id),
                        view_state,
                        event,
                    )
                    .await?;
                }
                Err(error) => {
                    tracing::warn!(
                        kind = %kind,
                        error = %error,
                        "decoding forwarded clients event payload",
                    );
                }
            }
        } else if kind.as_str()
            == bmux_control_catalog_plugin_api::control_catalog_events::EVENT_KIND.as_str()
        {
            match serde_json::from_slice::<
                bmux_control_catalog_plugin_api::control_catalog_events::CatalogEvent,
            >(payload)
            {
                Ok(bmux_control_catalog_plugin_api::control_catalog_events::CatalogEvent::Changed {
                    revision,
                    full_resync,
                    ..
                }) => {
                    handle_control_catalog_changed(client, view_state, revision, full_resync)
                        .await;
                }
                Err(error) => {
                    tracing::warn!(
                        kind = %kind,
                        error = %error,
                        "decoding forwarded control-catalog event payload",
                    );
                }
            }
        } else if kind.as_str() == bmux_scene_protocol::scene_protocol::STATE_KIND.as_str() {
            match serde_json::from_slice::<bmux_scene_protocol::scene_protocol::DecorationScene>(
                payload,
            ) {
                Ok(scene) => {
                    let scene_revision = scene.revision;
                    let _ = bmux_plugin::global_event_bus()
                        .publish_state(&bmux_scene_protocol::scene_protocol::STATE_KIND, scene);
                    if scene_revision != *last_scene_revision {
                        *last_scene_revision = scene_revision;
                        view_state
                            .dirty
                            .mark_extension_dirty(AttachDirtySource::SceneChanged);
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        kind = %kind,
                        error = %error,
                        "decoding forwarded scene-protocol payload",
                    );
                }
            }
        } else if kind.as_str() == bmux_contexts_plugin_api::contexts_events::EVENT_KIND.as_str() {
            match serde_json::from_slice::<bmux_contexts_plugin_api::contexts_events::ContextEvent>(
                payload,
            ) {
                Ok(event) => {
                    handle_context_event_forwarded(
                        client,
                        view_state,
                        &event,
                        self_client_id,
                        attach_config,
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
        } else if kind.as_str() == bmux_windows_plugin_api::windows_list::STATE_KIND.as_str() {
            match serde_json::from_slice::<bmux_windows_plugin_api::windows_list::WindowListSnapshot>(
                payload,
            ) {
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
        } else if kind.as_str() == RUNTIME_APPEARANCE_STATE_KIND.as_str() {
            match serde_json::from_slice::<RuntimeAppearance>(payload) {
                Ok(appearance) => {
                    let _ = bmux_plugin::global_event_bus()
                        .publish_state(&RUNTIME_APPEARANCE_STATE_KIND, appearance);
                    view_state
                        .dirty
                        .mark_full_frame(AttachDirtySource::AppearanceChanged);
                }
                Err(error) => {
                    tracing::warn!(
                        kind = %kind,
                        error = %error,
                        "decoding forwarded runtime appearance payload",
                    );
                }
            }
        }
    } else {
        let control = handle_attach_loop_event(
            AttachLoopEvent::Server(server_event),
            client,
            attach_input_processor,
            follow_target_id,
            Some(self_client_id),
            global,
            attach_help_lines,
            view_state,
            display_capture,
            kernel_client_factory,
        )
        .await?;
        return Ok(AttachServerEventHandling {
            control,
            image_fetch_requested,
        });
    }

    Ok(AttachServerEventHandling {
        control: AttachLoopControl::Continue,
        image_fetch_requested,
    })
}

#[derive(Debug, Clone, Copy, Default)]
struct PaneRuntimePluginEventOutcome {
    image_fetch_requested: bool,
}

fn handle_pane_runtime_plugin_event(
    event: pane_events::PaneEvent,
    view_state: &mut AttachViewState,
    pane_output_pending: &mut bool,
) -> PaneRuntimePluginEventOutcome {
    match event {
        pane_events::PaneEvent::OutputAvailable { .. } => {
            *pane_output_pending = true;
            PaneRuntimePluginEventOutcome::default()
        }
        pane_events::PaneEvent::ImageAvailable { .. } => PaneRuntimePluginEventOutcome {
            image_fetch_requested: true,
        },
        pane_events::PaneEvent::AttachViewChanged {
            context_id,
            session_id,
            components,
            ..
        } if attach_view_event_matches_target(view_state, context_id, session_id) => {
            let components = components
                .iter()
                .copied()
                .map(plugin_attach_view_component)
                .collect::<Vec<_>>();
            apply_attach_view_change_components(&components, view_state);
            *pane_output_pending = true;
            PaneRuntimePluginEventOutcome::default()
        }
        pane_events::PaneEvent::Exited {
            session_id,
            pane_id,
            reason,
        } if session_id == view_state.attached_id => {
            let message = reason.map_or_else(
                || format!("pane {} exited", short_uuid(pane_id)),
                |reason| format!("pane {} exited: {reason}", short_uuid(pane_id)),
            );
            view_state.set_transient_status(message, Instant::now(), ATTACH_TRANSIENT_STATUS_TTL);
            view_state
                .dirty
                .mark_status_dirty(AttachDirtySource::PaneLifecycle);
            PaneRuntimePluginEventOutcome::default()
        }
        pane_events::PaneEvent::Restarted {
            session_id,
            pane_id,
        } if session_id == view_state.attached_id => {
            view_state.set_transient_status(
                format!("pane {} restarted", short_uuid(pane_id)),
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
            view_state
                .dirty
                .mark_status_dirty(AttachDirtySource::PaneLifecycle);
            PaneRuntimePluginEventOutcome::default()
        }
        _ => PaneRuntimePluginEventOutcome::default(),
    }
}

const fn plugin_attach_view_component(
    component: pane_events::AttachViewComponent,
) -> AttachViewComponent {
    match component {
        pane_events::AttachViewComponent::Scene => AttachViewComponent::Scene,
        pane_events::AttachViewComponent::SurfaceContent => AttachViewComponent::SurfaceContent,
        pane_events::AttachViewComponent::Layout => AttachViewComponent::Layout,
        pane_events::AttachViewComponent::Status => AttachViewComponent::Status,
    }
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
/// need conditional compilation; it returns whether the outcome changed attach state.
pub async fn apply_plugin_command_outcome(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    outcome: PluginCommandOutcome,
) -> std::result::Result<bool, ClientError> {
    let Some(context_id) = selected_context_id_from_command_outcome(&outcome) else {
        return Ok(false);
    };
    if view_state.attached_context_id == Some(context_id) {
        return Ok(true);
    }
    retarget_attach_to_context(client, view_state, context_id).await?;
    Ok(true)
}

fn selected_context_id_from_command_outcome(outcome: &PluginCommandOutcome) -> Option<Uuid> {
    let value = outcome
        .metadata
        .get(COMMAND_OUTCOME_SELECTED_CONTEXT_ID_KEY)?;
    let context_id = value.as_str().and_then(|value| Uuid::parse_str(value).ok());
    if context_id.is_none() {
        warn!(
            value = ?value,
            "attach.plugin_command.invalid_selected_context_outcome"
        );
    }
    context_id
}

fn status_message_from_command_outcome(outcome: &PluginCommandOutcome) -> Option<String> {
    let value = outcome.metadata.get(COMMAND_OUTCOME_STATUS_MESSAGE_KEY)?;
    let Some(message) = value.as_str() else {
        warn!(
            value = ?value,
            "attach.plugin_command.invalid_status_message_outcome"
        );
        return None;
    };
    Some(truncate_attach_status_message(message))
}

fn truncate_attach_status_message(message: &str) -> String {
    let mut chars = message.chars();
    let truncated = chars
        .by_ref()
        .take(ATTACH_PLUGIN_STATUS_MESSAGE_MAX_CHARS)
        .collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
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
            view_state
                .dirty
                .mark_layout_frame_dirty(AttachDirtySource::SceneChanged);
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
            view_state
                .dirty
                .mark_layout_frame_dirty(AttachDirtySource::SceneChanged);
        }
        ContextEvent::Renamed { .. } => {
            refresh_attach_status_catalog_best_effort(client, view_state).await;
            view_state
                .dirty
                .mark_status_dirty(AttachDirtySource::StatusChanged);
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
    let from_context_id = view_state.attached_context_id;
    let from_session_id = view_state.attached_id;
    debug!(
        from_context_id = ?from_context_id,
        from_session_id = %from_session_id,
        to_context_id = %context_id,
        "attach.retarget.start"
    );
    let retarget_service_started = Instant::now();
    let geometry = current_attach_terminal_geometry();
    let (status_top_inset, status_bottom_inset) =
        status_insets_for_position(view_state.status_position);
    let attach_info = client
        .retarget_attach_context_with_insets(
            context_id,
            geometry.cols,
            geometry.rows,
            status_top_inset,
            status_bottom_inset,
        )
        .await?;
    let retarget_service_us = retarget_service_started.elapsed().as_micros();
    let select_us = 0_u128;
    let open_us = retarget_service_us;
    view_state.attached_id = attach_info.session_id;
    view_state.attached_context_id = attach_info.context_id.or(Some(context_id));
    view_state.can_write = attach_info.can_write;
    let viewport_us = 0_u128;
    let hydrate_started = Instant::now();
    hydrate_attach_state_from_snapshot(client, view_state).await?;
    let hydrate_us = hydrate_started.elapsed().as_micros();
    let catalog_started = Instant::now();
    refresh_attach_status_catalog_best_effort(client, view_state).await;
    let catalog_us = catalog_started.elapsed().as_micros();
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
    emit_attach_phase_timing(&serde_json::json!({
        "phase": "attach.retarget_context",
        "from_context_id": from_context_id,
        "from_session_id": from_session_id,
        "to_context_id": context_id,
        "selected_context_id": view_state.attached_context_id,
        "selected_session_id": view_state.attached_id,
        "retarget_service_us": retarget_service_us,
        "select_us": select_us,
        "open_us": open_us,
        "viewport_us": viewport_us,
        "hydrate_us": hydrate_us,
        "catalog_us": catalog_us,
        "total_us": started_at.elapsed().as_micros(),
    }));
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

fn build_attach_plugin_command_pipeline(
    plugin_id: &str,
    command_name: &str,
    args: &[String],
) -> std::result::Result<bmux_ipc::ServicePipelineRequest, ClientError> {
    let request = bmux_plugin_sdk::PluginCliCommandRequest::new(
        plugin_id.to_string(),
        command_name.to_string(),
        args.to_vec(),
    );
    let command_payload = bmux_plugin_sdk::encode_service_message(&request).map_err(|error| {
        ClientError::ServerError {
            code: bmux_ipc::ErrorCode::Internal,
            message: format!("encoding plugin command pipeline request: {error}"),
        }
    })?;
    Ok(bmux_ipc::ServicePipelineRequest {
        inputs: BTreeMap::new(),
        steps: vec![bmux_ipc::ServicePipelineStep {
            capability: bmux_plugin_sdk::CORE_CLI_COMMAND_CAPABILITY.to_string(),
            kind: InvokeServiceKind::Command,
            interface_id: bmux_plugin_sdk::CORE_CLI_COMMAND_INTERFACE_V1.to_string(),
            operation: bmux_plugin_sdk::CORE_CLI_COMMAND_RUN_PLUGIN_OPERATION_V1.to_string(),
            payload: bmux_ipc::ServicePipelinePayload::Encoded {
                payload: command_payload,
            },
        }],
    })
}

struct AttachPluginCommandPipelineExecution {
    status: i32,
    outcome: PluginCommandOutcome,
}

fn decode_attach_plugin_command_pipeline_results(
    plugin_id: &str,
    command_name: &str,
    results: &[bmux_ipc::ServicePipelineStepResult],
) -> std::result::Result<AttachPluginCommandPipelineExecution, ClientError> {
    let command_result = results.first().ok_or(ClientError::UnexpectedResponse(
        "missing plugin command pipeline result",
    ))?;
    let command_response: bmux_plugin_sdk::PluginCliCommandResponse =
        bmux_plugin_sdk::decode_service_message(&command_result.payload).map_err(|error| {
            ClientError::ServerError {
                code: bmux_ipc::ErrorCode::Internal,
                message: format!("decoding plugin command pipeline response: {error}"),
            }
        })?;
    Ok(AttachPluginCommandPipelineExecution {
        status: command_response.exit_code,
        outcome: PluginCommandOutcome {
            error_message: command_response.error.or_else(|| {
                (command_response.exit_code != 0)
                    .then(|| format!("plugin action failed ({plugin_id}:{command_name})"))
            }),
            metadata: command_result.metadata.clone(),
        },
    })
}

async fn run_attach_plugin_command_pipeline(
    client: &mut StreamingBmuxClient,
    plugin_id: &str,
    command_name: &str,
    args: &[String],
) -> std::result::Result<AttachPluginCommandPipelineExecution, ClientError> {
    let started_at = Instant::now();
    let pipeline = build_attach_plugin_command_pipeline(plugin_id, command_name, args)?;
    let results = client.invoke_service_pipeline_raw(pipeline).await?;
    let execution =
        decode_attach_plugin_command_pipeline_results(plugin_id, command_name, &results)?;
    emit_attach_phase_timing(&serde_json::json!({
        "phase": "attach.plugin_command_pipeline",
        "plugin_id": plugin_id,
        "command_name": command_name,
        "total_us": started_at.elapsed().as_micros(),
    }));
    Ok(execution)
}

async fn enforce_hot_path_plugin_policy(
    client: &mut StreamingBmuxClient,
    plugin_id: &str,
    command_name: &str,
    attached_session_id: Uuid,
    attached_context_id: Option<Uuid>,
) -> anyhow::Result<bmux_plugin_sdk::CommandExecutionKind> {
    let hints = plugin_command_policy_hints(plugin_id, command_name).map_err(|error| {
        ClientError::ServerError {
            code: bmux_ipc::ErrorCode::InvalidRequest,
            message: error.to_string(),
        }
    })?;
    let execution = hints.execution.clone();

    if !matches!(
        execution,
        bmux_plugin_sdk::CommandExecutionKind::RuntimeHook
    ) {
        return Ok(execution);
    }

    if matches!(
        hints.execution_class,
        bmux_plugin::PluginExecutionClass::NativeFast
    ) {
        return Ok(execution);
    }

    let Some(hot_path_capability) = hints
        .required_capabilities
        .iter()
        .find(|capability| capability.is_hot_path())
    else {
        return Ok(execution);
    };

    let client_id = clients_state::client::current_client(client)
        .await?
        .map_err(|err| anyhow::anyhow!("clients-state current-client failed: {err:?}"))?
        .id;
    let principal_info = client.whoami_principal().await?;
    let execution_class = match hints.execution_class {
        bmux_plugin::PluginExecutionClass::NativeFast => "native_fast",
        bmux_plugin::PluginExecutionClass::NativeStandard => "native_standard",
        bmux_plugin::PluginExecutionClass::Interpreter => "interpreter",
    }
    .to_string();
    let response = session_policy_state::client::check(
        client,
        attached_session_id,
        attached_context_id,
        client_id,
        principal_info.principal_id,
        "hot_path_execution".to_string(),
        Some(plugin_id.to_string()),
        Some(hot_path_capability.to_string()),
        Some(execution_class),
    )
    .await?;
    if response.allowed {
        Ok(execution)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachPluginCommandExecutionRoute {
    ProviderPipeline,
    CallerProcess,
}

const fn attach_plugin_command_execution_route(
    execution: &bmux_plugin_sdk::CommandExecutionKind,
) -> AttachPluginCommandExecutionRoute {
    match execution {
        bmux_plugin_sdk::CommandExecutionKind::CallerProcess => {
            AttachPluginCommandExecutionRoute::CallerProcess
        }
        bmux_plugin_sdk::CommandExecutionKind::ProviderExec
        | bmux_plugin_sdk::CommandExecutionKind::BackgroundTask
        | bmux_plugin_sdk::CommandExecutionKind::RuntimeHook => {
            AttachPluginCommandExecutionRoute::ProviderPipeline
        }
    }
}

fn run_attach_plugin_command_local(
    plugin_id: &str,
    command_name: &str,
    args: &[String],
    kernel_client_factory: Option<&KernelClientFactory>,
    caller_client_id: Option<Uuid>,
    active_keybindings: Vec<bmux_plugin_sdk::ActiveKeybinding>,
) -> std::result::Result<AttachPluginCommandPipelineExecution, ClientError> {
    let execution = run_plugin_keybinding_command_with_active_bindings(
        plugin_id,
        command_name,
        args,
        kernel_client_factory,
        caller_client_id,
        active_keybindings,
    )
    .map_err(|error| ClientError::ServerError {
        code: bmux_ipc::ErrorCode::Internal,
        message: format!("local plugin command failed: {error:#}"),
    })?;

    Ok(AttachPluginCommandPipelineExecution {
        status: execution.status,
        outcome: execution.outcome,
    })
}

fn active_keybindings_for_context(
    keymap: &Keymap,
    mode_id: Option<&str>,
    scroll_mode: bool,
) -> Vec<bmux_plugin_sdk::ActiveKeybinding> {
    keymap
        .active_bindings_for_state(mode_id, scroll_mode)
        .into_iter()
        .map(|binding| bmux_plugin_sdk::ActiveKeybinding {
            scope: binding.scope,
            chord: binding.chord,
            action: binding.action_name,
        })
        .collect()
}

#[allow(clippy::too_many_lines)]
pub async fn handle_attach_plugin_command_action(
    client: &mut StreamingBmuxClient,
    plugin_id: &str,
    command_name: &str,
    args: &[String],
    view_state: &mut AttachViewState,
    kernel_client_factory: Option<&KernelClientFactory>,
    active_keybindings: Vec<bmux_plugin_sdk::ActiveKeybinding>,
) -> std::result::Result<(), ClientError> {
    let total_started = Instant::now();
    let before_started = Instant::now();
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
    let before_us = before_started.elapsed().as_micros();
    debug!(
        plugin_id = %plugin_id,
        command_name = %command_name,
        before_context_id = ?before_context_id,
        attached_context_id = ?view_state.attached_context_id,
        attached_session_id = %view_state.attached_id,
        "attach.plugin_command.start"
    );
    let policy_started = Instant::now();
    let execution_kind = match enforce_hot_path_plugin_policy(
        client,
        plugin_id,
        command_name,
        view_state.attached_id,
        view_state.attached_context_id,
    )
    .await
    {
        Ok(execution) => execution,
        Err(error) => {
            let policy_us = policy_started.elapsed().as_micros();
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
            emit_attach_plugin_command_timing(
                plugin_id,
                command_name,
                "policy_denied",
                before_context_id,
                None,
                view_state.attached_context_id,
                view_state.attached_id,
                before_us,
                policy_us,
                0,
                0,
                0,
                0,
                total_started.elapsed().as_micros(),
            );
            return Ok(());
        }
    };
    let policy_us = policy_started.elapsed().as_micros();
    let run_started = Instant::now();
    let command_execution = match attach_plugin_command_execution_route(&execution_kind) {
        AttachPluginCommandExecutionRoute::ProviderPipeline => {
            run_attach_plugin_command_pipeline(client, plugin_id, command_name, args).await
        }
        AttachPluginCommandExecutionRoute::CallerProcess => run_attach_plugin_command_local(
            plugin_id,
            command_name,
            args,
            kernel_client_factory,
            view_state.self_client_id,
            active_keybindings,
        ),
    };
    match command_execution {
        Err(error) => {
            let run_us = run_started.elapsed().as_micros();
            warn!(
                plugin_id = %plugin_id,
                command_name = %command_name,
                error = %error,
                "attach.plugin_command.run_failed"
            );
            view_state.set_transient_status(
                format!("plugin command failed: {}", map_attach_client_error(error)),
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
            emit_attach_plugin_command_timing(
                plugin_id,
                command_name,
                "run_error",
                before_context_id,
                None,
                view_state.attached_context_id,
                view_state.attached_id,
                before_us,
                policy_us,
                run_us,
                0,
                0,
                0,
                total_started.elapsed().as_micros(),
            );
        }
        Ok(execution) => {
            let run_us = run_started.elapsed().as_micros();
            let status = execution.status;
            let outcome_status_message = status_message_from_command_outcome(&execution.outcome);
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
                let status_text = outcome_status_message.unwrap_or_else(|| {
                    error_detail.map_or_else(
                        || {
                            format!(
                                "plugin action failed ({plugin_id}:{command_name}) exit {status}"
                            )
                        },
                        truncate_attach_status_message,
                    )
                });
                view_state.set_transient_status(
                    status_text,
                    Instant::now(),
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
                emit_attach_plugin_command_timing(
                    plugin_id,
                    command_name,
                    "nonzero",
                    before_context_id,
                    None,
                    view_state.attached_context_id,
                    view_state.attached_id,
                    before_us,
                    policy_us,
                    run_us,
                    0,
                    0,
                    0,
                    total_started.elapsed().as_micros(),
                );
                return Ok(());
            }

            let outcome_started = Instant::now();
            let outcome_applied =
                match apply_plugin_command_outcome(client, view_state, execution.outcome).await {
                    Ok(applied) => applied,
                    Err(error) => {
                        let outcome_us = outcome_started.elapsed().as_micros();
                        view_state.set_transient_status(
                            format!(
                                "plugin outcome apply failed: {}",
                                map_attach_client_error(error)
                            ),
                            Instant::now(),
                            ATTACH_TRANSIENT_STATUS_TTL,
                        );
                        emit_attach_plugin_command_timing(
                            plugin_id,
                            command_name,
                            "outcome_error",
                            before_context_id,
                            None,
                            view_state.attached_context_id,
                            view_state.attached_id,
                            before_us,
                            policy_us,
                            run_us,
                            outcome_us,
                            0,
                            0,
                            total_started.elapsed().as_micros(),
                        );
                        return Ok(());
                    }
                };
            let outcome_us = outcome_started.elapsed().as_micros();

            let after_started = Instant::now();
            let (after_context_id, after_context_ids, after_us) = if outcome_applied {
                (None, None, 0)
            } else {
                let after_context_id = typed_current_context_attach(client)
                    .await
                    .map_or(None, |context| context.map(|entry| entry.id));
                let after_context_ids =
                    typed_list_contexts_attach(client)
                        .await
                        .ok()
                        .map(|contexts| {
                            contexts
                                .into_iter()
                                .map(|context| context.id)
                                .collect::<std::collections::BTreeSet<_>>()
                        });
                (
                    after_context_id,
                    after_context_ids,
                    after_started.elapsed().as_micros(),
                )
            };
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
                let retarget_started = Instant::now();
                if let Err(error) =
                    retarget_attach_to_context(client, view_state, fallback_context_id).await
                {
                    let retarget_us = retarget_started.elapsed().as_micros();
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
                    emit_attach_plugin_command_timing(
                        plugin_id,
                        command_name,
                        "fallback_retarget_error",
                        before_context_id,
                        after_context_id,
                        view_state.attached_context_id,
                        view_state.attached_id,
                        before_us,
                        policy_us,
                        run_us,
                        outcome_us,
                        after_us,
                        retarget_us,
                        total_started.elapsed().as_micros(),
                    );
                    return Ok(());
                }
                let retarget_us = retarget_started.elapsed().as_micros();
                view_state.set_transient_status(
                    format!("plugin action: {plugin_id}:{command_name} (fallback retarget)"),
                    Instant::now(),
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
                view_state
                    .dirty
                    .mark_layout_frame_dirty(AttachDirtySource::PluginCommand);
                emit_attach_plugin_command_timing(
                    plugin_id,
                    command_name,
                    "fallback_retarget",
                    before_context_id,
                    after_context_id,
                    view_state.attached_context_id,
                    view_state.attached_id,
                    before_us,
                    policy_us,
                    run_us,
                    outcome_us,
                    after_us,
                    retarget_us,
                    total_started.elapsed().as_micros(),
                );
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
                let retarget_started = Instant::now();
                if let Err(error) =
                    retarget_attach_to_context(client, view_state, fallback_context_id).await
                {
                    let retarget_us = retarget_started.elapsed().as_micros();
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
                    emit_attach_plugin_command_timing(
                        plugin_id,
                        command_name,
                        "fallback_new_retarget_error",
                        before_context_id,
                        after_context_id,
                        view_state.attached_context_id,
                        view_state.attached_id,
                        before_us,
                        policy_us,
                        run_us,
                        outcome_us,
                        after_us,
                        retarget_us,
                        total_started.elapsed().as_micros(),
                    );
                    return Ok(());
                }
                let retarget_us = retarget_started.elapsed().as_micros();
                view_state.set_transient_status(
                    format!("plugin action: {plugin_id}:{command_name} (new context retarget)"),
                    Instant::now(),
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
                view_state
                    .dirty
                    .mark_layout_frame_dirty(AttachDirtySource::PluginCommand);
                emit_attach_plugin_command_timing(
                    plugin_id,
                    command_name,
                    "fallback_new_retarget",
                    before_context_id,
                    after_context_id,
                    view_state.attached_context_id,
                    view_state.attached_id,
                    before_us,
                    policy_us,
                    run_us,
                    outcome_us,
                    after_us,
                    retarget_us,
                    total_started.elapsed().as_micros(),
                );
                return Ok(());
            }

            view_state.set_transient_status(
                outcome_status_message
                    .unwrap_or_else(|| format!("plugin action: {plugin_id}:{command_name}")),
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
            view_state
                .dirty
                .mark_layout_frame_dirty(AttachDirtySource::PluginCommand);
            emit_attach_plugin_command_timing(
                plugin_id,
                command_name,
                "ok",
                before_context_id,
                after_context_id,
                view_state.attached_context_id,
                view_state.attached_id,
                before_us,
                policy_us,
                run_us,
                outcome_us,
                after_us,
                0,
                total_started.elapsed().as_micros(),
            );
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct AttachCloseFallback {
    target: AttachCloseFallbackTarget,
    label: &'static str,
}

const FINAL_PANE_CHOICE_NEW_PANE: &str = "new-pane";
const FINAL_PANE_CHOICE_NEW_SESSION: &str = "new-session";
const FINAL_PANE_CHOICE_QUIT: &str = "quit";
const FINAL_PANE_CHOICE_CANCEL: &str = "cancel";

fn next_context_fallback(view_state: &AttachViewState) -> Option<AttachCloseFallback> {
    let current_context = view_state.attached_context_id;
    let windows = view_state.cached_window_list.as_ref()?;
    let candidates = windows
        .windows
        .iter()
        .filter(|entry| Some(entry.id) != current_context)
        .filter(|entry| {
            context_session_id(&view_state.cached_context_session_bindings, entry.id)
                != Some(view_state.attached_id)
        })
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return None;
    }
    let current_index =
        current_context.and_then(|id| windows.windows.iter().position(|w| w.id == id));
    let selected = current_index
        .and_then(|index| {
            windows.windows[index.saturating_add(1)..]
                .iter()
                .find(|entry| candidates.iter().any(|candidate| candidate.id == entry.id))
                .or_else(|| {
                    windows.windows[..index]
                        .iter()
                        .rev()
                        .find(|entry| candidates.iter().any(|candidate| candidate.id == entry.id))
                })
        })
        .or_else(|| candidates.first().copied())?;

    Some(AttachCloseFallback {
        target: AttachCloseFallbackTarget::Context {
            context_id: selected.id,
        },
        label: "another tab",
    })
}

fn next_session_fallback(view_state: &AttachViewState) -> Option<AttachCloseFallback> {
    view_state
        .cached_sessions
        .iter()
        .find(|session| session.id != view_state.attached_id)
        .map(|session| AttachCloseFallback {
            target: AttachCloseFallbackTarget::Session {
                session_id: session.id,
            },
            label: "another session",
        })
}

fn next_close_fallback(view_state: &AttachViewState) -> Option<AttachCloseFallback> {
    next_context_fallback(view_state).or_else(|| next_session_fallback(view_state))
}

fn enqueue_final_pane_action_prompt(
    view_state: &mut AttachViewState,
    pane_id: Uuid,
    session_id: Uuid,
) {
    view_state.prompt.enqueue_internal(
        PromptRequest::single_select(
            "Final pane action",
            vec![
                PromptOption::new(FINAL_PANE_CHOICE_NEW_PANE, "New pane in this session"),
                PromptOption::new(FINAL_PANE_CHOICE_NEW_SESSION, "New session"),
                PromptOption::new(FINAL_PANE_CHOICE_QUIT, "Quit bmux"),
                PromptOption::new(FINAL_PANE_CHOICE_CANCEL, "Cancel"),
            ],
        )
        .message("This is the final available pane. Choose what bmux should do next.")
        .policy(prompt::PromptPolicy::RejectIfBusy),
        AttachInternalPromptAction::FinalPaneAction {
            pane_id,
            session_id,
        },
    );
}

#[allow(clippy::too_many_lines)]
pub fn handle_attach_ui_action_at(
    action: &RuntimeAction,
    view_state: &mut AttachViewState,
    now: Instant,
) {
    match action {
        RuntimeAction::EnterScrollMode => {
            if enter_attach_scrollback(view_state) {
            } else {
                view_state.set_transient_status(
                    ATTACH_SCROLLBACK_UNAVAILABLE_STATUS,
                    now,
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
            }
        }
        RuntimeAction::ExitScrollMode => {
            if view_state.selection_active() {
                clear_attach_selection_at(view_state, true, now);
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
        RuntimeAction::ScrollTop if view_state.scrollback_active => {
            view_state.scrollback_offset = max_attach_scrollback(view_state);
            clamp_attach_scrollback_cursor(view_state);
        }
        RuntimeAction::ScrollBottom if view_state.scrollback_active => {
            view_state.scrollback_offset = 0;
            clamp_attach_scrollback_cursor(view_state);
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
        RuntimeAction::BeginSelection if begin_attach_selection(view_state) => {
            view_state.set_transient_status(
                ATTACH_SELECTION_STARTED_STATUS,
                now,
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        RuntimeAction::CopyScrollback => {
            copy_attach_selection_at(view_state, false, now);
        }
        RuntimeAction::ConfirmScrollback => {
            confirm_attach_scrollback_at(view_state, now);
        }
        RuntimeAction::SwitchProfile(_) => {
            view_state.set_transient_status(
                "switch_profile is handled by attach input loop",
                now,
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        RuntimeAction::Quit => {
            if view_state.prompt.is_busy() {
                view_state.set_transient_status(
                    "prompt already active",
                    now,
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
                return;
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
        RuntimeAction::PluginCommand {
            plugin_id,
            command_name,
            ..
        } if is_windows_close_active_pane_command(plugin_id, command_name) => {
            let Some(pane_id) = focused_attach_pane_id(view_state) else {
                view_state.set_transient_status(
                    "no focused pane",
                    now,
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
                return;
            };
            if view_state.prompt.is_busy() {
                view_state.set_transient_status(
                    "prompt already active",
                    now,
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
                return;
            }
            let pane_count = view_state
                .cached_layout_state
                .as_ref()
                .map_or(0, |layout| layout.panes.len());
            if pane_count > 1 {
                view_state.prompt.enqueue_internal(
                    PromptRequest::confirm("Close focused pane?")
                        .message("This will stop the pane process.")
                        .submit_label("Close")
                        .cancel_label("Cancel")
                        .confirm_default(false)
                        .policy(prompt::PromptPolicy::RejectIfBusy),
                    AttachInternalPromptAction::ClosePane { pane_id },
                );
            } else if let Some(fallback) = next_close_fallback(view_state) {
                view_state.prompt.enqueue_internal(
                    PromptRequest::confirm(format!(
                        "Close this pane and switch to {}?",
                        fallback.label
                    ))
                    .message(
                        "This is the last pane in the current session. bmux will switch before closing it.",
                    )
                    .submit_label("Close and switch")
                    .cancel_label("Cancel")
                    .confirm_default(false)
                    .policy(prompt::PromptPolicy::RejectIfBusy),
                    AttachInternalPromptAction::CloseLastPaneAndSwitch {
                        old_session_id: view_state.attached_id,
                        target: fallback.target,
                    },
                );
            } else {
                enqueue_final_pane_action_prompt(view_state, pane_id, view_state.attached_id);
            }
        }
        _ => {}
    }
}

pub fn enter_attach_scrollback(view_state: &mut AttachViewState) -> bool {
    let Some((inner_w, inner_h)) = focused_attach_pane_inner_size(view_state) else {
        return false;
    };
    let Some(buffer) = focused_attach_pane_buffer(view_state) else {
        return false;
    };
    let cursor = buffer.terminal_grid.grid().cursor();
    view_state.scrollback_active = true;
    view_state.scrollback_offset = 0;
    view_state.scrollback_cursor = Some(AttachScrollbackCursor {
        row: cursor.row.min(inner_h.saturating_sub(1)),
        col: cursor.col.min(inner_w.saturating_sub(1)),
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

pub fn clear_attach_selection_at(
    view_state: &mut AttachViewState,
    show_status: bool,
    now: Instant,
) {
    view_state.selection_anchor = None;
    if show_status {
        view_state.set_transient_status(
            ATTACH_SELECTION_CLEARED_STATUS,
            now,
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

pub fn copy_attach_selection_at(
    view_state: &mut AttachViewState,
    exit_after_copy: bool,
    now: Instant,
) {
    let Some(text) = selected_attach_text(view_state) else {
        if exit_after_copy {
            view_state.exit_scrollback();
        } else {
            view_state.set_transient_status(
                ATTACH_SELECTION_EMPTY_STATUS,
                now,
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        return;
    };

    match copy_text_with_clipboard_plugin(&text) {
        Ok(()) => {
            view_state.set_transient_status(
                ATTACH_SELECTION_COPIED_STATUS,
                now,
                ATTACH_TRANSIENT_STATUS_TTL,
            );
            if exit_after_copy {
                view_state.exit_scrollback();
            }
        }
        Err(error) => {
            view_state.set_transient_status(
                format_clipboard_service_error(&error),
                now,
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
    }
}

pub fn confirm_attach_scrollback_at(view_state: &mut AttachViewState, now: Instant) {
    copy_attach_selection_at(view_state, true, now);
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
        &config,
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

pub fn extract_attach_text(
    view_state: &mut AttachViewState,
    start: AttachScrollbackPosition,
    end: AttachScrollbackPosition,
) -> Option<String> {
    let buffer = focused_attach_pane_buffer(view_state)?;
    let grid = buffer.terminal_grid.grid();
    let width = grid.width();
    if width == 0 {
        return Some(String::new());
    }
    let selected_rows = end.row.saturating_sub(start.row).saturating_add(1);
    let main_row_count = grid.main_row_count();
    if main_row_count == 0 {
        return Some(String::new());
    }
    let display_end = main_row_count.saturating_sub(start.row.min(main_row_count));
    let display_start = display_end.saturating_sub(grid.height());
    let rows = grid.display_rows(start.row, grid.height());
    let rows = &rows[rows
        .len()
        .saturating_sub(display_end.saturating_sub(display_start))..];
    if rows.is_empty() {
        return Some(String::new());
    }
    let end_row = selected_rows
        .saturating_sub(1)
        .min(rows.len().saturating_sub(1));
    let mut lines = Vec::with_capacity(end_row.saturating_add(1));
    for (row_index, row) in rows.iter().enumerate().take(end_row.saturating_add(1)) {
        let start_col = if row_index == 0 { start.col } else { 0 };
        let end_col = if row_index == end_row {
            end.col.saturating_add(1)
        } else {
            width
        };
        lines.push(grid_row_text_range(row, width, start_col, end_col));
    }
    Some(lines.join("\n"))
}

fn grid_row_text_range(
    row: &bmux_terminal_grid::PhysicalRow,
    width: usize,
    start_col: usize,
    end_col: usize,
) -> String {
    let mut text = String::new();
    let start = start_col.min(width);
    let end = end_col.min(width);
    for col in start..end {
        let Some(cell) = row.cells().get(col) else {
            text.push(' ');
            continue;
        };
        if !cell.is_wide_continuation() {
            text.push_str(cell.text());
        }
    }
    text.trim_end().to_string()
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
    let local = buffer
        .terminal_grid
        .grid()
        .main_row_count()
        .saturating_sub(buffer.terminal_grid.grid().height());
    buffer
        .scrollback_window
        .as_ref()
        .map_or(local, |window| local.max(window.max_scrollback_offset))
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

fn attach_view_floating_summary(view_state: &AttachViewState) -> (usize, bool, bool) {
    let Some(layout_state) = view_state.cached_layout_state.as_ref() else {
        return (0, false, false);
    };
    let focused_pane_id = focused_attach_pane_id(view_state);
    let mut has_tiled = false;
    let mut focused_floating = false;
    let mut floating_panes = BTreeSet::new();
    for surface in &layout_state.scene.surfaces {
        if !surface.visible || surface.pane_id.is_none() {
            continue;
        }
        match surface.kind {
            AttachSurfaceKind::Pane => has_tiled = true,
            AttachSurfaceKind::FloatingPane => {
                if let Some(pane_id) = surface.pane_id {
                    floating_panes.insert(pane_id);
                    focused_floating |= Some(pane_id) == focused_pane_id;
                }
            }
            AttachSurfaceKind::Modal | AttachSurfaceKind::Overlay | AttachSurfaceKind::Tooltip => {}
        }
    }
    (
        floating_panes.len(),
        !has_tiled && !floating_panes.is_empty(),
        focused_floating,
    )
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
            // and the content_rect contract on `AttachSurface`.
            (
                usize::from(surface.content_rect.w.max(1)),
                usize::from(surface.content_rect.h.max(1)),
            )
        })
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::fn_params_excessive_bools,
    clippy::needless_pass_by_ref_mut
)]
pub fn build_attach_status_line_for_draw(
    _client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    status_config: &bmux_config::StatusBarConfig,
    runtime_appearance: &RuntimeAppearance,
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
    geometry: TerminalGeometry,
) -> AttachStatusLine {
    if geometry.cols == 0 {
        return AttachStatusLine {
            rendered: String::new(),
            spans: Vec::new(),
            tab_hitboxes: Vec::new(),
            drag_marker_col: None,
        };
    }

    let cached_contexts = view_state.cached_contexts.clone();
    let cached_sessions = view_state.cached_sessions.clone();
    let cached_context_session_bindings = view_state.cached_context_session_bindings.clone();

    let tabs = build_attach_tabs_from_catalog(
        &cached_contexts,
        &cached_context_session_bindings,
        view_state,
        status_config,
        context_id,
        session_id,
    );
    let (session_label, session_count) =
        resolve_attach_session_label_and_count_from_catalog(&cached_sessions, session_id);
    let current_context_label = resolve_attach_context_label_from_catalog(
        &cached_contexts,
        &cached_context_session_bindings,
        context_id,
        session_id,
    );
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
    let appearance_mode_id = if help_overlay_open {
        "help"
    } else if prompt_active {
        "prompt"
    } else if scrollback_active {
        "scroll"
    } else if zoomed {
        "zoom"
    } else {
        view_state.active_mode_id.as_str()
    };
    let runtime_appearance = runtime_appearance.for_mode(appearance_mode_id);
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
        let (floating_count, floating_only, focused_floating) =
            attach_view_floating_summary(view_state);
        if floating_only {
            format!("{floating_count} floating pane(s) remain; close them before closing this tab")
        } else if focused_floating {
            format!(
                "floating pane focused ({floating_count} visible) | {}",
                attach_mode_hint(&view_state.active_mode_id, ui_mode, keymap)
            )
        } else {
            attach_mode_hint(&view_state.active_mode_id, ui_mode, keymap)
        }
    };

    let mut status_line = build_attach_status_line(
        geometry.cols,
        status_config,
        &runtime_appearance,
        &session_label,
        session_count,
        &current_context_label,
        &tabs,
        tab_position_label.as_deref(),
        mode_label,
        role_label,
        follow_label.as_deref(),
        &hint,
    );
    status_line.drag_marker_col = view_state
        .mouse
        .tab_drag
        .as_ref()
        .and_then(|drag| drag.drop_target)
        .and_then(|target| attach_tab_drop_marker_col(&status_line, target, geometry.cols));
    status_line
}

pub fn attach_mode_hint(mode_id: &str, _ui_mode: AttachUiMode, keymap: &Keymap) -> String {
    let detach = key_hint_or_unbound(keymap, mode_id, &RuntimeAction::Detach);
    let quit = key_hint_or_unbound(keymap, mode_id, &RuntimeAction::Quit);
    let help = key_hint_or_unbound(keymap, mode_id, &RuntimeAction::ShowHelp);
    let restart = key_hint_or_unbound(
        keymap,
        mode_id,
        &RuntimeAction::PluginCommand {
            plugin_id: "bmux.windows".to_string(),
            command_name: "restart-pane".to_string(),
            args: Vec::new(),
        },
    );
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

fn windows_close_active_pane_action() -> RuntimeAction {
    RuntimeAction::PluginCommand {
        plugin_id: "bmux.windows".to_string(),
        command_name: "close-active-pane".to_string(),
        args: Vec::new(),
    }
}

fn is_windows_close_active_pane_command(plugin_id: &str, command_name: &str) -> bool {
    plugin_id == "bmux.windows" && command_name == "close-active-pane"
}

fn is_windows_close_active_pane_action(action: &RuntimeAction) -> bool {
    matches!(
        action,
        RuntimeAction::PluginCommand {
            plugin_id,
            command_name,
            ..
        } if is_windows_close_active_pane_command(plugin_id, command_name)
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
    let mode_changed = view_state.active_mode_id != mode_id;
    let label_changed = view_state.active_mode_label != mode_label;
    view_state.active_mode_id = mode_id;
    view_state.active_mode_label = mode_label;
    if mode_changed {
        view_state
            .dirty
            .mark_full_frame(AttachDirtySource::ActionDispatch);
        view_state
            .dirty
            .mark_status_dirty(AttachDirtySource::ActionDispatch);
    } else if label_changed {
        view_state
            .dirty
            .mark_status_dirty(AttachDirtySource::ActionDispatch);
    }
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
    let previous_tab_drag_enabled = view_state.mouse.tab_drag_enabled;
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
        view_state.mouse.tab_drag_enabled = status_tab_drag_enabled(&resolved_config.status_bar);
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
        view_state.mouse.tab_drag_enabled = previous_tab_drag_enabled;
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

fn status_line_render_ops(
    status_line: &AttachStatusLine,
    status_row: u16,
    cols: u16,
) -> Vec<RenderOp> {
    let mut ops = Vec::new();
    if status_line.spans.is_empty() {
        ops.push(RenderOp::text_run(
            0,
            status_row,
            &status_line.rendered,
            RenderStyle::new(),
        ));
    } else {
        ops.push(RenderOp::styled_text(
            0,
            status_row,
            status_line.spans.clone(),
        ));
    }
    if let Some(marker_col) = status_line.drag_marker_col {
        ops.push(RenderOp::text_run(
            marker_col.min(cols.saturating_sub(1)),
            status_row,
            "│",
            RenderStyle::new(),
        ));
    }
    ops
}

fn retained_status_surface(
    status_line: &AttachStatusLine,
    status_position: StatusPosition,
    terminal_size: (u16, u16),
) -> Option<RetainedSurface> {
    let (cols, rows) = terminal_size;
    if cols == 0 || rows == 0 {
        return None;
    }
    let status_row = status_row_for_position(status_position, rows)?;
    Some(
        RetainedSurface::builder(STATUS_SURFACE_ID, DamageRect::new(0, status_row, cols, 1))
            .layer(retained_layer_order(SurfaceLayer::Status))
            .z(i32::MAX)
            .opaque()
            .render_ops(status_line_render_ops(status_line, status_row, cols))
            .build(),
    )
}

pub fn help_overlay_visible_rows(lines: &[String], geometry: TerminalGeometry) -> usize {
    let max_content_rows = (geometry.rows as usize).saturating_sub(6);
    let content_rows = lines.len().min(max_content_rows);
    let height = (content_rows + 4).min((geometry.rows as usize).saturating_sub(2));
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

pub fn handle_help_overlay_terminal_key_event(
    key: &TerminalKeyEvent,
    help_lines: &[String],
    view_state: &mut AttachViewState,
    geometry: TerminalGeometry,
) -> bool {
    key.to_crossterm()
        .is_some_and(|key| handle_help_overlay_key_event(&key, help_lines, view_state, geometry))
}

pub fn handle_help_overlay_key_event(
    key: &KeyEvent,
    help_lines: &[String],
    view_state: &mut AttachViewState,
    geometry: TerminalGeometry,
) -> bool {
    if !help_overlay_accepts_key_kind(key.kind) {
        return false;
    }

    match key.code {
        KeyCode::Esc | KeyCode::Enter => {
            view_state.help_overlay_open = false;
            view_state.help_overlay_scroll = 0;
            view_state
                .dirty
                .mark_status_dirty(AttachDirtySource::HelpOverlay);
            view_state
                .dirty
                .mark_overlay_dirty(AttachDirtySource::HelpOverlay);
            true
        }
        KeyCode::Up | KeyCode::Char('k') => {
            view_state.help_overlay_scroll = adjust_help_overlay_scroll(
                view_state.help_overlay_scroll,
                -1,
                help_lines.len(),
                help_overlay_visible_rows(help_lines, geometry),
            );
            view_state
                .dirty
                .mark_overlay_dirty(AttachDirtySource::HelpOverlay);
            true
        }
        KeyCode::Down | KeyCode::Char('j') => {
            view_state.help_overlay_scroll = adjust_help_overlay_scroll(
                view_state.help_overlay_scroll,
                1,
                help_lines.len(),
                help_overlay_visible_rows(help_lines, geometry),
            );
            view_state
                .dirty
                .mark_overlay_dirty(AttachDirtySource::HelpOverlay);
            true
        }
        KeyCode::PageUp => {
            let page = help_overlay_visible_rows(help_lines, geometry).cast_signed();
            view_state.help_overlay_scroll = adjust_help_overlay_scroll(
                view_state.help_overlay_scroll,
                -page,
                help_lines.len(),
                help_overlay_visible_rows(help_lines, geometry),
            );
            view_state
                .dirty
                .mark_overlay_dirty(AttachDirtySource::HelpOverlay);
            true
        }
        KeyCode::PageDown => {
            let page = help_overlay_visible_rows(help_lines, geometry).cast_signed();
            view_state.help_overlay_scroll = adjust_help_overlay_scroll(
                view_state.help_overlay_scroll,
                page,
                help_lines.len(),
                help_overlay_visible_rows(help_lines, geometry),
            );
            view_state
                .dirty
                .mark_overlay_dirty(AttachDirtySource::HelpOverlay);
            true
        }
        KeyCode::Home => {
            view_state.help_overlay_scroll = 0;
            view_state
                .dirty
                .mark_overlay_dirty(AttachDirtySource::HelpOverlay);
            true
        }
        KeyCode::End => {
            let visible = help_overlay_visible_rows(help_lines, geometry);
            view_state.help_overlay_scroll = help_lines.len().saturating_sub(visible);
            view_state
                .dirty
                .mark_overlay_dirty(AttachDirtySource::HelpOverlay);
            true
        }
        _ => false,
    }
}

#[allow(clippy::cast_possible_truncation)] // Terminal dimensions bounded by u16
pub fn help_overlay_surface(lines: &[String], geometry: TerminalGeometry) -> Option<AttachSurface> {
    if geometry.cols < 20 || geometry.rows < 6 {
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
        .min((geometry.cols as usize).saturating_sub(2));
    let max_content_rows = (geometry.rows as usize).saturating_sub(6);
    let content_rows = lines.len().min(max_content_rows);
    let height = (content_rows + 4).min((geometry.rows as usize).saturating_sub(2));
    let x = ((geometry.cols as usize).saturating_sub(width)) / 2;
    let y = ((geometry.rows as usize).saturating_sub(height)) / 2;

    Some(AttachSurface {
        id: HELP_OVERLAY_SURFACE_ID,
        kind: AttachSurfaceKind::Overlay,
        layer: SurfaceLayer::Overlay,
        z: i32::MAX,
        rect: AttachRect {
            x: x as u16,
            y: y as u16,
            w: width as u16,
            h: height as u16,
        },
        content_rect: AttachRect {
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

#[allow(clippy::cast_possible_truncation)] // Overlay geometry is clamped to terminal bounds before u16 conversion.
fn help_overlay_render_ops(
    surface_meta: &AttachSurface,
    lines: &[String],
    scroll: usize,
) -> Vec<RenderOp> {
    let width = usize::from(surface_meta.rect.w);
    let height = usize::from(surface_meta.rect.h);
    let x = usize::from(surface_meta.rect.x);
    let y = usize::from(surface_meta.rect.y);
    let body_rows = height.saturating_sub(4).max(1);
    let text_width = width.saturating_sub(4);
    let style = RenderStyle::new();
    let rect = ExtensionRect::new(
        surface_meta.rect.x,
        surface_meta.rect.y,
        surface_meta.rect.w,
        surface_meta.rect.h,
    );
    let interior = ExtensionRect::new(
        surface_meta.rect.x.saturating_add(1),
        surface_meta.rect.y.saturating_add(1),
        surface_meta.rect.w.saturating_sub(2),
        surface_meta.rect.h.saturating_sub(2),
    );

    let mut ops = vec![
        RenderOp::clear_rect(interior, style),
        RenderOp::border(rect, BorderGlyphs::ascii(), style),
    ];

    let title = " bmux help ";
    let title_x = x + ((width.saturating_sub(title.len())) / 2);
    ops.push(RenderOp::text_run(title_x as u16, y as u16, title, style));

    let header = "scope    chord                action";
    ops.push(RenderOp::text_run(
        (x + 2) as u16,
        (y + 1) as u16,
        opaque_row_text(header, text_width),
        style,
    ));

    let start = scroll.min(lines.len().saturating_sub(body_rows));
    let end = (start + body_rows).min(lines.len());
    for (idx, line) in lines.iter().skip(start).take(body_rows).enumerate() {
        let row = y + 2 + idx;
        if row >= y + height - 1 {
            break;
        }
        ops.push(RenderOp::text_run(
            (x + 2) as u16,
            row as u16,
            opaque_row_text(line, text_width),
            style,
        ));
    }

    let footer = format!(
        "j/k or ↑/↓ scroll | PgUp/PgDn | Esc close | {}-{} / {}",
        if lines.is_empty() { 0 } else { start + 1 },
        end,
        lines.len()
    );
    ops.push(RenderOp::text_run(
        (x + 2) as u16,
        (y + height - 2) as u16,
        opaque_row_text(&footer, text_width),
        style,
    ));
    ops
}

const fn attach_surface_damage_rect(surface: &AttachSurface) -> DamageRect {
    DamageRect::new(
        surface.rect.x,
        surface.rect.y,
        surface.rect.w,
        surface.rect.h,
    )
}

fn retained_surface_from_attach_surface(
    surface: &AttachSurface,
    opacity: RetainedOpacity,
    ops: Vec<RenderOp>,
) -> RetainedSurface {
    RetainedSurface::builder(surface.id, attach_surface_damage_rect(surface))
        .layer(retained_layer_order(surface.layer))
        .z(surface.z)
        .opacity(opacity)
        .render_ops(ops)
        .build()
}

fn retained_help_overlay_surface(
    surface: &AttachSurface,
    lines: &[String],
    scroll: usize,
) -> RetainedSurface {
    retained_surface_from_attach_surface(
        surface,
        RetainedOpacity::Opaque,
        help_overlay_render_ops(surface, lines, scroll),
    )
}

fn retained_prompt_overlay_surface(render: &AttachPromptOverlayRender) -> RetainedSurface {
    retained_surface_from_attach_surface(
        &render.surface,
        RetainedOpacity::Opaque,
        render.ops.clone(),
    )
}

fn retained_damage_overlay_surface(
    scene: &AttachScene,
    frame_damage: &bmux_attach_pipeline::FrameDamage,
    terminal_size: (u16, u16),
    status_top_inset: u16,
    status_bottom_inset: u16,
) -> Option<RetainedSurface> {
    let ops = frame_damage_overlay_render_ops(
        scene,
        frame_damage,
        terminal_size,
        status_top_inset,
        status_bottom_inset,
    );
    if ops.is_empty() || terminal_size.0 == 0 || terminal_size.1 == 0 {
        return None;
    }
    Some(
        RetainedSurface::builder(
            DAMAGE_OVERLAY_SURFACE_ID,
            DamageRect::new(0, 0, terminal_size.0, terminal_size.1),
        )
        .layer(retained_layer_order(SurfaceLayer::Tooltip))
        .z(i32::MAX)
        .transparent()
        .render_ops(ops)
        .build(),
    )
}

fn retained_full_surface_repaint(surface: &RetainedSurface) -> RetainedRepaintSurface {
    RetainedRepaintSurface {
        surface_id: surface.id,
        rect: surface.rect,
        layer: surface.layer,
        z: surface.z,
        opaque: surface.opaque,
        opacity: surface.opacity,
        clip_rect: surface.clip_rect,
        interactive_regions: surface.interactive_regions.clone(),
        damage: vec![surface.rect],
    }
}

fn render_damage_from_retained_damage_rects(rects: &[DamageRect]) -> RenderDamage {
    RenderDamage::from_rects(
        rects
            .iter()
            .map(|rect| ExtensionRect::new(rect.x, rect.y, rect.w, rect.h)),
    )
}

fn queue_retained_render_ops(
    stdout: &mut impl Write,
    surface: &RetainedSurface,
    repaint: &RetainedRepaintSurface,
) -> Result<bool> {
    let RetainedSurfacePayload::RenderOps(ops) = &surface.payload else {
        return Ok(false);
    };
    let surface_rect = ExtensionRect::new(
        surface.rect.x,
        surface.rect.y,
        surface.rect.w,
        surface.rect.h,
    );
    let damage = render_damage_from_retained_damage_rects(&repaint.damage);
    queue_render_ops(stdout, surface_rect, &damage, ops)
        .context("failed queueing retained render ops")
}

fn frame_uses_synchronized_update(frame_damage: &bmux_attach_pipeline::FrameDamage) -> bool {
    frame_damage.scene_damaged() || frame_damage.overlay_damaged()
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub fn render_attach_frame(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    layout_state: &AttachLayoutState,
    status_config: &bmux_config::StatusBarConfig,
    runtime_appearance: &RuntimeAppearance,
    follow_target_id: Option<Uuid>,
    follow_global: bool,
    keymap: &crate::input::Keymap,
    help_lines: &[String],
    help_scroll: usize,
    damage_config: &bmux_config::DamageBehaviorConfig,
    slow_terminal_write_ms: u64,
    display_capture: &mut DisplayCaptureFanout,
    now: Instant,
    geometry: TerminalGeometry,
    mut render_trace: Option<&mut AttachRenderTrace>,
) -> Result<AttachFrameRenderStats> {
    let damage_policy = DamageCoalescingPolicy {
        max_rects: damage_config.max_rects,
        max_area_percent: damage_config.max_area_percent,
    };
    let current_help_overlay_surface = if view_state.help_overlay_open {
        help_overlay_surface(help_lines, geometry)
    } else {
        None
    };
    let current_prompt_overlay_surface = view_state.prompt.overlay_surface(geometry);
    let mut frame_damage = view_state.dirty.frame_damage(&layout_state.scene);

    if view_state.dirty.status_needs_redraw {
        let transient_status = view_state.transient_status_text(now).map(str::to_owned);
        view_state.cached_status_line = Some(build_attach_status_line_for_draw(
            client,
            view_state,
            status_config,
            runtime_appearance,
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
            geometry,
        ));
        view_state.dirty.status_needs_redraw = false;
    }

    let (status_top_inset, status_bottom_inset) =
        status_insets_for_position(view_state.status_position);
    let terminal_size = (geometry.cols, geometry.rows);
    let viewport = DamageRect::new(0, 0, terminal_size.0, terminal_size.1);
    let status_surface = view_state
        .cached_status_line
        .as_ref()
        .and_then(|status_line| {
            retained_status_surface(status_line, view_state.status_position, terminal_size)
        });
    let help_retained_surface = current_help_overlay_surface
        .as_ref()
        .map(|surface| retained_help_overlay_surface(surface, help_lines, help_scroll));
    let prompt_overlay_render = if view_state.prompt.is_active() {
        view_state.prompt.attach_prompt_overlay_render(geometry)
    } else {
        None
    };
    let prompt_retained_surface = prompt_overlay_render
        .as_ref()
        .map(retained_prompt_overlay_surface);

    let retained_frame_damage = retained_frame_damage_from_frame_damage(
        &layout_state.scene,
        &frame_damage,
        viewport,
        damage_policy,
    );
    let mut explicit_ui_damage_rects = Vec::new();
    if frame_damage.status_damaged()
        && let Some(surface) = status_surface.as_ref()
    {
        explicit_ui_damage_rects.push(surface.rect);
    }
    if frame_damage.overlay_damaged() {
        if let Some(surface) = help_retained_surface.as_ref() {
            explicit_ui_damage_rects.push(surface.rect);
        }
        if let Some(surface) = prompt_retained_surface.as_ref() {
            explicit_ui_damage_rects.push(surface.rect);
        }
    }
    let explicit_ui_damage =
        retained_damage_from_absolute_rects(explicit_ui_damage_rects, viewport, damage_policy);
    let mut retained_surfaces = retained_surfaces_from_attach_scene(&layout_state.scene);
    retained_surfaces.extend(status_surface.iter().cloned());
    retained_surfaces.extend(help_retained_surface.iter().cloned());
    retained_surfaces.extend(prompt_retained_surface.iter().cloned());
    let retained_graph_damage = view_state.retained_compositor.replace_surfaces(
        retained_surfaces.clone(),
        viewport,
        damage_policy,
    );
    let retained_damage = merge_retained_damages(
        [
            retained_frame_damage,
            explicit_ui_damage,
            retained_graph_damage,
        ],
        viewport,
        damage_policy,
    );
    let retained_repaint_plan = view_state
        .retained_compositor
        .repaint_plan(&retained_damage);
    frame_damage.merge_from(&frame_damage_from_retained_repaint_plan(
        &layout_state.scene,
        &retained_repaint_plan,
        damage_policy,
    ));
    let damage_stats = frame_damage.stats();
    let render_scene = frame_damage.scene_damaged();
    let retained_repaint_by_id = retained_repaint_plan
        .iter()
        .map(|surface| (surface.surface_id, surface))
        .collect::<BTreeMap<_, _>>();

    let use_synchronized_update =
        frame_uses_synchronized_update(&frame_damage) || damage_config.visualize;

    let mut frame_bytes = Vec::new();
    // Wrap multi-region scene/overlay frames in a synchronized update so the
    // terminal buffers output and displays it atomically (Mode 2026). Status-only
    // frames are tiny and safe to write directly, avoiding two extra control
    // sequences on the common low-damage path.
    if use_synchronized_update {
        queue!(frame_bytes, BeginSynchronizedUpdate)
            .context("failed queuing begin synchronized update")?;
    }
    queue!(frame_bytes, SavePosition).context("failed queuing cursor save for attach frame")?;
    // Hide the cursor during the frame render to prevent it from visibly
    // jumping to every MoveTo position as pane content is drawn.
    queue!(frame_bytes, Hide).context("failed queuing cursor hide for attach frame")?;
    // Reflect the forced-hide in tracked state so apply_attach_cursor_state
    // will re-emit Show if the cursor should be visible after the frame.
    let cursor_state_before_forced_hide = view_state.last_cursor_state;
    if let Some(ref mut cs) = view_state.last_cursor_state {
        cs.visible = false;
    }
    let status_rendered = if let Some((surface, repaint)) = status_surface
        .as_ref()
        .zip(retained_repaint_by_id.get(&STATUS_SURFACE_ID))
    {
        queue_retained_render_ops(&mut frame_bytes, surface, repaint)?
    } else {
        false
    };
    if status_rendered
        && let Some(trace) = render_trace.as_deref_mut()
        && let Some(row) = status_row_for_position(view_state.status_position, terminal_size.1)
    {
        trace.push(AttachRenderTraceOp::StatusLine {
            row,
            cells: terminal_size.0,
        });
    }
    let appearance_mode_id = if view_state.help_overlay_open {
        "help"
    } else if view_state.prompt.is_active() {
        "prompt"
    } else if view_state.scrollback_active {
        "scroll"
    } else if layout_state.zoomed {
        "zoom"
    } else {
        view_state.active_mode_id.as_str()
    };
    let active_runtime_appearance = runtime_appearance.for_mode(appearance_mode_id);
    let mut scene_render_stats = AttachSceneRenderStats::default();
    let cursor_state = if render_scene {
        // Snapshot the current set of registered render extensions
        // once per frame. The snapshot is cheap (Arc-clones) and
        // keeps the per-surface loop inside `render_attach_scene`
        // free of registry-lock churn.
        let extensions = bmux_plugin::registered_render_extensions();
        let (cursor_state, stats) = render_attach_scene_with_stats_and_trace(
            &mut frame_bytes,
            &layout_state.scene,
            &layout_state.panes,
            &mut view_state.pane_buffers,
            &frame_damage,
            status_top_inset,
            status_bottom_inset,
            view_state.scrollback_active,
            view_state.scrollback_offset,
            view_state.scrollback_cursor,
            view_state.selection_anchor,
            layout_state.zoomed,
            terminal_size,
            &active_runtime_appearance,
            damage_policy,
            &extensions,
            render_trace.as_deref_mut(),
        )?;
        scene_render_stats = stats;
        cursor_state
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
    if render_scene && view_state.host_image_caps.any_supported() {
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
    let mut overlay_rendered = false;
    let mut overlay_cursor_state = None;
    if let Some(help_surface) = help_retained_surface.as_ref()
        && let Some(repaint) = retained_repaint_by_id.get(&help_surface.id)
        && queue_retained_render_ops(&mut frame_bytes, help_surface, repaint)?
    {
        if let Some(trace) = render_trace.as_deref_mut() {
            trace.push(AttachRenderTraceOp::HelpOverlay {
                rows: help_surface.rect.h,
                cells: u64::from(help_surface.rect.w)
                    .saturating_mul(u64::from(help_surface.rect.h)),
            });
        }
        overlay_rendered = true;
    }
    if let Some(prompt_surface) = prompt_retained_surface.as_ref()
        && let Some(repaint) = retained_repaint_by_id.get(&prompt_surface.id)
        && queue_retained_render_ops(&mut frame_bytes, prompt_surface, repaint)?
    {
        overlay_cursor_state = prompt_overlay_render
            .as_ref()
            .and_then(|render| render.cursor_state);
        if let Some(trace) = render_trace.as_deref_mut() {
            trace.push(AttachRenderTraceOp::PromptOverlay {
                rows: prompt_surface.rect.h,
                cells: u64::from(prompt_surface.rect.w)
                    .saturating_mul(u64::from(prompt_surface.rect.h)),
            });
        }
        overlay_rendered = true;
    } else if view_state.prompt.is_active() {
        overlay_cursor_state = cursor_state_before_forced_hide.map(|mut cursor| {
            cursor.visible = true;
            cursor
        });
    }

    if damage_config.visualize
        && let Some(damage_surface) = retained_damage_overlay_surface(
            &layout_state.scene,
            &frame_damage,
            terminal_size,
            status_top_inset,
            status_bottom_inset,
        )
    {
        let repaint = retained_full_surface_repaint(&damage_surface);
        if queue_retained_render_ops(&mut frame_bytes, &damage_surface, &repaint)? {
            let rects = frame_damage_overlay_rects(
                &layout_state.scene,
                &frame_damage,
                terminal_size,
                status_top_inset,
                status_bottom_inset,
            );
            if let Some(trace) = render_trace.as_mut() {
                trace.push(AttachRenderTraceOp::DamageOverlay {
                    rects: u16::try_from(rects.len()).unwrap_or(u16::MAX),
                    cells: rects.iter().map(|rect| u64::from(rect.area())).sum::<u64>(),
                });
            }
            overlay_rendered = true;
        }
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
    display_capture.record_activity(DisplayActivityKind::Output);
    display_capture.record_cursor_snapshot(view_state.last_cursor_state);
    if previous_cursor_state != view_state.last_cursor_state {
        display_capture.record_activity(DisplayActivityKind::Cursor);
    }
    // Record structured image data for GIF export.
    #[cfg(any(
        feature = "image-sixel",
        feature = "image-kitty",
        feature = "image-iterm2"
    ))]
    {
        let mut all_images: Vec<bmux_attach_image_protocol::AttachPaneImage> = Vec::new();
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

    if use_synchronized_update {
        queue!(frame_bytes, EndSynchronizedUpdate)
            .context("failed queuing end synchronized update")?;
    }

    let terminal_write_started_at = Instant::now();
    let stdout = io::stdout();
    let mut stdout = io::BufWriter::new(stdout.lock());
    stdout
        .write_all(&frame_bytes)
        .context("failed writing attach frame")?;
    stdout.flush().context("failed flushing attach frame")?;
    let terminal_write_ms = duration_millis_u64(terminal_write_started_at.elapsed());
    if terminal_write_ms >= slow_terminal_write_ms {
        tracing::warn!(
            terminal_write_ms,
            threshold_ms = slow_terminal_write_ms,
            frame_bytes = frame_bytes.len(),
            "attach.terminal.slow_write"
        );
    }
    let stats = AttachFrameRenderStats {
        frame_bytes: frame_bytes.len(),
        terminal_write_ms,
        damage_rects: damage_stats.rect_count,
        damage_area_cells: damage_stats.rect_area_cells,
        full_surface_fallbacks: damage_stats.full_surface_count,
        full_frame_fallback: damage_stats.full_frame,
        scene_render: scene_render_stats,
        status_rendered,
        overlay_rendered,
        synchronized_update: use_synchronized_update,
        dirty_event_count: view_state.dirty.dirty_events().len(),
    };
    view_state.last_help_overlay_surface = current_help_overlay_surface;
    view_state.last_prompt_overlay_surface = current_prompt_overlay_surface;
    view_state.dirty.clear_frame_damage();
    Ok(stats)
}

pub const fn status_tab_drag_enabled(status_config: &bmux_config::StatusBarConfig) -> bool {
    !matches!(status_config.tab_scope, bmux_config::StatusTabScope::Mru)
        && !matches!(status_config.tab_order, bmux_config::StatusTabOrder::Mru)
}

pub fn build_attach_tabs_from_catalog(
    contexts: &[ContextRow],
    bindings: &[ContextSessionBinding],
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
            bindings,
            status_config,
            context_id,
            session_id,
        );
    }

    build_attach_tabs_from_raw_contexts(contexts, bindings, status_config, context_id, session_id)
}

/// Project the windows-plugin's authoritative ordered window list onto
/// the attach tab-bar view. Called whenever `cached_window_list` has
/// been populated from the `windows-list` state channel.
fn build_attach_tabs_from_plugin_snapshot(
    snapshot: &bmux_windows_plugin_api::windows_list::WindowListSnapshot,
    contexts: &[ContextRow],
    bindings: &[ContextSessionBinding],
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
                        context_session_id(bindings, context.id) == Some(session_id)
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
            .find(|context| context_session_id(bindings, context.id) == Some(session_id))
            .map(|context| context.id)
    });

    filtered
        .into_iter()
        .map(|entry| AttachTab {
            label: entry.name.clone(),
            active: current_id.map_or(entry.active, |id| id == entry.id),
            context_id: Some(entry.id),
        })
        .collect()
}

/// Baseline tab-bar renderer when no plugin snapshot is available
/// (plugin absent, not yet activated, or user opted into MRU mode).
/// Renders `cached_contexts` in the order the server delivered them.
/// Core does not stabilize or reorder — that's the plugin's job.
fn build_attach_tabs_from_raw_contexts(
    contexts: &[ContextRow],
    bindings: &[ContextSessionBinding],
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
                .filter(|context| context_session_id(bindings, context.id) == Some(session_id))
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
            .find(|context| context_session_id(bindings, context.id) == Some(session_id))
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
    contexts: &[ContextRow],
    bindings: &[ContextSessionBinding],
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

    if let Some((index, context)) = contexts
        .iter()
        .enumerate()
        .find(|(_, context)| context_session_id(bindings, context.id) == Some(session_id))
    {
        return context_summary_label(context, Some(index.saturating_add(1)));
    }

    "terminal".to_string()
}

pub fn context_summary_label(context: &ContextRow, fallback_index: Option<usize>) -> String {
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
    sessions: &[SessionRow],
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

pub fn session_summary_label(session: &SessionRow) -> String {
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
        &view_state.cached_context_session_bindings,
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

fn context_session_id(bindings: &[ContextSessionBinding], context_id: Uuid) -> Option<Uuid> {
    bindings
        .iter()
        .find(|binding| binding.context_id == context_id)
        .map(|binding| binding.session_id)
}

fn apply_control_catalog_snapshot(
    view_state: &mut AttachViewState,
    snapshot: control_catalog_state::Snapshot,
) {
    view_state.cached_contexts = snapshot.contexts;
    view_state.cached_sessions = snapshot.sessions;
    view_state.cached_context_session_bindings = snapshot.context_session_bindings;
    view_state.control_catalog_revision = snapshot.revision;
}

pub async fn reconcile_attached_session_from_catalog(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
) -> std::result::Result<bool, ClientError> {
    let Some(context_id) = view_state.attached_context_id else {
        return Ok(false);
    };

    let Some(mapped_session_id) =
        context_session_id(&view_state.cached_context_session_bindings, context_id)
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
    let snapshot =
        control_catalog_state::client::snapshot(client, Some(view_state.control_catalog_revision))
            .await
            .context("control-catalog snapshot dispatch failed")?;
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
        .map_or(None, |timeout| timeout.timeout_ms());
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
    let close = key_hint_or_unbound(&keymap, active_mode_id, &windows_close_active_pane_action());
    let restart = key_hint_or_unbound(
        &keymap,
        active_mode_id,
        &RuntimeAction::PluginCommand {
            plugin_id: "bmux.windows".to_string(),
            command_name: "restart-pane".to_string(),
            args: Vec::new(),
        },
    );
    let mut groups: Vec<(&str, Vec<AttachKeybindingEntry>)> = vec![
        ("Session", Vec::new()),
        ("Pane", Vec::new()),
        ("Mode", Vec::new()),
        ("Other", Vec::new()),
    ];

    for entry in effective_attach_keybindings(config) {
        let category = match &entry.action {
            RuntimeAction::Detach | RuntimeAction::Quit => "Session",
            RuntimeAction::PluginCommand {
                plugin_id,
                command_name,
                ..
            } if is_windows_close_active_pane_command(plugin_id, command_name) => "Pane",
            RuntimeAction::PluginCommand {
                plugin_id,
                command_name,
                ..
            } if plugin_id == "bmux.windows" && command_name == "focus-pane-in-direction" => "Pane",
            RuntimeAction::PluginCommand {
                plugin_id,
                command_name,
                ..
            } if plugin_id == "bmux.windows" && command_name == "resize-pane" => "Pane",
            RuntimeAction::PluginCommand {
                plugin_id,
                command_name,
                ..
            } if plugin_id == "bmux.windows"
                && matches!(
                    command_name.as_str(),
                    "split-pane" | "zoom-pane" | "restart-pane"
                ) =>
            {
                "Pane"
            }
            RuntimeAction::ExitMode
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
        RuntimeAction::NoOp
            | RuntimeAction::Detach
            | RuntimeAction::Quit
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
            | RuntimeAction::PluginCommand { .. }
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
    update_attach_viewport_with_geometry(
        client,
        session_id,
        status_position,
        current_attach_terminal_geometry(),
    )
    .await
}

pub async fn update_attach_viewport_with_geometry(
    client: &mut StreamingBmuxClient,
    session_id: Uuid,
    status_position: StatusPosition,
    geometry: TerminalGeometry,
) -> std::result::Result<(), ClientError> {
    if geometry.cols == 0 || geometry.rows == 0 {
        return Ok(());
    }
    let (status_top_inset, status_bottom_inset) = status_insets_for_position(status_position);
    client
        .attach_set_viewport_with_insets(
            session_id,
            geometry.cols,
            geometry.rows,
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

async fn apply_attach_structured_grid_deltas(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    pane_ids: Vec<Uuid>,
) -> std::result::Result<bool, ClientError> {
    if pane_ids.is_empty() {
        return Ok(false);
    }
    let base_revisions = pane_ids
        .iter()
        .map(|pane_id| {
            view_state
                .pane_buffers
                .get(pane_id)
                .map_or(0, |buffer| buffer.terminal_grid.grid().revision())
        })
        .collect::<Vec<_>>();
    let deltas = match attach_pane_grid_delta_state_streaming(
        client,
        view_state.attached_id,
        pane_ids,
        base_revisions,
        ATTACH_GRID_DELTA_MAX_BATCHES_PER_PANE,
    )
    .await
    {
        Ok(deltas) => deltas,
        Err(error) => {
            tracing::debug!(%error, "structured pane grid delta unavailable; structured snapshot recovery will be attempted by caller");
            return Ok(false);
        }
    };

    let mut needs_snapshot = Vec::new();
    let mut applied_any = false;
    for delta_result in deltas {
        if delta_result.desynced {
            needs_snapshot.push(delta_result.pane_id);
            continue;
        }
        if delta_result.encoded.is_empty() {
            continue;
        }
        let batches = match serde_json::from_slice::<Vec<bmux_terminal_grid::GridDeltaBatch>>(
            &delta_result.encoded,
        ) {
            Ok(batches) => batches,
            Err(error) => {
                tracing::warn!(pane_id = %delta_result.pane_id, %error, "failed decoding structured pane grid deltas");
                needs_snapshot.push(delta_result.pane_id);
                continue;
            }
        };
        let mut pane_applied = false;
        {
            let buffer = view_state
                .pane_buffers
                .entry(delta_result.pane_id)
                .or_default();
            for batch in &batches {
                if let Err(error) = buffer
                    .terminal_grid
                    .apply_delta(batch, bmux_terminal_grid::GridLimits::default())
                {
                    tracing::warn!(pane_id = %delta_result.pane_id, %error, "failed applying structured pane grid delta");
                    needs_snapshot.push(delta_result.pane_id);
                    break;
                }
                pane_applied = true;
            }
            if pane_applied {
                buffer.prev_rows.clear();
            }
        }
        if pane_applied {
            applied_any = true;
            view_state
                .dirty
                .mark_pane_dirty(delta_result.pane_id, AttachDirtySource::PaneOutput);
        }
    }
    if !needs_snapshot.is_empty() {
        let _ =
            hydrate_attach_structured_grid_snapshots(client, view_state, needs_snapshot).await?;
        applied_any = true;
    }
    Ok(applied_any)
}

fn attach_grid_snapshot_max_rows_for_panes(
    view_state: &AttachViewState,
    pane_ids: &[Uuid],
) -> usize {
    let requested = pane_ids.iter().copied().collect::<BTreeSet<_>>();
    view_state
        .cached_layout_state
        .as_ref()
        .and_then(|layout_state| {
            layout_state
                .scene
                .surfaces
                .iter()
                .filter(|surface| {
                    surface.visible
                        && surface
                            .pane_id
                            .is_some_and(|pane_id| requested.contains(&pane_id))
                })
                .map(|surface| usize::from(surface.content_rect.h.max(1)))
                .max()
        })
        .unwrap_or(ATTACH_GRID_SNAPSHOT_FALLBACK_ROWS_PER_PANE)
}

async fn hydrate_attach_structured_grid_snapshots(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    pane_ids: Vec<Uuid>,
) -> std::result::Result<BTreeSet<Uuid>, ClientError> {
    if pane_ids.is_empty() {
        return Ok(BTreeSet::new());
    }
    let max_rows_per_pane = attach_grid_snapshot_max_rows_for_panes(view_state, &pane_ids);
    let snapshots = match attach_pane_grid_snapshot_state_streaming(
        client,
        view_state.attached_id,
        pane_ids,
        max_rows_per_pane,
    )
    .await
    {
        Ok(snapshots) => snapshots,
        Err(error) => {
            tracing::debug!(%error, "structured pane grid snapshot unavailable; pane rendering will wait for structured state");
            return Ok(BTreeSet::new());
        }
    };

    let mut hydrated = BTreeSet::new();
    for snapshot in snapshots {
        let decoded = match serde_json::from_slice::<bmux_terminal_grid::GridSnapshot>(
            &snapshot.encoded,
        ) {
            Ok(decoded) => decoded,
            Err(error) => {
                tracing::warn!(pane_id = %snapshot.pane_id, %error, "failed decoding structured pane grid snapshot");
                continue;
            }
        };
        let stream = match bmux_terminal_grid::TerminalGridStream::from_snapshot(
            &decoded,
            bmux_terminal_grid::GridLimits::default(),
        ) {
            Ok(stream) => stream,
            Err(error) => {
                tracing::warn!(pane_id = %snapshot.pane_id, %error, "failed hydrating structured pane grid snapshot");
                continue;
            }
        };
        let buffer = view_state.pane_buffers.entry(snapshot.pane_id).or_default();
        buffer.terminal_grid = stream;
        buffer.expected_stream_start = Some(snapshot.stream_end);
        buffer.scrollback_window = None;
        buffer.prev_rows.clear();
        hydrated.insert(snapshot.pane_id);
    }

    Ok(hydrated)
}

async fn ensure_focused_scrollback_window(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
) -> std::result::Result<(), ClientError> {
    if !view_state.scrollback_active {
        return Ok(());
    }
    let Some(pane_id) = focused_attach_pane_id(view_state) else {
        return Ok(());
    };
    let Some((_, rows)) = focused_attach_pane_inner_size(view_state) else {
        return Ok(());
    };
    let scrollback_offset = view_state.scrollback_offset;
    if view_state
        .pane_buffers
        .get(&pane_id)
        .and_then(|buffer| buffer.scrollback_window.as_ref())
        .is_some_and(|window| window.scrollback_offset == scrollback_offset)
    {
        return Ok(());
    }

    let windows = attach_pane_grid_window_state_streaming(
        client,
        view_state.attached_id,
        vec![PaneGridWindowRequest {
            pane_id,
            scrollback_offset,
            rows,
        }],
    )
    .await?;
    let Some(window) = windows.into_iter().find(|window| window.pane_id == pane_id) else {
        return Ok(());
    };
    let decoded = serde_json::from_slice::<bmux_terminal_grid::GridSnapshot>(&window.encoded)
        .map_err(|error| ClientError::ServerError {
            code: bmux_ipc::ErrorCode::Internal,
            message: format!("decoding structured scrollback window: {error}"),
        })?;
    let grid = bmux_terminal_grid::TerminalGrid::from_snapshot(
        &decoded,
        bmux_terminal_grid::GridLimits::default(),
    )
    .map_err(|error| ClientError::ServerError {
        code: bmux_ipc::ErrorCode::Internal,
        message: format!("hydrating structured scrollback window: {error}"),
    })?;
    if let Some(buffer) = view_state.pane_buffers.get_mut(&pane_id) {
        buffer.scrollback_window = Some(PaneScrollbackWindow {
            scrollback_offset: window.scrollback_offset,
            max_scrollback_offset: window.max_scrollback_offset,
            rows: grid.viewport_rows(),
        });
    }
    Ok(())
}

async fn handle_attach_mouse_scrollback_with_window(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    kind: MouseEventKind,
) -> std::result::Result<bool, ClientError> {
    let was_active = view_state.scrollback_active;
    let before_offset = view_state.scrollback_offset;
    let scroll_lines =
        isize::try_from(view_state.mouse.config.scroll_lines_per_tick.max(1)).unwrap_or(isize::MAX);
    let consumed = handle_attach_mouse_scrollback(view_state, kind);
    if !consumed {
        return Ok(false);
    }

    ensure_focused_scrollback_window(client, view_state).await?;

    if matches!(kind, MouseEventKind::ScrollUp)
        && !was_active
        && view_state.scrollback_active
        && view_state.scrollback_offset == before_offset
    {
        step_attach_scrollback(view_state, -scroll_lines);
        ensure_focused_scrollback_window(client, view_state).await?;
    }

    Ok(true)
}

async fn handle_attach_ui_action_with_scrollback(
    client: &mut StreamingBmuxClient,
    action: &RuntimeAction,
    view_state: &mut AttachViewState,
    now: Instant,
) -> std::result::Result<(), ClientError> {
    let needs_max_before = matches!(
        action,
        RuntimeAction::ScrollUpLine
            | RuntimeAction::ScrollUpPage
            | RuntimeAction::ScrollTop
            | RuntimeAction::MoveCursorUp
    ) && view_state.scrollback_active;
    if needs_max_before {
        ensure_focused_scrollback_window(client, view_state).await?;
    }

    handle_attach_ui_action_at(action, view_state, now);

    if view_state.scrollback_active {
        ensure_focused_scrollback_window(client, view_state).await?;
    }
    Ok(())
}

#[allow(clippy::too_many_lines)] // Snapshot hydration intentionally keeps related state transitions in one ordered flow.
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
        .attach_snapshot(
            view_state.attached_id,
            ATTACH_STRUCTURED_SNAPSHOT_MAX_BYTES_PER_PANE,
        )
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
        resize_attach_grids_for_scene(&mut view_state.pane_buffers, &layout_state.scene);
    }
    let structured_hydrated = hydrate_attach_structured_grid_snapshots(
        client,
        view_state,
        active_pane_ids.iter().copied().collect(),
    )
    .await?;
    for chunk in chunks {
        if structured_hydrated.contains(&chunk.pane_id) {
            if let Some(buffer) = view_state.pane_buffers.get_mut(&chunk.pane_id) {
                buffer.sync_update_in_progress = chunk.sync_update_active;
                buffer.expected_stream_start = Some(chunk.stream_end);
            }
            continue;
        }
        if !session_changed && !full_resync && retained_pane_ids.contains(&chunk.pane_id) {
            continue;
        }
        let _ = apply_attach_output_chunk_protocol_only(
            view_state,
            chunk.pane_id,
            &chunk.data,
            AttachOutputChunkMeta {
                stream_start: chunk.stream_start,
                stream_end: chunk.stream_end,
                stream_gap: chunk.stream_gap,
                sync_update_active: chunk.sync_update_active,
            },
        );
        if let Some(buffer) = view_state.pane_buffers.get_mut(&chunk.pane_id) {
            buffer.sync_update_in_progress = chunk.sync_update_active;
            buffer.expected_stream_start = Some(chunk.stream_end);
        }
    }
    view_state.dirty.layout_needs_refresh = false;
    view_state
        .dirty
        .mark_full_frame(AttachDirtySource::SnapshotHydration);
    view_state
        .dirty
        .mark_status_dirty(AttachDirtySource::SnapshotHydration);
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
            ATTACH_STRUCTURED_SNAPSHOT_MAX_BYTES_PER_PANE,
        )
        .await?;

    for pane_id in pane_ids {
        view_state
            .pane_buffers
            .insert(*pane_id, PaneRenderBuffer::default());
    }
    resize_attach_grids_for_scene(&mut view_state.pane_buffers, &layout_state.scene);

    let structured_hydrated =
        hydrate_attach_structured_grid_snapshots(client, view_state, pane_ids.to_vec()).await?;

    for chunk in chunks {
        if !requested_pane_ids.contains(&chunk.pane_id) {
            continue;
        }
        if structured_hydrated.contains(&chunk.pane_id) {
            if let Some(buffer) = view_state.pane_buffers.get_mut(&chunk.pane_id) {
                buffer.sync_update_in_progress = chunk.sync_update_active;
                buffer.expected_stream_start = Some(chunk.stream_end);
            }
            continue;
        }

        let _ = apply_attach_output_chunk_protocol_only(
            view_state,
            chunk.pane_id,
            &chunk.data,
            AttachOutputChunkMeta {
                stream_start: chunk.stream_start,
                stream_end: chunk.stream_end,
                stream_gap: chunk.stream_gap,
                sync_update_active: chunk.sync_update_active,
            },
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
        view_state
            .dirty
            .mark_pane_dirty(*pane_id, AttachDirtySource::SnapshotHydration);
    }

    Ok(())
}

pub fn attach_scene_revealed_pane_ids(
    previous: &AttachScene,
    next: &AttachScene,
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

fn current_attach_terminal_geometry() -> TerminalGeometry {
    let (cols, rows) = terminal::size().unwrap_or((0, 0));
    TerminalGeometry { cols, rows }
}

pub fn resize_attach_grids_for_scene(
    pane_buffers: &mut std::collections::BTreeMap<Uuid, attach::state::PaneRenderBuffer>,
    scene: &AttachScene,
) {
    let geometry = current_attach_terminal_geometry();
    resize_attach_grids_for_scene_with_size(pane_buffers, scene, geometry.cols, geometry.rows);
}

pub fn resize_attach_grids_for_scene_with_size(
    pane_buffers: &mut std::collections::BTreeMap<Uuid, attach::state::PaneRenderBuffer>,
    scene: &AttachScene,
    cols: u16,
    rows: u16,
) {
    bmux_attach_pipeline::reconcile::resize_attach_grids_for_scene_with_size(
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
    _follow_target_id: Option<Uuid>,
    _self_client_id: Option<Uuid>,
    _global: bool,
    help_lines: &[String],
    view_state: &mut AttachViewState,
    display_capture: &mut DisplayCaptureFanout,
    kernel_client_factory: Option<&KernelClientFactory>,
) -> Result<AttachLoopControl> {
    match event {
        AttachLoopEvent::Server(_server_event) => Ok(AttachLoopControl::Continue),
        AttachLoopEvent::Terminal(terminal_event) => {
            handle_attach_terminal_event(
                client,
                terminal_event,
                attach_input_processor,
                help_lines,
                view_state,
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
                kernel_client_factory,
            )
            .await
        }
    }
}

async fn handle_attached_session_removed(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    session_id: Uuid,
) -> Result<Option<AttachLoopControl>> {
    if session_id != view_state.attached_id {
        return Ok(None);
    }

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
        return Ok(Some(AttachLoopControl::Continue));
    }
    Ok(Some(AttachLoopControl::Break(
        AttachExitReason::StreamClosed,
    )))
}

async fn handle_control_catalog_changed(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    revision: u64,
    full_resync: bool,
) {
    if full_resync || revision > view_state.control_catalog_revision {
        if let Err(error) = refresh_attach_status_catalog(client, view_state).await {
            view_state.set_transient_status(
                format!("catalog refresh failed: {error:#}"),
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        } else if let Err(error) = reconcile_attached_session_from_catalog(client, view_state).await
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
    view_state
        .dirty
        .mark_status_dirty(AttachDirtySource::ControlCatalogChanged);
}

async fn handle_clients_plugin_event(
    client: &mut StreamingBmuxClient,
    follow_target_id: Option<Uuid>,
    self_client_id: Option<Uuid>,
    view_state: &mut AttachViewState,
    event: bmux_clients_plugin_api::clients_events::ClientEvent,
) -> Result<()> {
    match event {
        bmux_clients_plugin_api::clients_events::ClientEvent::FollowTargetChanged {
            follower_client_id,
            leader_client_id,
            context_id,
            session_id,
        } => {
            if Some(leader_client_id) != follow_target_id
                || Some(follower_client_id) != self_client_id
            {
                return Ok(());
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
                return Ok(());
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
                // Route the read-only notice to the status line; raw
                // mode is active and `println!` would overwrite pane content.
                view_state.set_transient_status(
                    "read-only attach: input disabled".to_string(),
                    Instant::now(),
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
            }
            view_state
                .dirty
                .mark_layout_frame_and_status_dirty(AttachDirtySource::FollowTargetChanged);
        }
        bmux_clients_plugin_api::clients_events::ClientEvent::FollowTargetGone {
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
            view_state
                .dirty
                .mark_status_dirty(AttachDirtySource::FollowTargetChanged);
        }
        _ => {}
    }
    Ok(())
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
                view_state
                    .dirty
                    .mark_layout_frame_and_status_dirty(AttachDirtySource::SceneChanged);
            }
            AttachViewComponent::SurfaceContent => {
                view_state
                    .dirty
                    .mark_layout_frame_dirty(AttachDirtySource::SceneChanged);
            }
            AttachViewComponent::Status => {
                view_state
                    .dirty
                    .mark_status_dirty(AttachDirtySource::StatusChanged);
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
    terminal_event: AttachTerminalEvent,
    attach_input_processor: &mut InputProcessor,
    help_lines: &[String],
    view_state: &mut AttachViewState,
    display_capture: &mut DisplayCaptureFanout,
    kernel_client_factory: Option<&KernelClientFactory>,
) -> Result<AttachLoopControl> {
    let now = terminal_event.received_at;
    let geometry = terminal_event.geometry;
    let normalized_event = terminal_event.normalized;
    let raw_event = terminal_event.raw;
    if matches!(&raw_event, Event::Resize(_, _)) {
        update_attach_viewport_with_geometry(
            client,
            view_state.attached_id,
            view_state.status_position,
            geometry,
        )
        .await?;
    }

    if view_state.prompt.is_active() {
        let prompt_disposition = match &normalized_event {
            Some(TerminalInputEvent::Key(key))
                if prompt_accepts_key_kind(key.kind.to_crossterm()) =>
            {
                Some(view_state.prompt.handle_terminal_key_event(key))
            }
            Some(TerminalInputEvent::Key(_) | TerminalInputEvent::Bytes(_)) => {
                Some(PromptKeyDisposition::NotActive)
            }
            Some(TerminalInputEvent::Mouse(mouse)) => Some(
                view_state
                    .prompt
                    .handle_terminal_mouse_event(*mouse, geometry),
            ),
            _ => match &raw_event {
                Event::Key(key) if prompt_accepts_key_kind(key.kind) => {
                    Some(view_state.prompt.handle_key_event(key))
                }
                Event::Key(_) | Event::Paste(_) => Some(PromptKeyDisposition::NotActive),
                Event::Mouse(mouse) => Some(view_state.prompt.handle_mouse_event(*mouse, geometry)),
                _ => None,
            },
        };

        if let Some(disposition) = prompt_disposition {
            match disposition {
                PromptKeyDisposition::Completed(completion) => {
                    if let Some(control) =
                        handle_attach_prompt_completion_at(client, view_state, completion, now)
                            .await?
                    {
                        return Ok(control);
                    }
                }
                PromptKeyDisposition::Consumed => {
                    view_state
                        .dirty
                        .mark_overlay_dirty(AttachDirtySource::PromptOverlay);
                }
                PromptKeyDisposition::NotActive => {}
            }
            return Ok(AttachLoopControl::Continue);
        }
    }

    if view_state.help_overlay_open {
        let handled = match &normalized_event {
            Some(TerminalInputEvent::Key(key)) => {
                handle_help_overlay_terminal_key_event(key, help_lines, view_state, geometry)
            }
            _ => {
                matches!(&raw_event, Event::Key(key) if handle_help_overlay_key_event(key, help_lines, view_state, geometry))
            }
        };
        if handled {
            return Ok(AttachLoopControl::Continue);
        }
    }

    if matches!(&raw_event, Event::Key(_)) {
        let focused_input_mode = focused_attach_pane_input_mode(view_state);
        attach_input_processor.set_pane_input_mode(
            focused_input_mode.application_cursor,
            focused_input_mode.application_keypad,
        );
    }

    let attach_actions = if let Some(normalized_event) = &normalized_event {
        attach_terminal_input_event_actions(
            normalized_event,
            attach_input_processor,
            view_state.ui_mode,
        )?
    } else {
        attach_event_actions(&raw_event, attach_input_processor, view_state.ui_mode)?
    };

    for attach_action in attach_actions {
        match attach_action {
            AttachEventAction::Detach => {
                return try_detach_or_continue_at(client, view_state, now).await;
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
                    if let Err(error) =
                        send_attach_bytes_to_focused_pane(client, view_state, bytes).await
                    {
                        return Err(map_attach_client_error(error));
                    }
                    display_capture.record_activity(DisplayActivityKind::Input);
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
                    active_keybindings_for_context(
                        attach_input_processor.keymap(),
                        attach_input_processor.active_mode_id(),
                        view_state.scrollback_active,
                    ),
                )
                .await
                {
                    view_state.set_transient_status(
                        format!("plugin action failed: {}", map_attach_client_error(error)),
                        now,
                        ATTACH_TRANSIENT_STATUS_TTL,
                    );
                }
                attach_input_processor.set_scroll_mode(view_state.scrollback_active);
            }
            AttachEventAction::Mouse(mouse_event) => {
                if let Err(error) = handle_attach_mouse_event_at(
                    client,
                    mouse_event,
                    view_state,
                    kernel_client_factory,
                    now,
                    geometry,
                )
                .await
                {
                    view_state.set_transient_status(
                        format!("mouse action failed: {}", map_attach_client_error(error)),
                        now,
                        ATTACH_TRANSIENT_STATUS_TTL,
                    );
                }
                if let Err(error) = ensure_focused_scrollback_window(client, view_state).await {
                    view_state.set_transient_status(
                        format!(
                            "scrollback window fetch failed: {}",
                            map_attach_client_error(error)
                        ),
                        now,
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
                            view_state.dirty.mark_layout_frame_and_status_dirty(
                                AttachDirtySource::ProfileChanged,
                            );
                        }
                        Err(error) => {
                            view_state.set_transient_status(
                                format!("profile switch failed: {error}"),
                                now,
                                ATTACH_TRANSIENT_STATUS_TTL,
                            );
                        }
                    }
                    continue;
                }
                if matches!(action, RuntimeAction::ShowHelp) {
                    view_state.help_overlay_open = !view_state.help_overlay_open;
                    if !view_state.help_overlay_open {
                        view_state.help_overlay_scroll = 0;
                    }
                    view_state
                        .dirty
                        .mark_overlay_dirty(AttachDirtySource::HelpOverlay);
                    view_state
                        .dirty
                        .mark_status_dirty(AttachDirtySource::HelpOverlay);
                    continue;
                }
                if view_state.help_overlay_open {
                    if matches!(action, RuntimeAction::ExitMode)
                        || matches!(action, RuntimeAction::ForwardToPane(_))
                    {
                        view_state.help_overlay_open = false;
                        view_state.help_overlay_scroll = 0;
                        view_state
                            .dirty
                            .mark_status_dirty(AttachDirtySource::HelpOverlay);
                        view_state
                            .dirty
                            .mark_overlay_dirty(AttachDirtySource::HelpOverlay);
                    }
                    continue;
                }
                let prompt_only_action = matches!(action, RuntimeAction::Quit)
                    || is_windows_close_active_pane_action(&action);
                if let Err(error) =
                    handle_attach_ui_action_with_scrollback(client, &action, view_state, now).await
                {
                    view_state.set_transient_status(
                        format!(
                            "scrollback window fetch failed: {}",
                            map_attach_client_error(error)
                        ),
                        now,
                        ATTACH_TRANSIENT_STATUS_TTL,
                    );
                }
                if prompt_only_action && view_state.prompt.is_active() {
                    view_state
                        .dirty
                        .mark_overlay_dirty(AttachDirtySource::PromptOverlay);
                } else {
                    view_state
                        .dirty
                        .mark_layout_frame_dirty(AttachDirtySource::UserAction);
                }
                attach_input_processor.set_scroll_mode(view_state.scrollback_active);
                view_state
                    .dirty
                    .mark_status_dirty(AttachDirtySource::UserAction);
            }
            AttachEventAction::Redraw => {
                view_state
                    .dirty
                    .mark_status_dirty(AttachDirtySource::ManualRedraw);
                view_state
                    .dirty
                    .mark_layout_refresh(AttachDirtySource::ManualRedraw);
                view_state
                    .dirty
                    .mark_full_frame(AttachDirtySource::ManualRedraw);
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

const fn prompt_response_single_value(response: &PromptResponse) -> Option<&str> {
    match response {
        PromptResponse::Submitted(PromptValue::Single(value)) => Some(value.as_str()),
        _ => None,
    }
}

async fn retarget_attach_to_session(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    session_id: Uuid,
) -> std::result::Result<(), ClientError> {
    let attach_info = open_attach_for_session(client, session_id).await?;
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
    Ok(())
}

async fn kill_session_for_safe_close(
    client: &mut StreamingBmuxClient,
    session_id: Uuid,
) -> std::result::Result<(), ClientError> {
    match typed_kill_session_attach(client, SessionSelector::ById(session_id), false).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(err)) => Err(ClientError::ServerError {
            code: bmux_ipc::ErrorCode::Internal,
            message: format!("kill-session failed: {err:?}"),
        }),
        Err(error) => Err(error),
    }
}

async fn close_pane_by_id_for_prompt(
    client: &mut StreamingBmuxClient,
    pane_id: Uuid,
) -> std::result::Result<(), ClientError> {
    let _ack: bmux_windows_plugin_api::windows_commands::PaneAck = invoke_windows_command(
        client,
        "focus-pane",
        &windows_commands::client::FocusPaneRequest { id: pane_id },
    )
    .await?;
    let _ack: bmux_windows_plugin_api::windows_commands::PaneAck = invoke_windows_command(
        client,
        "close-pane",
        &windows_commands::client::ClosePaneRequest { id: pane_id },
    )
    .await?;
    Ok(())
}

async fn close_last_pane_after_retarget(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    old_session_id: Uuid,
    target: AttachCloseFallbackTarget,
) -> std::result::Result<(), ClientError> {
    match target {
        AttachCloseFallbackTarget::Context { context_id } => {
            retarget_attach_to_context(client, view_state, context_id).await?;
        }
        AttachCloseFallbackTarget::Session { session_id } => {
            retarget_attach_to_session(client, view_state, session_id).await?;
        }
    }
    kill_session_for_safe_close(client, old_session_id).await
}

async fn create_new_window_and_retarget(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
) -> std::result::Result<(), ClientError> {
    let ack: bmux_windows_plugin_api::windows_commands::WindowAck = invoke_windows_command(
        client,
        "new-window",
        &windows_commands::client::NewWindowRequest { name: None },
    )
    .await?;
    let context_id = ack
        .id
        .as_deref()
        .and_then(|id| Uuid::parse_str(id).ok())
        .ok_or_else(|| ClientError::ServerError {
            code: bmux_ipc::ErrorCode::Internal,
            message: "new-window did not return a context id".to_string(),
        })?;
    retarget_attach_to_context(client, view_state, context_id).await
}

async fn split_new_pane_then_close_old(
    client: &mut StreamingBmuxClient,
    pane_id: Uuid,
    session_id: Uuid,
) -> std::result::Result<(), ClientError> {
    let _ack: bmux_windows_plugin_api::windows_commands::PaneAck = invoke_windows_command(
        client,
        "split-pane",
        &windows_commands::client::SplitPaneRequest {
            session: Some(ipc_to_windows_selector(SessionSelector::ById(session_id))),
            target: Some(pane_id_windows_selector(pane_id)),
            direction: windows_commands::PaneDirection::Horizontal,
            ratio_pct: None,
        },
    )
    .await?;
    close_pane_by_id_for_prompt(client, pane_id).await
}

fn set_close_prompt_error_at(view_state: &mut AttachViewState, error: ClientError, now: Instant) {
    view_state.set_transient_status(
        format!("close pane failed: {}", map_attach_client_error(error)),
        now,
        ATTACH_TRANSIENT_STATUS_TTL,
    );
}

#[allow(clippy::too_many_lines)] // Internal prompt completions are a compact action state machine.
pub async fn handle_attach_prompt_completion_at(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    completion: AttachPromptCompletion,
    now: Instant,
) -> std::result::Result<Option<AttachLoopControl>, ClientError> {
    let mut requires_layout_refresh = false;
    match completion.origin {
        AttachPromptOrigin::External { response_tx, .. } => {
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
                                now,
                                ATTACH_TRANSIENT_STATUS_TTL,
                            );
                        }
                        Err(error) => {
                            let status = attach_quit_failure_status(&error);
                            view_state.set_transient_status(
                                status,
                                now,
                                ATTACH_TRANSIENT_STATUS_TTL,
                            );
                        }
                    }
                } else {
                    view_state.set_transient_status(
                        "quit canceled",
                        now,
                        ATTACH_TRANSIENT_STATUS_TTL,
                    );
                }
            }
            AttachInternalPromptAction::ClosePane { pane_id } => {
                if prompt_response_is_confirmed(&completion.response) {
                    match close_pane_by_id_for_prompt(client, pane_id).await {
                        Ok(()) => {
                            view_state.set_transient_status(
                                "pane closed",
                                now,
                                ATTACH_TRANSIENT_STATUS_TTL,
                            );
                            requires_layout_refresh = true;
                        }
                        Err(error) => set_close_prompt_error_at(view_state, error, now),
                    }
                } else {
                    view_state.set_transient_status(
                        "close pane canceled",
                        now,
                        ATTACH_TRANSIENT_STATUS_TTL,
                    );
                }
            }
            AttachInternalPromptAction::CloseLastPaneAndSwitch {
                old_session_id,
                target,
            } => {
                if prompt_response_is_confirmed(&completion.response) {
                    match close_last_pane_after_retarget(client, view_state, old_session_id, target)
                        .await
                    {
                        Ok(()) => {
                            view_state.set_transient_status(
                                "pane closed; switched target",
                                now,
                                ATTACH_TRANSIENT_STATUS_TTL,
                            );
                            requires_layout_refresh = true;
                        }
                        Err(error) => set_close_prompt_error_at(view_state, error, now),
                    }
                } else {
                    view_state.set_transient_status(
                        "close pane canceled",
                        now,
                        ATTACH_TRANSIENT_STATUS_TTL,
                    );
                }
            }
            AttachInternalPromptAction::FinalPaneAction {
                pane_id,
                session_id,
            } => match prompt_response_single_value(&completion.response) {
                Some(FINAL_PANE_CHOICE_NEW_PANE) => {
                    match split_new_pane_then_close_old(client, pane_id, session_id).await {
                        Ok(()) => {
                            view_state.set_transient_status(
                                "new pane opened; old pane closed",
                                now,
                                ATTACH_TRANSIENT_STATUS_TTL,
                            );
                            requires_layout_refresh = true;
                        }
                        Err(error) => set_close_prompt_error_at(view_state, error, now),
                    }
                }
                Some(FINAL_PANE_CHOICE_NEW_SESSION) => {
                    let result = match create_new_window_and_retarget(client, view_state).await {
                        Ok(()) => kill_session_for_safe_close(client, session_id).await,
                        Err(error) => Err(error),
                    };
                    match result {
                        Ok(()) => {
                            view_state.set_transient_status(
                                "new session opened; old pane closed",
                                now,
                                ATTACH_TRANSIENT_STATUS_TTL,
                            );
                            requires_layout_refresh = true;
                        }
                        Err(error) => set_close_prompt_error_at(view_state, error, now),
                    }
                }
                Some(FINAL_PANE_CHOICE_QUIT) => {
                    match typed_kill_session_attach(
                        client,
                        SessionSelector::ById(session_id),
                        false,
                    )
                    .await
                    {
                        Ok(Ok(_)) => {
                            return Ok(Some(AttachLoopControl::Break(AttachExitReason::Quit)));
                        }
                        Ok(Err(err)) => set_close_prompt_error_at(
                            view_state,
                            ClientError::ServerError {
                                code: bmux_ipc::ErrorCode::Internal,
                                message: format!("kill-session failed: {err:?}"),
                            },
                            now,
                        ),
                        Err(error) => set_close_prompt_error_at(view_state, error, now),
                    }
                }
                _ => {
                    view_state.set_transient_status(
                        "close pane canceled",
                        now,
                        ATTACH_TRANSIENT_STATUS_TTL,
                    );
                }
            },
        },
    }

    view_state
        .dirty
        .mark_status_dirty(AttachDirtySource::PromptOverlay);
    view_state
        .dirty
        .mark_overlay_dirty(AttachDirtySource::PromptOverlay);
    if requires_layout_refresh {
        view_state
            .dirty
            .mark_layout_frame_dirty(AttachDirtySource::PromptOverlay);
    }
    Ok(None)
}

/// Handle an action dispatch request from async plugin code.
///
/// Parses the action string into a `RuntimeAction` and routes it through
/// the same dispatch path as keybinding-triggered actions.
async fn handle_attach_action_dispatch(
    client: &mut StreamingBmuxClient,
    dispatch_request: bmux_plugin_sdk::ActionDispatchRequest,
    view_state: &mut AttachViewState,
    kernel_client_factory: Option<&KernelClientFactory>,
) -> Result<AttachLoopControl> {
    let now = SystemAttachClock.now();
    let action_str = &dispatch_request.action;
    let action = match parse_action(action_str) {
        Ok(action) => action,
        Err(error) => {
            view_state.set_transient_status(
                format!("invalid dispatched action: {error}"),
                now,
                ATTACH_TRANSIENT_STATUS_TTL,
            );
            return Ok(AttachLoopControl::Continue);
        }
    };

    let event_action = runtime_action_to_attach_event_action(action);

    match event_action {
        AttachEventAction::Detach => {
            return try_detach_or_continue_at(client, view_state, now).await;
        }
        AttachEventAction::Send(bytes) => {
            if view_state.can_write
                && let Err(error) =
                    send_attach_bytes_to_focused_pane(client, view_state, bytes).await
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
                Vec::new(),
            )
            .await
            {
                view_state.set_transient_status(
                    format!(
                        "dispatched plugin action failed: {}",
                        map_attach_client_error(error)
                    ),
                    now,
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
            }
        }
        AttachEventAction::Ui(ui_action) => {
            if let Err(error) =
                handle_attach_ui_action_with_scrollback(client, &ui_action, view_state, now).await
            {
                view_state.set_transient_status(
                    format!(
                        "scrollback window fetch failed: {}",
                        map_attach_client_error(error)
                    ),
                    now,
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
            }
            view_state
                .dirty
                .mark_layout_frame_dirty(AttachDirtySource::ActionDispatch);
            view_state
                .dirty
                .mark_status_dirty(AttachDirtySource::ActionDispatch);
        }
        AttachEventAction::Redraw => {
            view_state
                .dirty
                .mark_layout_frame_and_status_dirty(AttachDirtySource::ManualRedraw);
        }
        AttachEventAction::Mouse(_) | AttachEventAction::Ignore => {}
    }

    Ok(AttachLoopControl::Continue)
}

async fn try_detach_or_continue_at(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    now: Instant,
) -> Result<AttachLoopControl> {
    match client.detach().await {
        Ok(()) => Ok(AttachLoopControl::Break(AttachExitReason::Detached)),
        Err(error) => {
            view_state.set_transient_status(
                format!("detach blocked: {}", map_attach_client_error(error)),
                now,
                ATTACH_TRANSIENT_STATUS_TTL,
            );
            Ok(AttachLoopControl::Continue)
        }
    }
}

pub const fn record_attach_mouse_event(
    mouse_event: MouseEvent,
    view_state: &mut AttachViewState,
    now: Instant,
) {
    view_state.mouse.last_position = Some((mouse_event.column, mouse_event.row));
    view_state.mouse.last_event_at = Some(now);
}

#[allow(clippy::too_many_lines)]
async fn handle_attach_mouse_event_at(
    client: &mut StreamingBmuxClient,
    mouse_event: MouseEvent,
    view_state: &mut AttachViewState,
    kernel_client_factory: Option<&KernelClientFactory>,
    now: Instant,
    geometry: TerminalGeometry,
) -> std::result::Result<(), ClientError> {
    record_attach_mouse_event(mouse_event, view_state, now);

    if !view_state.mouse.config.enabled {
        view_state.mouse.resize_drag = None;
        view_state.mouse.floating_drag = None;
        view_state.mouse.tab_drag = None;
        return Ok(());
    }
    if view_state.help_overlay_open || view_state.prompt.is_active() {
        view_state.mouse.resize_drag = None;
        view_state.mouse.floating_drag = None;
        view_state.mouse.tab_drag = None;
        return Ok(());
    }

    if handle_attach_status_tab_mouse_event_at(
        client,
        view_state,
        mouse_event,
        kernel_client_factory,
        now,
        geometry,
    )
    .await?
    {
        return Ok(());
    }

    if !view_state.can_write {
        view_state.mouse.resize_drag = None;
        view_state.mouse.floating_drag = None;
        view_state.mouse.tab_drag = None;
        return Ok(());
    }

    let resize_reduction =
        reduce_attach_mouse_resize_event(view_state, TerminalMouseEvent::from(mouse_event), now);
    if resize_reduction.consumed {
        execute_attach_ui_effects(
            client,
            view_state,
            resize_reduction.effects,
            kernel_client_factory,
            now,
        )
        .await?;
        return Ok(());
    }

    let floating_reduction =
        reduce_attach_mouse_floating_drag_event(view_state, TerminalMouseEvent::from(mouse_event));
    if floating_reduction.consumed {
        execute_attach_ui_effects(
            client,
            view_state,
            floating_reduction.effects,
            kernel_client_factory,
            now,
        )
        .await?;
        return Ok(());
    }

    let target_pane = attach_scene_pane_at(view_state, mouse_event.column, mouse_event.row);
    let focused_pane = view_state
        .cached_layout_state
        .as_ref()
        .map(|layout| layout.focused_pane_id);
    let in_focused_pane = target_pane.is_some() && target_pane == focused_pane;

    if matches!(
        mouse_event.kind,
        MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
    ) && handle_attach_mouse_selection_drag_at(view_state, mouse_event, now)
    {
        return Ok(());
    }

    if matches!(mouse_event.kind, MouseEventKind::ScrollUp)
        && handle_attach_mouse_gesture_action(
            client,
            view_state,
            "scroll_up",
            kernel_client_factory,
            now,
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
            now,
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
            bmux_config::MouseWheelPropagation::Auto => {
                let _ = handle_attach_mouse_wheel_auto(
                    client,
                    view_state,
                    mouse_event,
                    target_pane,
                    in_focused_pane,
                )
                .await?;
                return Ok(());
            }
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
                let _ = handle_attach_mouse_scrollback_with_window(
                    client,
                    view_state,
                    mouse_event.kind,
                )
                .await?;
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
                let _ = handle_attach_mouse_scrollback_with_window(
                    client,
                    view_state,
                    mouse_event.kind,
                )
                .await?;
                return Ok(());
            }
        }
    }

    match mouse_event.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let target = target_pane;
            view_state.mouse.hovered_pane_id = target;
            view_state.mouse.hover_started_at = Some(now);
            if !handle_attach_mouse_gesture_action(
                client,
                view_state,
                "click_left",
                kernel_client_factory,
                now,
            )
            .await?
            {
                if maybe_begin_attach_mouse_selection_drag(view_state, target, mouse_event) {
                    return Ok(());
                }

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
                        now,
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

pub fn reduce_attach_mouse_floating_drag_event(
    view_state: &mut AttachViewState,
    mouse_event: TerminalMouseEvent,
) -> AttachUiReduction {
    match (mouse_event.phase, mouse_event.button) {
        (TerminalMousePhase::Down, Some(TerminalMouseButton::Left)) => {
            let Some(surface) =
                attach_scene_floating_drag_surface_at(view_state, mouse_event.col, mouse_event.row)
            else {
                return AttachUiReduction::ignored();
            };
            let Some(pane_id) = surface.pane_id else {
                return AttachUiReduction::ignored();
            };
            view_state.mouse.selection_drag = None;
            let (scene_max_x, scene_max_y) =
                attach_scene_bounds(view_state).unwrap_or((u16::MAX, u16::MAX));
            view_state.mouse.floating_drag = Some(AttachMouseFloatingDrag {
                pane_id,
                start_x: surface.rect.x,
                start_y: surface.rect.y,
                width: surface.rect.w,
                height: surface.rect.h,
                scene_max_x,
                scene_max_y,
                last_x: surface.rect.x,
                last_y: surface.rect.y,
                start_column: mouse_event.col,
                start_row: mouse_event.row,
            });
            AttachUiReduction::with_effect(AttachUiEffect::FocusPane { pane_id })
        }
        (TerminalMousePhase::Drag, Some(TerminalMouseButton::Left)) => {
            let Some(mut drag) = view_state.mouse.floating_drag.take() else {
                return AttachUiReduction::ignored();
            };
            let effect = floating_drag_move_effect(&mut drag, mouse_event.col, mouse_event.row);
            view_state.mouse.floating_drag = Some(drag);
            AttachUiReduction {
                consumed: true,
                effects: effect.into_iter().collect(),
            }
        }
        (TerminalMousePhase::Up, Some(TerminalMouseButton::Left)) => {
            let Some(mut drag) = view_state.mouse.floating_drag.take() else {
                return AttachUiReduction::ignored();
            };
            AttachUiReduction {
                consumed: true,
                effects: floating_drag_move_effect(&mut drag, mouse_event.col, mouse_event.row)
                    .into_iter()
                    .collect(),
            }
        }
        _ => {
            if view_state.mouse.floating_drag.is_some() {
                AttachUiReduction::consumed()
            } else {
                AttachUiReduction::ignored()
            }
        }
    }
}

fn floating_drag_move_effect(
    drag: &mut AttachMouseFloatingDrag,
    column: u16,
    row: u16,
) -> Option<AttachUiEffect> {
    let (x, y) = floating_drag_position(*drag, column, row);
    if x == drag.last_x && y == drag.last_y {
        return None;
    }
    drag.last_x = x;
    drag.last_y = y;
    Some(AttachUiEffect::MoveFloatingPane {
        pane_id: drag.pane_id,
        x,
        y,
    })
}

pub fn reduce_attach_mouse_resize_event(
    view_state: &mut AttachViewState,
    mouse_event: TerminalMouseEvent,
    now: Instant,
) -> AttachUiReduction {
    if !view_state.mouse.config.resize_borders {
        view_state.mouse.resize_drag = None;
        return AttachUiReduction::ignored();
    }

    match (mouse_event.phase, mouse_event.button) {
        (TerminalMousePhase::Down, Some(TerminalMouseButton::Left)) => {
            let Some(mut drag) =
                attach_scene_resize_separator_at(view_state, mouse_event.col, mouse_event.row, now)
            else {
                return AttachUiReduction::ignored();
            };
            let throttle = Duration::from_millis(view_state.mouse.config.resize_drag_throttle_ms);
            drag.last_applied_at = now.checked_sub(throttle).unwrap_or(now);
            view_state.mouse.resize_drag = Some(drag);
            AttachUiReduction::consumed()
        }
        (TerminalMousePhase::Drag, Some(TerminalMouseButton::Left)) => {
            let Some(mut drag) = view_state.mouse.resize_drag else {
                return AttachUiReduction::ignored();
            };
            drag.latest_column = mouse_event.col;
            drag.latest_row = mouse_event.row;
            if !attach_mouse_resize_drag_has_pending_delta(&drag) {
                view_state.mouse.resize_drag = Some(drag);
                return AttachUiReduction::consumed();
            }
            let throttle = Duration::from_millis(view_state.mouse.config.resize_drag_throttle_ms);
            if !throttle.is_zero() && now.duration_since(drag.last_applied_at) < throttle {
                view_state.mouse.resize_drag = Some(drag);
                return AttachUiReduction::consumed();
            }
            let effects = resize_effects_for_drag_delta(&mut drag);
            if !effects.is_empty() {
                drag.last_applied_at = now;
            }
            view_state.mouse.resize_drag = Some(drag);
            AttachUiReduction {
                consumed: true,
                effects,
            }
        }
        (TerminalMousePhase::Up, Some(TerminalMouseButton::Left)) => {
            let Some(mut drag) = view_state.mouse.resize_drag.take() else {
                return AttachUiReduction::ignored();
            };
            drag.latest_column = mouse_event.col;
            drag.latest_row = mouse_event.row;
            AttachUiReduction {
                consumed: true,
                effects: resize_effects_for_drag_delta(&mut drag),
            }
        }
        _ => {
            if view_state.mouse.resize_drag.is_some() {
                AttachUiReduction::consumed()
            } else {
                AttachUiReduction::ignored()
            }
        }
    }
}

fn attach_mouse_resize_drag_has_pending_delta(drag: &AttachMouseResizeDrag) -> bool {
    resize_drag_axis_delta(
        drag.horizontal,
        i32::from(drag.latest_column) - i32::from(drag.last_column),
    )
    .is_some()
        || resize_drag_axis_delta(
            drag.vertical,
            i32::from(drag.latest_row) - i32::from(drag.last_row),
        )
        .is_some()
}

fn resize_effects_for_drag_delta(drag: &mut AttachMouseResizeDrag) -> Vec<AttachUiEffect> {
    let mut effects = Vec::new();
    if let Some((pane_id, direction, cells)) = resize_drag_axis_delta(
        drag.horizontal,
        i32::from(drag.latest_column) - i32::from(drag.last_column),
    ) {
        effects.push(AttachUiEffect::ResizePane {
            pane_id,
            direction,
            cells,
        });
        drag.last_column = drag.latest_column;
    }
    if let Some((pane_id, direction, cells)) = resize_drag_axis_delta(
        drag.vertical,
        i32::from(drag.latest_row) - i32::from(drag.last_row),
    ) {
        effects.push(AttachUiEffect::ResizePane {
            pane_id,
            direction,
            cells,
        });
        drag.last_row = drag.latest_row;
    }
    effects
}

fn resize_drag_axis_delta(
    axis_drag: Option<AttachMouseResizeAxisDrag>,
    delta: i32,
) -> Option<(
    Uuid,
    bmux_windows_plugin_api::windows_commands::PaneResizeDirection,
    u16,
)> {
    let axis_drag = axis_drag?;
    if delta == 0 {
        return None;
    }
    let cells = u16::try_from(delta.unsigned_abs()).unwrap_or(u16::MAX);
    if delta > 0 {
        Some((
            axis_drag.positive_target_pane_id,
            axis_drag.positive_direction,
            cells,
        ))
    } else {
        Some((
            axis_drag.negative_target_pane_id,
            axis_drag.negative_direction,
            cells,
        ))
    }
}

pub const fn should_forward_click_like_mouse(view_state: &AttachViewState) -> bool {
    !matches!(
        view_state.mouse.config.effective_click_propagation(),
        bmux_config::MouseClickPropagation::FocusOnly
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttachPaneMouseProtocol {
    pub mode: AttachMouseProtocolMode,
    pub encoding: AttachMouseProtocolEncoding,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct AttachPaneInputMode {
    pub application_cursor: bool,
    pub application_keypad: bool,
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
    let structured_mode = view_state.pane_buffers.get(&pane_id).map(|buffer| {
        let protocol = buffer.protocol_tracker.protocol_state();
        AttachPaneInputMode {
            application_cursor: protocol.application_cursor,
            application_keypad: protocol.application_keypad,
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

    match (structured_mode, hint_mode) {
        (Some(structured), Some(hint)) => Some(AttachPaneInputMode {
            application_cursor: structured.application_cursor || hint.application_cursor,
            application_keypad: structured.application_keypad || hint.application_keypad,
        }),
        (Some(structured), None) => Some(structured),
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
    mode: AttachMouseProtocolMode,
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

    client
        .pane_direct_input(view_state.attached_id, target_pane, bytes)
        .await?;
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
) -> Option<AttachRect> {
    let layout_state = view_state.cached_layout_state.as_ref()?;
    let mut best: Option<(SurfaceLayer, i32, usize, uuid::Uuid, AttachRect)> = None;
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
            return Some(AttachRect {
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

async fn handle_attach_status_tab_mouse_event_at(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    mouse_event: MouseEvent,
    kernel_client_factory: Option<&KernelClientFactory>,
    now: Instant,
    geometry: TerminalGeometry,
) -> std::result::Result<bool, ClientError> {
    let reduction = reduce_attach_status_tab_mouse_event(
        view_state,
        TerminalMouseEvent::from(mouse_event),
        geometry,
    );
    if !reduction.consumed {
        return Ok(false);
    }
    execute_attach_ui_effects(
        client,
        view_state,
        reduction.effects,
        kernel_client_factory,
        now,
    )
    .await?;
    Ok(true)
}

pub fn reduce_attach_status_tab_mouse_event(
    view_state: &mut AttachViewState,
    mouse_event: TerminalMouseEvent,
    geometry: TerminalGeometry,
) -> AttachUiReduction {
    match (mouse_event.phase, mouse_event.button) {
        (TerminalMousePhase::Down, Some(TerminalMouseButton::Left)) => {
            let Some(source_context_id) =
                attach_status_tab_context_at(view_state, mouse_event, geometry)
            else {
                view_state.mouse.tab_drag = None;
                return AttachUiReduction::ignored();
            };

            if !view_state.mouse.tab_drag_enabled {
                trace!("attach.status_tab_drag.disabled.mru_order");
                return AttachUiReduction::with_effect(AttachUiEffect::SwitchWindow {
                    target_context_id: source_context_id,
                });
            }

            let drop_target = view_state
                .cached_status_line
                .as_ref()
                .and_then(|status_line| {
                    resolve_attach_tab_drop_target(status_line, mouse_event.col)
                });
            view_state.mouse.tab_drag = Some(AttachMouseTabDrag {
                source_context_id,
                started_col: mouse_event.col,
                started_row: mouse_event.row,
                active: false,
                drop_target,
            });
            view_state.dirty.mark_status_dirty(AttachDirtySource::Mouse);
            AttachUiReduction::consumed()
        }
        (TerminalMousePhase::Drag | TerminalMousePhase::Move, _) => {
            if view_state.mouse.tab_drag.is_none() {
                return AttachUiReduction::ignored();
            }
            let drop_target = if attach_status_mouse_row_matches(view_state, mouse_event, geometry)
            {
                view_state
                    .cached_status_line
                    .as_ref()
                    .and_then(|status_line| {
                        resolve_attach_tab_drop_target(status_line, mouse_event.col)
                    })
            } else {
                None
            };
            if let Some(drag) = view_state.mouse.tab_drag.as_mut() {
                drag.active |= attach_tab_drag_motion_is_active(drag, mouse_event, drop_target);
                drag.drop_target = drop_target;
            }
            view_state.dirty.mark_status_dirty(AttachDirtySource::Mouse);
            AttachUiReduction::consumed()
        }
        (TerminalMousePhase::Up, Some(TerminalMouseButton::Left)) => {
            let Some(mut drag) = view_state.mouse.tab_drag.take() else {
                return AttachUiReduction::ignored();
            };
            let drop_target = if attach_status_mouse_row_matches(view_state, mouse_event, geometry)
            {
                view_state
                    .cached_status_line
                    .as_ref()
                    .and_then(|status_line| {
                        resolve_attach_tab_drop_target(status_line, mouse_event.col)
                    })
            } else {
                None
            }
            .or_else(|| drag.drop_target.take());

            let drag_active = attach_tab_drag_motion_is_active(&drag, mouse_event, drop_target);
            view_state
                .dirty
                .mark_layout_frame_and_status_dirty(AttachDirtySource::Mouse);
            if let (true, Some(target)) = (drag_active, drop_target) {
                if !view_state.can_write {
                    AttachUiReduction::with_effect(AttachUiEffect::ShowTransientStatus {
                        message: "tab reorder unavailable in read-only attach".to_string(),
                    })
                } else if target.context_id == drag.source_context_id {
                    AttachUiReduction::consumed()
                } else {
                    AttachUiReduction::with_effect(AttachUiEffect::MoveWindow {
                        source_context_id: drag.source_context_id,
                        target_context_id: target.context_id,
                        placement: target.placement,
                    })
                }
            } else if drag_active {
                AttachUiReduction::consumed()
            } else {
                AttachUiReduction::with_effect(AttachUiEffect::SwitchWindow {
                    target_context_id: drag.source_context_id,
                })
            }
        }
        _ => AttachUiReduction::ignored(),
    }
}

async fn execute_attach_ui_effects(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    effects: Vec<AttachUiEffect>,
    kernel_client_factory: Option<&KernelClientFactory>,
    now: Instant,
) -> std::result::Result<(), ClientError> {
    for effect in effects {
        match effect {
            AttachUiEffect::SwitchWindow { target_context_id } => {
                switch_attach_status_tab(
                    client,
                    view_state,
                    target_context_id,
                    kernel_client_factory,
                )
                .await?;
                view_state
                    .dirty
                    .mark_layout_frame_and_status_dirty(AttachDirtySource::Mouse);
            }
            AttachUiEffect::MoveWindow {
                source_context_id,
                target_context_id,
                placement,
            } => {
                move_attach_status_tab(client, source_context_id, target_context_id, placement)
                    .await?;
                optimistically_reorder_attach_window_list(
                    view_state,
                    source_context_id,
                    target_context_id,
                    placement,
                );
            }
            AttachUiEffect::ResizePane {
                pane_id,
                direction,
                cells,
            } => {
                if let Err(error) =
                    resize_attach_pane(client, view_state, pane_id, direction, cells).await
                {
                    tracing::warn!(
                        %error,
                        %pane_id,
                        ?direction,
                        cells,
                        "attach.mouse_resize.apply_failed"
                    );
                }
            }
            AttachUiEffect::FocusPane { pane_id } => {
                focus_attach_pane(client, view_state, pane_id).await?;
            }
            AttachUiEffect::MoveFloatingPane { pane_id, x, y } => {
                move_attach_floating_pane(client, view_state, pane_id, x, y).await?;
            }
            AttachUiEffect::ShowTransientStatus { message } => {
                view_state.set_transient_status(message, now, ATTACH_TRANSIENT_STATUS_TTL);
            }
        }
    }
    Ok(())
}

const fn attach_status_mouse_row_matches(
    view_state: &AttachViewState,
    mouse_event: TerminalMouseEvent,
    geometry: TerminalGeometry,
) -> bool {
    let Some(status_row) = status_row_for_position(view_state.status_position, geometry.rows)
    else {
        return false;
    };
    status_row_matches_mouse(status_row, mouse_event.row, geometry.rows)
}

fn attach_status_tab_context_at(
    view_state: &AttachViewState,
    mouse_event: TerminalMouseEvent,
    geometry: TerminalGeometry,
) -> Option<Uuid> {
    if !attach_status_mouse_row_matches(view_state, mouse_event, geometry) {
        return None;
    }
    view_state
        .cached_status_line
        .as_ref()
        .and_then(|status_line| {
            status_line
                .tab_hitboxes
                .iter()
                .find(|hitbox| {
                    mouse_event.col >= hitbox.start_col && mouse_event.col <= hitbox.end_col
                })
                .map(|hitbox| hitbox.context_id)
        })
}

pub fn resolve_attach_tab_drop_target(
    status_line: &AttachStatusLine,
    column: u16,
) -> Option<AttachTabDropTarget> {
    let mut hitboxes = status_line.tab_hitboxes.iter().collect::<Vec<_>>();
    hitboxes.sort_by_key(|hitbox| hitbox.start_col);
    let first = hitboxes.first()?;
    if column < first.start_col {
        return Some(AttachTabDropTarget {
            context_id: first.context_id,
            placement: AttachTabDropPlacement::Before,
        });
    }
    for hitbox in &hitboxes {
        if column < hitbox.start_col {
            return Some(AttachTabDropTarget {
                context_id: hitbox.context_id,
                placement: AttachTabDropPlacement::Before,
            });
        }
        if column <= hitbox.end_col {
            let width = hitbox
                .end_col
                .saturating_sub(hitbox.start_col)
                .saturating_add(1);
            let offset = column.saturating_sub(hitbox.start_col);
            let placement = if u32::from(offset).saturating_mul(2) < u32::from(width) {
                AttachTabDropPlacement::Before
            } else {
                AttachTabDropPlacement::After
            };
            return Some(AttachTabDropTarget {
                context_id: hitbox.context_id,
                placement,
            });
        }
    }
    hitboxes.last().map(|hitbox| AttachTabDropTarget {
        context_id: hitbox.context_id,
        placement: AttachTabDropPlacement::After,
    })
}

pub(super) fn attach_tab_drop_marker_col(
    status_line: &AttachStatusLine,
    target: AttachTabDropTarget,
    cols: u16,
) -> Option<u16> {
    let hitbox = status_line
        .tab_hitboxes
        .iter()
        .find(|hitbox| hitbox.context_id == target.context_id)?;
    let col = match target.placement {
        AttachTabDropPlacement::Before => hitbox.start_col,
        AttachTabDropPlacement::After => hitbox.end_col.saturating_add(1),
    };
    Some(col.min(cols.saturating_sub(1)))
}

fn attach_tab_drag_motion_is_active(
    drag: &AttachMouseTabDrag,
    mouse_event: TerminalMouseEvent,
    drop_target: Option<AttachTabDropTarget>,
) -> bool {
    drag.active
        || drop_target.is_some_and(|target| target.context_id != drag.source_context_id)
        || mouse_event
            .col
            .abs_diff(drag.started_col)
            .max(mouse_event.row.abs_diff(drag.started_row))
            > ATTACH_TAB_DRAG_THRESHOLD_CELLS
}

const fn attach_tab_drop_placement_api(
    placement: AttachTabDropPlacement,
) -> windows_commands::WindowMovePlacement {
    match placement {
        AttachTabDropPlacement::Before => windows_commands::WindowMovePlacement::Before,
        AttachTabDropPlacement::After => windows_commands::WindowMovePlacement::After,
    }
}

async fn move_attach_status_tab(
    client: &mut StreamingBmuxClient,
    source_context_id: Uuid,
    target_context_id: Uuid,
    placement: AttachTabDropPlacement,
) -> std::result::Result<(), ClientError> {
    let _ack: windows_commands::WindowAck = invoke_windows_command(
        client,
        "move-window",
        &windows_commands::client::MoveWindowRequest {
            source: source_context_id,
            target: target_context_id,
            placement: attach_tab_drop_placement_api(placement),
        },
    )
    .await?;
    Ok(())
}

fn optimistically_reorder_attach_window_list(
    view_state: &mut AttachViewState,
    source_context_id: Uuid,
    target_context_id: Uuid,
    placement: AttachTabDropPlacement,
) {
    if source_context_id == target_context_id {
        return;
    }

    let Some(snapshot) = view_state.cached_window_list.as_ref() else {
        return;
    };
    let mut next = snapshot.as_ref().clone();
    let Some(source_index) = next
        .windows
        .iter()
        .position(|entry| entry.id == source_context_id)
    else {
        return;
    };
    let source = next.windows.remove(source_index);
    let Some(target_index) = next
        .windows
        .iter()
        .position(|entry| entry.id == target_context_id)
    else {
        return;
    };
    let insert_index = match placement {
        AttachTabDropPlacement::Before => target_index,
        AttachTabDropPlacement::After => target_index.saturating_add(1),
    };
    next.windows
        .insert(insert_index.min(next.windows.len()), source);
    next.revision = next.revision.saturating_add(1);
    view_state.cached_window_list = Some(std::sync::Arc::new(next));
}

async fn switch_attach_status_tab(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    target_context_id: Uuid,
    kernel_client_factory: Option<&KernelClientFactory>,
) -> std::result::Result<(), ClientError> {
    debug!(target_context_id = %target_context_id, "attach.status_click.retarget");
    handle_attach_plugin_command_action(
        client,
        "bmux.windows",
        "switch-window",
        &[target_context_id.to_string()],
        view_state,
        kernel_client_factory,
        Vec::new(),
    )
    .await
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
    now: Instant,
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
                Vec::new(),
            )
            .await?;
            Ok(true)
        }
        AttachEventAction::Ui(action) => {
            handle_attach_ui_action_with_scrollback(client, &action, view_state, now).await?;
            view_state
                .dirty
                .mark_layout_frame_and_status_dirty(AttachDirtySource::Mouse);
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

pub async fn handle_attach_mouse_wheel_auto(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    mouse_event: MouseEvent,
    target_pane: Option<Uuid>,
    in_focused_pane: bool,
) -> std::result::Result<bool, ClientError> {
    if pane_mouse_protocol_reports_event(view_state, target_pane, mouse_event.kind)
        && maybe_forward_attach_mouse_event(
            client,
            view_state,
            mouse_event,
            target_pane,
            in_focused_pane,
            false,
        )
        .await?
    {
        return Ok(true);
    }

    if !in_focused_pane {
        return Ok(false);
    }

    let Some(target_pane) = target_pane else {
        return Ok(false);
    };

    if attach_pane_uses_alternate_screen(view_state, target_pane) {
        return match view_state.mouse.config.alternate_screen_wheel {
            bmux_config::AlternateScreenWheelBehavior::Ignore => Ok(false),
            bmux_config::AlternateScreenWheelBehavior::ForwardOnly => {
                maybe_forward_attach_mouse_event(
                    client,
                    view_state,
                    mouse_event,
                    Some(target_pane),
                    in_focused_pane,
                    false,
                )
                .await
            }
            bmux_config::AlternateScreenWheelBehavior::ScrollbackOnly => {
                handle_attach_mouse_scrollback_with_window(client, view_state, mouse_event.kind)
                    .await
            }
        };
    }

    handle_attach_mouse_scrollback_with_window(client, view_state, mouse_event.kind).await
}

pub fn pane_mouse_protocol_reports_event(
    view_state: &AttachViewState,
    target_pane: Option<Uuid>,
    kind: MouseEventKind,
) -> bool {
    let Some(target_pane) = target_pane else {
        return false;
    };
    let Some(protocol) = attach_pane_mouse_protocol(view_state, target_pane) else {
        return false;
    };
    attach_mouse::mode_reports_event(protocol.mode, mouse_event_kind_to_shared(kind))
}

pub fn attach_pane_uses_alternate_screen(view_state: &AttachViewState, pane_id: Uuid) -> bool {
    view_state
        .pane_buffers
        .get(&pane_id)
        .is_some_and(|buffer| buffer.protocol_tracker.alternate_screen())
}

pub fn maybe_begin_attach_mouse_selection_drag(
    view_state: &mut AttachViewState,
    target_pane: Option<Uuid>,
    mouse_event: MouseEvent,
) -> bool {
    if view_state.mouse.config.effective_wheel_propagation()
        == bmux_config::MouseWheelPropagation::ForwardOnly
        || view_state.mouse.config.effective_click_propagation()
            == bmux_config::MouseClickPropagation::ForwardOnly
    {
        return false;
    }

    let Some(target_pane) = target_pane else {
        return false;
    };
    if focused_attach_pane_id(view_state) != Some(target_pane) {
        return false;
    }
    if pane_mouse_protocol_reports_event(view_state, Some(target_pane), mouse_event.kind)
        || attach_pane_uses_alternate_screen(view_state, target_pane)
    {
        return false;
    }

    let Some(anchor) = attach_mouse_scrollback_position_for_event(
        view_state,
        target_pane,
        mouse_event.column,
        mouse_event.row,
        false,
    ) else {
        return false;
    };

    view_state.mouse.selection_drag = Some(AttachMouseSelectionDrag {
        pane_id: target_pane,
        anchor,
        active: false,
    });
    true
}

pub fn handle_attach_mouse_selection_drag_at(
    view_state: &mut AttachViewState,
    mouse_event: MouseEvent,
    now: Instant,
) -> bool {
    match mouse_event.kind {
        MouseEventKind::Drag(MouseButton::Left) => update_attach_mouse_selection_drag_at(
            view_state,
            mouse_event.column,
            mouse_event.row,
            now,
        ),
        MouseEventKind::Up(MouseButton::Left) => {
            finish_attach_mouse_selection_drag_at(view_state, now)
        }
        _ => false,
    }
}

fn update_attach_mouse_selection_drag_at(
    view_state: &mut AttachViewState,
    column: u16,
    row: u16,
    now: Instant,
) -> bool {
    let Some(drag) = view_state.mouse.selection_drag else {
        return false;
    };

    if !view_state.scrollback_active && !enter_attach_scrollback(view_state) {
        view_state.mouse.selection_drag = None;
        return true;
    }

    let Some(head) =
        attach_mouse_scrollback_position_for_event(view_state, drag.pane_id, column, row, true)
    else {
        return true;
    };

    view_state.selection_anchor = Some(drag.anchor);
    let _ = set_attach_scrollback_cursor_to_position(view_state, head);
    if !drag.active {
        view_state.set_transient_status(
            ATTACH_SELECTION_STARTED_STATUS,
            now,
            ATTACH_TRANSIENT_STATUS_TTL,
        );
    }
    view_state.mouse.selection_drag = Some(AttachMouseSelectionDrag {
        active: true,
        ..drag
    });
    view_state
        .dirty
        .mark_full_frame(AttachDirtySource::Selection);
    view_state
        .dirty
        .mark_status_dirty(AttachDirtySource::Selection);
    true
}

fn finish_attach_mouse_selection_drag_at(view_state: &mut AttachViewState, now: Instant) -> bool {
    let Some(drag) = view_state.mouse.selection_drag.take() else {
        return false;
    };
    if drag.active {
        match view_state.mouse.config.selection_release {
            bmux_config::MouseSelectionReleaseBehavior::Select => {}
            bmux_config::MouseSelectionReleaseBehavior::Copy => {
                copy_attach_selection_at(view_state, false, now);
            }
            bmux_config::MouseSelectionReleaseBehavior::CopyAndExit => {
                copy_attach_selection_at(view_state, true, now);
            }
        }
        view_state
            .dirty
            .mark_full_frame(AttachDirtySource::Selection);
        view_state
            .dirty
            .mark_status_dirty(AttachDirtySource::Selection);
    }
    true
}

fn attach_mouse_scrollback_position_for_event(
    view_state: &AttachViewState,
    pane_id: Uuid,
    column: u16,
    row: u16,
    clamp: bool,
) -> Option<AttachScrollbackPosition> {
    let content = attach_scene_pane_content_rect(view_state, pane_id)?;
    if content.w == 0 || content.h == 0 {
        return None;
    }

    let max_column = content.x.saturating_add(content.w.saturating_sub(1));
    let max_row = content.y.saturating_add(content.h.saturating_sub(1));
    if !clamp && (column < content.x || column > max_column || row < content.y || row > max_row) {
        return None;
    }

    let local_col = column
        .saturating_sub(content.x)
        .min(content.w.saturating_sub(1));
    let local_row = row
        .saturating_sub(content.y)
        .min(content.h.saturating_sub(1));
    Some(AttachScrollbackPosition {
        row: view_state
            .scrollback_offset
            .saturating_add(usize::from(local_row)),
        col: usize::from(local_col),
    })
}

fn set_attach_scrollback_cursor_to_position(
    view_state: &mut AttachViewState,
    position: AttachScrollbackPosition,
) -> bool {
    let Some((inner_w, inner_h)) = focused_attach_pane_inner_size(view_state) else {
        return false;
    };
    if inner_w == 0 || inner_h == 0 {
        return false;
    }
    view_state.scrollback_cursor = Some(AttachScrollbackCursor {
        row: position
            .row
            .saturating_sub(view_state.scrollback_offset)
            .min(inner_h.saturating_sub(1)),
        col: position.col.min(inner_w.saturating_sub(1)),
    });
    true
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
            view_state
                .dirty
                .mark_full_frame(AttachDirtySource::Scrollback);
            view_state
                .dirty
                .mark_status_dirty(AttachDirtySource::Scrollback);
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
            view_state
                .dirty
                .mark_full_frame(AttachDirtySource::Scrollback);
            view_state
                .dirty
                .mark_status_dirty(AttachDirtySource::Scrollback);
            true
        }
        _ => false,
    }
}

async fn send_attach_bytes_to_focused_pane(
    client: &mut StreamingBmuxClient,
    view_state: &AttachViewState,
    bytes: Vec<u8>,
) -> std::result::Result<(), ClientError> {
    if let Some(pane_id) = focused_attach_pane_id(view_state) {
        client
            .pane_direct_input(view_state.attached_id, pane_id, bytes)
            .await?;
    }
    Ok(())
}

fn attach_scene_pane_is_floating(view_state: &AttachViewState, pane_id: Uuid) -> bool {
    view_state
        .cached_layout_state
        .as_ref()
        .is_some_and(|layout_state| {
            layout_state.scene.surfaces.iter().any(|surface| {
                surface.visible
                    && surface.pane_id == Some(pane_id)
                    && surface.kind == AttachSurfaceKind::FloatingPane
            })
        })
}

pub async fn focus_attach_pane(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    pane_id: Uuid,
) -> std::result::Result<(), ClientError> {
    if view_state.mouse.last_focused_pane_id == Some(pane_id) {
        return Ok(());
    }

    let selector = attached_session_selector(view_state);
    if attach_scene_pane_is_floating(view_state, pane_id) {
        let _ack: bmux_windows_plugin_api::windows_commands::PaneAck = invoke_windows_command(
            client,
            "focus-floating-pane",
            &windows_commands::client::FocusFloatingPaneRequest {
                session: Some(ipc_to_windows_selector(selector)),
                target: pane_id_windows_selector(pane_id),
            },
        )
        .await?;
    } else {
        let _ack: bmux_windows_plugin_api::windows_commands::PaneAck = invoke_windows_command(
            client,
            "focus-pane-by-selector",
            &windows_commands::client::FocusPaneBySelectorRequest {
                session: Some(ipc_to_windows_selector(selector)),
                target: pane_id_windows_selector(pane_id),
            },
        )
        .await?;
    }

    view_state.mouse.last_focused_pane_id = Some(pane_id);
    view_state
        .dirty
        .mark_layout_frame_and_status_dirty(AttachDirtySource::FocusChanged);

    Ok(())
}

async fn resize_attach_pane(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    pane_id: Uuid,
    direction: bmux_windows_plugin_api::windows_commands::PaneResizeDirection,
    cells: u16,
) -> std::result::Result<(), ClientError> {
    let selector = attached_session_selector(view_state);
    let _ack: bmux_windows_plugin_api::windows_commands::PaneAck = invoke_windows_command(
        client,
        "resize-pane",
        &windows_commands::client::ResizePaneRequest {
            session: Some(ipc_to_windows_selector(selector)),
            target: Some(pane_id_windows_selector(pane_id)),
            direction,
            cells: cells.max(1),
        },
    )
    .await?;

    view_state
        .dirty
        .mark_layout_frame_and_status_dirty(AttachDirtySource::LayoutChanged);
    Ok(())
}

async fn move_attach_floating_pane(
    client: &mut StreamingBmuxClient,
    view_state: &mut AttachViewState,
    pane_id: Uuid,
    x: u16,
    y: u16,
) -> std::result::Result<(), ClientError> {
    let selector = attached_session_selector(view_state);
    let _ack: bmux_windows_plugin_api::windows_commands::PaneAck = invoke_windows_command(
        client,
        "move-floating-pane",
        &windows_commands::client::MoveFloatingPaneRequest {
            session: Some(ipc_to_windows_selector(selector)),
            target: pane_id_windows_selector(pane_id),
            x,
            y,
        },
    )
    .await?;

    view_state
        .dirty
        .mark_layout_frame_and_status_dirty(AttachDirtySource::LayoutChanged);
    Ok(())
}

pub fn attach_scene_pane_at(view_state: &AttachViewState, column: u16, row: u16) -> Option<Uuid> {
    let layout_state = view_state.cached_layout_state.as_ref()?;
    attach_mouse::pane_at(&layout_state.scene, column, row)
}

fn attach_scene_floating_drag_surface_at(
    view_state: &AttachViewState,
    column: u16,
    row: u16,
) -> Option<AttachSurface> {
    let layout_state = view_state.cached_layout_state.as_ref()?;
    layout_state
        .scene
        .surfaces
        .iter()
        .filter(|surface| {
            surface.visible
                && surface.accepts_input
                && matches!(surface.kind, AttachSurfaceKind::FloatingPane)
                && attach_rect_contains(surface.rect, column, row)
                && !attach_rect_contains(surface.content_rect, column, row)
        })
        .enumerate()
        .max_by_key(|(index, surface)| (surface.layer, surface.z, *index))
        .map(|(_, surface)| surface.clone())
}

fn floating_drag_position(drag: AttachMouseFloatingDrag, column: u16, row: u16) -> (u16, u16) {
    let dx = i32::from(column) - i32::from(drag.start_column);
    let dy = i32::from(row) - i32::from(drag.start_row);
    let max_x = drag
        .scene_max_x
        .saturating_sub(drag.width.saturating_sub(1));
    let max_y = drag
        .scene_max_y
        .saturating_sub(drag.height.saturating_sub(1));
    (
        u16::try_from((i32::from(drag.start_x) + dx).clamp(0, i32::from(max_x)))
            .unwrap_or(u16::MAX),
        u16::try_from((i32::from(drag.start_y) + dy).clamp(0, i32::from(max_y)))
            .unwrap_or(u16::MAX),
    )
}

fn attach_scene_bounds(view_state: &AttachViewState) -> Option<(u16, u16)> {
    let layout_state = view_state.cached_layout_state.as_ref()?;
    layout_state
        .scene
        .surfaces
        .iter()
        .filter(|surface| surface.visible)
        .filter_map(|surface| Some((rect_max_x(surface.rect)?, rect_max_y(surface.rect)?)))
        .reduce(|(max_x, max_y), (x, y)| (max_x.max(x), max_y.max(y)))
}

fn attach_rect_contains(rect: AttachRect, column: u16, row: u16) -> bool {
    let Some(max_x) = rect_max_x(rect) else {
        return false;
    };
    let Some(max_y) = rect_max_y(rect) else {
        return false;
    };
    (rect.x..=max_x).contains(&column) && (rect.y..=max_y).contains(&row)
}

pub fn attach_scene_resize_separator_at(
    view_state: &AttachViewState,
    column: u16,
    row: u16,
    last_applied_at: Instant,
) -> Option<AttachMouseResizeDrag> {
    let layout_state = view_state.cached_layout_state.as_ref()?;
    let pane_surfaces = layout_state
        .scene
        .surfaces
        .iter()
        .filter(|surface| {
            surface.visible
                && surface.accepts_input
                && matches!(surface.kind, AttachSurfaceKind::Pane)
        })
        .filter_map(|surface| surface.pane_id.map(|pane_id| (pane_id, surface.rect)))
        .collect::<Vec<_>>();

    let mut horizontal: Option<AttachMouseResizeAxisDrag> = None;
    let mut vertical: Option<AttachMouseResizeAxisDrag> = None;

    for (first_pane, first_rect) in &pane_surfaces {
        for (second_pane, second_rect) in &pane_surfaces {
            if first_pane == second_pane {
                continue;
            }
            if horizontal.is_none()
                && let Some(drag) = vertical_resize_axis_drag(
                    *first_pane,
                    *first_rect,
                    *second_pane,
                    *second_rect,
                    column,
                    row,
                )
            {
                horizontal = Some(drag);
            }
            if vertical.is_none()
                && let Some(drag) = horizontal_resize_axis_drag(
                    *first_pane,
                    *first_rect,
                    *second_pane,
                    *second_rect,
                    column,
                    row,
                )
            {
                vertical = Some(drag);
            }
            if horizontal.is_some() && vertical.is_some() {
                break;
            }
        }
        if horizontal.is_some() && vertical.is_some() {
            break;
        }
    }

    (horizontal.is_some() || vertical.is_some()).then_some(AttachMouseResizeDrag {
        horizontal,
        vertical,
        last_column: column,
        last_row: row,
        latest_column: column,
        latest_row: row,
        last_applied_at,
    })
}

fn vertical_resize_axis_drag(
    left_pane: Uuid,
    left_rect: AttachRect,
    right_pane: Uuid,
    right_rect: AttachRect,
    column: u16,
    row: u16,
) -> Option<AttachMouseResizeAxisDrag> {
    let left_max_x = rect_max_x(left_rect)?;
    if left_max_x.saturating_add(1) != right_rect.x {
        return None;
    }
    if !ranges_overlap_at(
        left_rect.y,
        rect_max_y(left_rect)?,
        right_rect.y,
        rect_max_y(right_rect)?,
        row,
    ) {
        return None;
    }
    if column != left_max_x && column != right_rect.x {
        return None;
    }
    Some(AttachMouseResizeAxisDrag {
        positive_target_pane_id: left_pane,
        positive_direction: bmux_windows_plugin_api::windows_commands::PaneResizeDirection::Right,
        negative_target_pane_id: right_pane,
        negative_direction: bmux_windows_plugin_api::windows_commands::PaneResizeDirection::Left,
    })
}

fn horizontal_resize_axis_drag(
    top_pane: Uuid,
    top_rect: AttachRect,
    bottom_pane: Uuid,
    bottom_rect: AttachRect,
    column: u16,
    row: u16,
) -> Option<AttachMouseResizeAxisDrag> {
    let top_max_y = rect_max_y(top_rect)?;
    if top_max_y.saturating_add(1) != bottom_rect.y {
        return None;
    }
    if !ranges_overlap_at(
        top_rect.x,
        rect_max_x(top_rect)?,
        bottom_rect.x,
        rect_max_x(bottom_rect)?,
        column,
    ) {
        return None;
    }
    if row != top_max_y && row != bottom_rect.y {
        return None;
    }
    Some(AttachMouseResizeAxisDrag {
        positive_target_pane_id: top_pane,
        positive_direction: bmux_windows_plugin_api::windows_commands::PaneResizeDirection::Down,
        negative_target_pane_id: bottom_pane,
        negative_direction: bmux_windows_plugin_api::windows_commands::PaneResizeDirection::Up,
    })
}

fn ranges_overlap_at(a_start: u16, a_end: u16, b_start: u16, b_end: u16, value: u16) -> bool {
    let start = a_start.max(b_start);
    let end = a_end.min(b_end);
    value >= start && value <= end
}

fn rect_max_x(rect: AttachRect) -> Option<u16> {
    (rect.w > 0).then(|| rect.x.saturating_add(rect.w.saturating_sub(1)))
}

fn rect_max_y(rect: AttachRect) -> Option<u16> {
    (rect.h > 0).then(|| rect.y.saturating_add(rect.h.saturating_sub(1)))
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

fn attach_terminal_input_event_actions(
    event: &TerminalInputEvent,
    attach_input_processor: &mut InputProcessor,
    ui_mode: AttachUiMode,
) -> Result<Vec<AttachEventAction>> {
    match event {
        TerminalInputEvent::Key(key) => {
            attach_terminal_key_event_actions(key, attach_input_processor, ui_mode)
        }
        TerminalInputEvent::Mouse(mouse) => mouse.to_crossterm().map_or_else(
            || Ok(vec![AttachEventAction::Ignore]),
            |mouse| Ok(vec![AttachEventAction::Mouse(mouse)]),
        ),
        TerminalInputEvent::Resize { .. } => Ok(vec![AttachEventAction::Redraw]),
        TerminalInputEvent::Bytes(_) => Ok(vec![AttachEventAction::Ignore]),
    }
}

fn attach_terminal_key_event_actions(
    key: &TerminalKeyEvent,
    attach_input_processor: &mut InputProcessor,
    ui_mode: AttachUiMode,
) -> Result<Vec<AttachEventAction>> {
    let Some(key) = key.to_crossterm() else {
        return Ok(vec![AttachEventAction::Ignore]);
    };
    attach_key_event_actions(&key, attach_input_processor, ui_mode)
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
        RuntimeAction::ScrollUpLine
        | RuntimeAction::ScrollDownLine
        | RuntimeAction::ScrollUpPage
        | RuntimeAction::ScrollDownPage
        | RuntimeAction::MoveCursorLeft
        | RuntimeAction::MoveCursorRight
        | RuntimeAction::MoveCursorUp
        | RuntimeAction::MoveCursorDown
        | RuntimeAction::ForwardToPane(_) => true,
        // Plugin commands: delegate to the manifest flag.
        RuntimeAction::PluginCommand {
            plugin_id,
            command_name,
            ..
        } => command_accepts_repeat(plugin_id, command_name),
        // Mutating and mode-switching — never repeat.
        RuntimeAction::Quit
        | RuntimeAction::NoOp
        | RuntimeAction::Detach
        | RuntimeAction::ShowHelp
        | RuntimeAction::EnterScrollMode
        | RuntimeAction::ExitScrollMode
        | RuntimeAction::ScrollTop
        | RuntimeAction::ScrollBottom
        | RuntimeAction::BeginSelection
        | RuntimeAction::CopyScrollback
        | RuntimeAction::ConfirmScrollback
        | RuntimeAction::ExitMode
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
        } if is_windows_close_active_pane_command(&plugin_id, &command_name) => {
            AttachEventAction::Ui(RuntimeAction::PluginCommand {
                plugin_id,
                command_name,
                args,
            })
        }
        RuntimeAction::PluginCommand {
            plugin_id,
            command_name,
            args,
        } => AttachEventAction::PluginCommand {
            plugin_id,
            command_name,
            args,
        },
        RuntimeAction::NoOp
        | RuntimeAction::ExitMode
        | RuntimeAction::Quit
        | RuntimeAction::ShowHelp
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
    ) || matches!(
        error,
        ClientError::ServerError { code: bmux_ipc::ErrorCode::Internal, message }
            if message.contains("plugin 'bmux.pane_runtime' service invocation failed with status 4")
    )
}

pub fn is_attach_not_attached_runtime_error(error: &ClientError) -> bool {
    matches!(
        error,
        ClientError::ServerError { message, .. }
            if message.contains("not attached to session runtime")
                || (message.contains("attach-pane-output-batch typed dispatch failed")
                    && message.contains("expected service invoked"))
    )
}
#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;
    use crate::input::InputProcessor;
    use crate::runtime::attach::input::TerminalModifiers;
    use crate::runtime::attach::render::append_pane_output;
    use crate::runtime::attach::state::{
        AttachEventAction, AttachMouseSelectionDrag, AttachScrollbackCursor,
        AttachScrollbackPosition, AttachUiEffect, AttachUiMode, AttachViewState, PaneRenderBuffer,
    };
    use crate::status::AttachStatusTabHitbox;

    use bmux_attach_layout_protocol::{
        AttachFocusTarget, AttachInputModeState, AttachMouseProtocolState, AttachRect, AttachScene,
        AttachSurface, AttachSurfaceKind, PaneLayoutNode, PaneState, PaneSummary,
    };
    use bmux_attach_view_protocol::AttachViewComponent;
    use bmux_client::{AttachLayoutState, AttachOpenInfo};
    use bmux_config::{
        BmuxConfig, MouseClickPropagation, MouseSelectionReleaseBehavior, MouseWheelPropagation,
    };

    use crossterm::event::{
        Event as CrosstermEvent, KeyCode as CrosstermKeyCode, KeyEvent as CrosstermKeyEvent,
        KeyEventKind as CrosstermKeyEventKind, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    };
    use std::collections::{BTreeMap, BTreeSet};
    use uuid::Uuid;

    #[test]
    fn pane_runtime_status_four_is_treated_as_closed_attach_stream() {
        let error = ClientError::ServerError {
            code: bmux_ipc::ErrorCode::Internal,
            message:
                "service invocation failed: plugin 'bmux.pane_runtime' service invocation failed with status 4"
                    .to_string(),
        };

        assert!(is_attach_stream_closed_error(&error));
    }

    fn focus_action(direction: &str) -> RuntimeAction {
        RuntimeAction::PluginCommand {
            plugin_id: "bmux.windows".to_string(),
            command_name: "focus-pane-in-direction".to_string(),
            args: vec!["--direction".to_string(), direction.to_string()],
        }
    }

    fn tab_status_line(hitboxes: Vec<AttachStatusTabHitbox>) -> AttachStatusLine {
        AttachStatusLine {
            rendered: String::new(),
            spans: Vec::new(),
            tab_hitboxes: hitboxes,
            drag_marker_col: None,
        }
    }

    #[test]
    fn attach_output_batch_unexpected_response_after_detach_is_stream_closed() {
        let error = ClientError::ServerError {
            code: bmux_ipc::ErrorCode::Internal,
            message: "attach-pane-output-batch typed dispatch failed: unexpected response invoking attach-runtime-state/attach-pane-output-batch: expected service invoked".to_string(),
        };

        assert!(is_attach_not_attached_runtime_error(&error));
    }

    #[test]
    fn tab_drop_target_uses_tab_halves_and_gaps() {
        let first = Uuid::from_u128(1);
        let second = Uuid::from_u128(2);
        let third = Uuid::from_u128(3);
        let status = tab_status_line(vec![
            AttachStatusTabHitbox {
                start_col: 2,
                end_col: 5,
                context_id: first,
            },
            AttachStatusTabHitbox {
                start_col: 8,
                end_col: 11,
                context_id: second,
            },
            AttachStatusTabHitbox {
                start_col: 14,
                end_col: 17,
                context_id: third,
            },
        ]);

        assert_eq!(
            resolve_attach_tab_drop_target(&status, 1),
            Some(AttachTabDropTarget {
                context_id: first,
                placement: AttachTabDropPlacement::Before,
            })
        );
        assert_eq!(
            resolve_attach_tab_drop_target(&status, 3),
            Some(AttachTabDropTarget {
                context_id: first,
                placement: AttachTabDropPlacement::Before,
            })
        );
        assert_eq!(
            resolve_attach_tab_drop_target(&status, 4),
            Some(AttachTabDropTarget {
                context_id: first,
                placement: AttachTabDropPlacement::After,
            })
        );
        assert_eq!(
            resolve_attach_tab_drop_target(&status, 7),
            Some(AttachTabDropTarget {
                context_id: second,
                placement: AttachTabDropPlacement::Before,
            })
        );
        assert_eq!(
            resolve_attach_tab_drop_target(&status, 99),
            Some(AttachTabDropTarget {
                context_id: third,
                placement: AttachTabDropPlacement::After,
            })
        );
        assert_eq!(
            attach_tab_drop_marker_col(
                &status,
                AttachTabDropTarget {
                    context_id: second,
                    placement: AttachTabDropPlacement::Before,
                },
                80,
            ),
            Some(8)
        );
        assert_eq!(
            attach_tab_drop_marker_col(
                &status,
                AttachTabDropTarget {
                    context_id: second,
                    placement: AttachTabDropPlacement::After,
                },
                80,
            ),
            Some(12)
        );
    }

    #[test]
    fn tab_drag_motion_is_active_for_move_events_and_cross_tab_release() {
        let first = Uuid::from_u128(1);
        let second = Uuid::from_u128(2);
        let drag = AttachMouseTabDrag {
            source_context_id: first,
            started_col: 4,
            started_row: 23,
            active: false,
            drop_target: Some(AttachTabDropTarget {
                context_id: first,
                placement: AttachTabDropPlacement::Before,
            }),
        };
        let same_cell = MouseEvent {
            kind: MouseEventKind::Moved,
            column: 4,
            row: 23,
            modifiers: KeyModifiers::empty(),
        };
        assert!(!attach_tab_drag_motion_is_active(
            &drag,
            same_cell.into(),
            drag.drop_target
        ));

        let moved_cell = MouseEvent {
            kind: MouseEventKind::Moved,
            column: 6,
            row: 23,
            modifiers: KeyModifiers::empty(),
        };
        assert!(attach_tab_drag_motion_is_active(
            &drag,
            moved_cell.into(),
            drag.drop_target
        ));

        assert!(attach_tab_drag_motion_is_active(
            &drag,
            same_cell.into(),
            Some(AttachTabDropTarget {
                context_id: second,
                placement: AttachTabDropPlacement::After,
            })
        ));
    }

    fn tab_reducer_view_state() -> AttachViewState {
        let first = Uuid::from_u128(1);
        let second = Uuid::from_u128(2);
        let third = Uuid::from_u128(3);
        let mut view_state = AttachViewState::new(AttachOpenInfo {
            context_id: None,
            session_id: Uuid::from_u128(99),
            can_write: true,
        });
        view_state.mouse.tab_drag_enabled = true;
        view_state.cached_status_line = Some(tab_status_line(vec![
            AttachStatusTabHitbox {
                start_col: 2,
                end_col: 5,
                context_id: first,
            },
            AttachStatusTabHitbox {
                start_col: 8,
                end_col: 11,
                context_id: second,
            },
            AttachStatusTabHitbox {
                start_col: 14,
                end_col: 17,
                context_id: third,
            },
        ]));
        view_state
    }

    const fn tab_reducer_geometry() -> TerminalGeometry {
        TerminalGeometry { cols: 80, rows: 24 }
    }

    const fn tab_mouse_event(phase: TerminalMousePhase, col: u16, row: u16) -> TerminalMouseEvent {
        TerminalMouseEvent {
            phase,
            button: Some(TerminalMouseButton::Left),
            col,
            row,
            modifiers: TerminalModifiers {
                shift: false,
                control: false,
                alt: false,
                super_key: false,
                hyper: false,
                meta: false,
            },
        }
    }

    #[test]
    fn status_tab_reducer_emits_move_after_motion() {
        let mut view_state = tab_reducer_view_state();
        let first = Uuid::from_u128(1);
        let third = Uuid::from_u128(3);

        let down = reduce_attach_status_tab_mouse_event(
            &mut view_state,
            tab_mouse_event(TerminalMousePhase::Down, 3, 23),
            tab_reducer_geometry(),
        );
        assert!(down.consumed);
        assert!(down.effects.is_empty());

        let motion = reduce_attach_status_tab_mouse_event(
            &mut view_state,
            tab_mouse_event(TerminalMousePhase::Move, 17, 23),
            tab_reducer_geometry(),
        );
        assert!(motion.consumed);
        assert!(motion.effects.is_empty());
        assert!(view_state.mouse.tab_drag.is_some_and(|drag| drag.active));

        let up = reduce_attach_status_tab_mouse_event(
            &mut view_state,
            tab_mouse_event(TerminalMousePhase::Up, 17, 23),
            tab_reducer_geometry(),
        );
        assert_eq!(
            up.effects,
            vec![AttachUiEffect::MoveWindow {
                source_context_id: first,
                target_context_id: third,
                placement: AttachTabDropPlacement::After,
            }]
        );
        assert!(view_state.mouse.tab_drag.is_none());
    }

    #[test]
    fn status_tab_reducer_infers_drag_on_cross_tab_mouse_up() {
        let mut view_state = tab_reducer_view_state();
        let first = Uuid::from_u128(1);
        let third = Uuid::from_u128(3);

        let down = reduce_attach_status_tab_mouse_event(
            &mut view_state,
            tab_mouse_event(TerminalMousePhase::Down, 3, 23),
            tab_reducer_geometry(),
        );
        assert!(down.consumed);

        let up = reduce_attach_status_tab_mouse_event(
            &mut view_state,
            tab_mouse_event(TerminalMousePhase::Up, 17, 23),
            tab_reducer_geometry(),
        );
        assert_eq!(
            up.effects,
            vec![AttachUiEffect::MoveWindow {
                source_context_id: first,
                target_context_id: third,
                placement: AttachTabDropPlacement::After,
            }]
        );
    }

    #[test]
    fn status_tab_reducer_click_without_drag_switches() {
        let mut view_state = tab_reducer_view_state();
        let first = Uuid::from_u128(1);

        let down = reduce_attach_status_tab_mouse_event(
            &mut view_state,
            tab_mouse_event(TerminalMousePhase::Down, 3, 23),
            tab_reducer_geometry(),
        );
        assert!(down.consumed);

        let up = reduce_attach_status_tab_mouse_event(
            &mut view_state,
            tab_mouse_event(TerminalMousePhase::Up, 3, 23),
            tab_reducer_geometry(),
        );
        assert_eq!(
            up.effects,
            vec![AttachUiEffect::SwitchWindow {
                target_context_id: first,
            }]
        );
    }

    #[test]
    fn status_tab_reducer_left_half_moves_before_target() {
        let mut view_state = tab_reducer_view_state();
        let first = Uuid::from_u128(1);
        let third = Uuid::from_u128(3);

        let down = reduce_attach_status_tab_mouse_event(
            &mut view_state,
            tab_mouse_event(TerminalMousePhase::Down, 15, 23),
            tab_reducer_geometry(),
        );
        assert!(down.consumed);

        let up = reduce_attach_status_tab_mouse_event(
            &mut view_state,
            tab_mouse_event(TerminalMousePhase::Up, 2, 23),
            tab_reducer_geometry(),
        );
        assert_eq!(
            up.effects,
            vec![AttachUiEffect::MoveWindow {
                source_context_id: third,
                target_context_id: first,
                placement: AttachTabDropPlacement::Before,
            }]
        );
    }

    #[test]
    fn status_tab_reducer_right_half_moves_after_target() {
        let mut view_state = tab_reducer_view_state();
        let first = Uuid::from_u128(1);
        let second = Uuid::from_u128(2);

        let down = reduce_attach_status_tab_mouse_event(
            &mut view_state,
            tab_mouse_event(TerminalMousePhase::Down, 3, 23),
            tab_reducer_geometry(),
        );
        assert!(down.consumed);

        let up = reduce_attach_status_tab_mouse_event(
            &mut view_state,
            tab_mouse_event(TerminalMousePhase::Up, 11, 23),
            tab_reducer_geometry(),
        );
        assert_eq!(
            up.effects,
            vec![AttachUiEffect::MoveWindow {
                source_context_id: first,
                target_context_id: second,
                placement: AttachTabDropPlacement::After,
            }]
        );
    }

    #[test]
    fn status_tab_reducer_gap_drop_resolves_to_nearest_insertion_point() {
        let mut view_state = tab_reducer_view_state();
        let first = Uuid::from_u128(1);
        let third = Uuid::from_u128(3);

        let down = reduce_attach_status_tab_mouse_event(
            &mut view_state,
            tab_mouse_event(TerminalMousePhase::Down, 3, 23),
            tab_reducer_geometry(),
        );
        assert!(down.consumed);

        let up = reduce_attach_status_tab_mouse_event(
            &mut view_state,
            tab_mouse_event(TerminalMousePhase::Up, 79, 23),
            tab_reducer_geometry(),
        );
        assert_eq!(
            up.effects,
            vec![AttachUiEffect::MoveWindow {
                source_context_id: first,
                target_context_id: third,
                placement: AttachTabDropPlacement::After,
            }]
        );
    }

    #[test]
    fn status_tab_reducer_tracks_marker_target_and_clears_drag_after_release() {
        let mut view_state = tab_reducer_view_state();
        let first = Uuid::from_u128(1);
        let second = Uuid::from_u128(2);

        let down = reduce_attach_status_tab_mouse_event(
            &mut view_state,
            tab_mouse_event(TerminalMousePhase::Down, 3, 23),
            tab_reducer_geometry(),
        );
        assert!(down.consumed);

        let motion = reduce_attach_status_tab_mouse_event(
            &mut view_state,
            tab_mouse_event(TerminalMousePhase::Move, 8, 23),
            tab_reducer_geometry(),
        );
        assert!(motion.consumed);
        let drag = view_state.mouse.tab_drag.expect("active tab drag");
        let drop_target = drag.drop_target.expect("drop target");
        assert!(drag.active);
        assert_eq!(drop_target.context_id, second);
        assert_eq!(drop_target.placement, AttachTabDropPlacement::Before);
        let status_line = view_state
            .cached_status_line
            .as_ref()
            .expect("cached status line");
        assert_eq!(
            attach_tab_drop_marker_col(status_line, drop_target, tab_reducer_geometry().cols),
            Some(8)
        );

        let up = reduce_attach_status_tab_mouse_event(
            &mut view_state,
            tab_mouse_event(TerminalMousePhase::Up, 8, 23),
            tab_reducer_geometry(),
        );
        assert_eq!(
            up.effects,
            vec![AttachUiEffect::MoveWindow {
                source_context_id: first,
                target_context_id: second,
                placement: AttachTabDropPlacement::Before,
            }]
        );
        assert!(view_state.mouse.tab_drag.is_none());
    }

    #[test]
    fn status_tab_reducer_mru_disables_move() {
        let mut view_state = tab_reducer_view_state();
        let first = Uuid::from_u128(1);
        view_state.mouse.tab_drag_enabled = false;

        let down = reduce_attach_status_tab_mouse_event(
            &mut view_state,
            tab_mouse_event(TerminalMousePhase::Down, 3, 23),
            tab_reducer_geometry(),
        );
        assert_eq!(
            down.effects,
            vec![AttachUiEffect::SwitchWindow {
                target_context_id: first,
            }]
        );
        assert!(view_state.mouse.tab_drag.is_none());

        let up = reduce_attach_status_tab_mouse_event(
            &mut view_state,
            tab_mouse_event(TerminalMousePhase::Up, 17, 23),
            tab_reducer_geometry(),
        );
        assert!(!up.consumed);
        assert!(up.effects.is_empty());
    }

    #[test]
    fn optimistic_window_list_reorder_moves_entry_and_bumps_revision() {
        let first = Uuid::from_u128(1);
        let second = Uuid::from_u128(2);
        let third = Uuid::from_u128(3);
        let session_id = Uuid::from_u128(10);
        let mut view_state = AttachViewState::new(AttachOpenInfo {
            context_id: Some(first),
            session_id,
            can_write: true,
        });
        view_state.cached_window_list = Some(std::sync::Arc::new(
            bmux_windows_plugin_api::windows_list::WindowListSnapshot {
                windows: vec![
                    bmux_windows_plugin_api::windows_list::WindowListEntry {
                        id: first,
                        name: "one".to_string(),
                        active: true,
                    },
                    bmux_windows_plugin_api::windows_list::WindowListEntry {
                        id: second,
                        name: "two".to_string(),
                        active: false,
                    },
                    bmux_windows_plugin_api::windows_list::WindowListEntry {
                        id: third,
                        name: "three".to_string(),
                        active: false,
                    },
                ],
                revision: 7,
            },
        ));

        optimistically_reorder_attach_window_list(
            &mut view_state,
            first,
            third,
            AttachTabDropPlacement::After,
        );

        let snapshot = view_state.cached_window_list.as_ref().expect("window list");
        let order = snapshot
            .windows
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>();
        assert_eq!(order, vec![second, third, first]);
        assert_eq!(snapshot.revision, 8);
        assert!(snapshot.windows[2].active);
    }

    #[test]
    fn tab_drag_is_disabled_for_mru_status_config() {
        let mut config = BmuxConfig::default().status_bar;
        assert!(status_tab_drag_enabled(&config));

        config.tab_order = bmux_config::StatusTabOrder::Mru;
        assert!(!status_tab_drag_enabled(&config));

        config.tab_order = bmux_config::StatusTabOrder::Stable;
        config.tab_scope = bmux_config::StatusTabScope::Mru;
        assert!(!status_tab_drag_enabled(&config));
    }

    fn frame_stats_for_classifier(scene_render: AttachSceneRenderStats) -> AttachFrameRenderStats {
        AttachFrameRenderStats {
            frame_bytes: 0,
            terminal_write_ms: 0,
            damage_rects: 0,
            damage_area_cells: 0,
            full_surface_fallbacks: 0,
            full_frame_fallback: false,
            scene_render,
            status_rendered: false,
            overlay_rendered: false,
            synchronized_update: false,
            dirty_event_count: 1,
        }
    }

    #[test]
    fn render_inefficiency_classifier_flags_no_visible_row_change() {
        let flags = classify_attach_render_inefficiency(&frame_stats_for_classifier(
            AttachSceneRenderStats {
                pane_rows_examined: 3,
                pane_rows_cached_skipped: 3,
                ..AttachSceneRenderStats::default()
            },
        ));

        assert!(flags.contains(AttachRenderInefficiencyFlags::DIRTY_NO_VISIBLE_ROW_CHANGE));
    }

    #[test]
    fn render_inefficiency_classifier_flags_large_partial_frame() {
        let flags = classify_attach_render_inefficiency(&frame_stats_for_classifier(
            AttachSceneRenderStats {
                viewport_cells: 100,
                pane_cells_emitted: 60,
                pane_rows_emitted: 3,
                ..AttachSceneRenderStats::default()
            },
        ));

        assert!(flags.contains(AttachRenderInefficiencyFlags::LARGE_PARTIAL_FRAME));
    }

    #[test]
    fn render_inefficiency_classifier_flags_full_frame_and_slow_write() {
        let mut stats = frame_stats_for_classifier(AttachSceneRenderStats::default());
        stats.full_frame_fallback = true;
        stats.frame_bytes = 1024;
        stats.terminal_write_ms = ATTACH_OVERRENDER_SLOW_TERMINAL_WRITE_MS_PER_KIB;
        let flags = classify_attach_render_inefficiency(&stats);

        assert!(flags.contains(AttachRenderInefficiencyFlags::FULL_FRAME_FALLBACK));
        assert!(flags.contains(AttachRenderInefficiencyFlags::SLOW_TERMINAL_WRITE_PER_KIB));
    }

    #[test]
    fn render_inefficiency_classifier_flags_extension_cache_miss_pressure() {
        let flags = classify_attach_render_inefficiency(&frame_stats_for_classifier(
            AttachSceneRenderStats {
                extension_render_calls: 10,
                extension_cache_hits: 1,
                extension_imperative_calls: 1,
                ..AttachSceneRenderStats::default()
            },
        ));

        assert!(flags.contains(AttachRenderInefficiencyFlags::EXTENSION_IMPERATIVE_OR_CACHE_MISS));
    }

    #[test]
    fn caller_process_command_execution_routes_to_caller_process() {
        assert_eq!(
            attach_plugin_command_execution_route(
                &bmux_plugin_sdk::CommandExecutionKind::CallerProcess
            ),
            AttachPluginCommandExecutionRoute::CallerProcess
        );
    }

    #[test]
    fn provider_command_executions_route_to_provider_pipeline() {
        for execution in [
            bmux_plugin_sdk::CommandExecutionKind::ProviderExec,
            bmux_plugin_sdk::CommandExecutionKind::BackgroundTask,
            bmux_plugin_sdk::CommandExecutionKind::RuntimeHook,
        ] {
            assert_eq!(
                attach_plugin_command_execution_route(&execution),
                AttachPluginCommandExecutionRoute::ProviderPipeline
            );
        }
    }

    fn test_pane_surface(pane_id: Uuid, rect: AttachRect) -> AttachSurface {
        AttachSurface {
            id: Uuid::new_v4(),
            kind: AttachSurfaceKind::Pane,
            layer: SurfaceLayer::Pane,
            z: 0,
            rect,
            content_rect: rect,
            interactive_regions: Vec::new(),
            opaque: true,
            visible: true,
            accepts_input: true,
            cursor_owner: false,
            pane_id: Some(pane_id),
        }
    }

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
                    layer: SurfaceLayer::Pane,
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
                terminal_grid: bmux_terminal_grid::TerminalGridStream::new(
                    20,
                    4,
                    bmux_terminal_grid::GridLimits::default(),
                )
                .expect("test grid dimensions are valid"),
                ..PaneRenderBuffer::default()
            });
        append_pane_output(buffer, b"one\r\n  four\r\n     five\r\n  six\r\n\x1b[4;3H");
        view_state
    }

    #[test]
    fn command_outcome_selected_context_metadata_parses_uuid() {
        let context_id = Uuid::from_u128(42);
        let mut outcome = PluginCommandOutcome::default();
        outcome.metadata.insert(
            COMMAND_OUTCOME_SELECTED_CONTEXT_ID_KEY.to_string(),
            serde_json::json!(context_id.to_string()),
        );

        assert_eq!(
            selected_context_id_from_command_outcome(&outcome),
            Some(context_id)
        );
    }

    #[test]
    fn command_outcome_selected_context_metadata_ignores_invalid_value() {
        let mut outcome = PluginCommandOutcome::default();
        outcome.metadata.insert(
            COMMAND_OUTCOME_SELECTED_CONTEXT_ID_KEY.to_string(),
            serde_json::json!("not-a-uuid"),
        );

        assert_eq!(selected_context_id_from_command_outcome(&outcome), None);
    }

    #[test]
    fn command_outcome_status_message_metadata_is_truncated() {
        let mut outcome = PluginCommandOutcome::default();
        let long_message = "x".repeat(ATTACH_PLUGIN_STATUS_MESSAGE_MAX_CHARS + 1);
        outcome.metadata.insert(
            COMMAND_OUTCOME_STATUS_MESSAGE_KEY.to_string(),
            serde_json::json!(long_message),
        );

        let message = status_message_from_command_outcome(&outcome)
            .expect("status message should be present");
        assert!(message.ends_with('…'));
        assert_eq!(
            message.chars().count(),
            ATTACH_PLUGIN_STATUS_MESSAGE_MAX_CHARS + 1
        );
    }

    #[test]
    fn attach_plugin_command_pipeline_is_generic_command_step() {
        let pipeline = build_attach_plugin_command_pipeline(
            "example.plugin",
            "do-thing",
            &["arg".to_string()],
        )
        .expect("pipeline should build");

        assert!(pipeline.inputs.is_empty());
        assert_eq!(pipeline.steps.len(), 1);
        let step = &pipeline.steps[0];
        assert_eq!(
            step.capability,
            bmux_plugin_sdk::CORE_CLI_COMMAND_CAPABILITY
        );
        assert_eq!(step.kind, InvokeServiceKind::Command);
        assert_eq!(
            step.interface_id,
            bmux_plugin_sdk::CORE_CLI_COMMAND_INTERFACE_V1
        );
        assert_eq!(
            step.operation,
            bmux_plugin_sdk::CORE_CLI_COMMAND_RUN_PLUGIN_OPERATION_V1
        );
    }

    #[test]
    fn attach_plugin_command_pipeline_metadata_is_optional() {
        let response = bmux_plugin_sdk::PluginCliCommandResponse::new(0);
        let payload =
            bmux_plugin_sdk::encode_service_message(&response).expect("response should encode");
        let execution = decode_attach_plugin_command_pipeline_results(
            "example.plugin",
            "do-thing",
            &[bmux_ipc::ServicePipelineStepResult {
                payload,
                metadata: BTreeMap::new(),
            }],
        )
        .expect("pipeline result should decode");

        assert_eq!(execution.status, 0);
        assert!(execution.outcome.metadata.is_empty());
        assert_eq!(
            selected_context_id_from_command_outcome(&execution.outcome),
            None
        );
    }

    #[test]
    fn plugin_window_snapshot_uses_attached_context_as_active() {
        let session_id = Uuid::new_v4();
        let stale_active = Uuid::new_v4();
        let attached_context = Uuid::new_v4();
        let mut view_state = AttachViewState::new(AttachOpenInfo {
            context_id: Some(attached_context),
            session_id,
            can_write: true,
        });
        view_state.cached_window_list = Some(std::sync::Arc::new(
            bmux_windows_plugin_api::windows_list::WindowListSnapshot {
                windows: vec![
                    bmux_windows_plugin_api::windows_list::WindowListEntry {
                        id: stale_active,
                        name: "old".to_string(),
                        active: true,
                    },
                    bmux_windows_plugin_api::windows_list::WindowListEntry {
                        id: attached_context,
                        name: "new".to_string(),
                        active: false,
                    },
                ],
                revision: 1,
            },
        ));
        let contexts = vec![
            ContextRow {
                id: stale_active,
                name: Some("old".to_string()),
            },
            ContextRow {
                id: attached_context,
                name: Some("new".to_string()),
            },
        ];
        let bindings = vec![
            ContextSessionBinding {
                context_id: stale_active,
                session_id,
            },
            ContextSessionBinding {
                context_id: attached_context,
                session_id,
            },
        ];

        let tabs = build_attach_tabs_from_catalog(
            &contexts,
            &bindings,
            &view_state,
            &BmuxConfig::default().status_bar,
            Some(attached_context),
            session_id,
        );

        assert_eq!(tabs.iter().filter(|tab| tab.active).count(), 1);
        assert!(
            tabs.iter()
                .any(|tab| tab.context_id == Some(attached_context) && tab.active)
        );
        assert!(
            tabs.iter()
                .any(|tab| tab.context_id == Some(stale_active) && !tab.active)
        );
    }

    #[test]
    fn protocol_only_output_marks_full_redraw_on_alt_screen_toggle() {
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

        let outcome = super::apply_attach_output_chunk_protocol_only(
            &mut view_state,
            pane_id,
            &payload,
            super::AttachOutputChunkMeta {
                stream_start: 0,
                stream_end: u64::try_from(payload.len()).expect("payload length fits u64"),
                stream_gap: false,
                sync_update_active: false,
            },
        );

        assert!(matches!(
            outcome,
            super::AttachChunkApplyOutcome::Applied { had_data: true }
        ));
        assert!(!view_state.dirty.pane_dirty_ids.contains(&pane_id));
        assert!(view_state.dirty.full_pane_redraw);
        assert!(view_state.force_cursor_move_next_frame);

        let buffer = view_state
            .pane_buffers
            .get(&pane_id)
            .expect("pane render buffer");
        assert!(!buffer.protocol_tracker.alternate_screen());
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
    fn structured_incremental_path_does_not_feed_raw_bytes_to_grid() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        let pane_id = view_state
            .cached_layout_state
            .as_ref()
            .map(|layout| layout.focused_pane_id)
            .expect("focused pane id");
        let before_revision = view_state
            .pane_buffers
            .get(&pane_id)
            .expect("pane render buffer")
            .terminal_grid
            .grid()
            .revision();

        let outcome = super::apply_attach_output_chunk_protocol_only(
            &mut view_state,
            pane_id,
            b"abc",
            super::AttachOutputChunkMeta {
                stream_start: 0,
                stream_end: 3,
                stream_gap: false,
                sync_update_active: false,
            },
        );

        assert!(matches!(
            outcome,
            super::AttachChunkApplyOutcome::Applied { had_data: true }
        ));
        let protocol_only_buffer = view_state
            .pane_buffers
            .get(&pane_id)
            .expect("pane render buffer");
        assert_eq!(protocol_only_buffer.expected_stream_start, Some(3));
        assert_eq!(
            protocol_only_buffer.terminal_grid.grid().revision(),
            before_revision
        );

        assert!(!view_state.dirty.pane_dirty_ids.contains(&pane_id));
    }

    #[test]
    fn protocol_only_output_preserves_snapshot_mouse_hint_until_explicit_disable() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        let pane_id = view_state
            .cached_layout_state
            .as_ref()
            .map(|layout| layout.focused_pane_id)
            .expect("focused pane id");
        view_state.pane_mouse_protocol_hints.insert(
            pane_id,
            AttachMouseProtocolState {
                mode: AttachMouseProtocolMode::PressRelease,
                encoding: AttachMouseProtocolEncoding::Sgr,
            },
        );

        let outcome = super::apply_attach_output_chunk_protocol_only(
            &mut view_state,
            pane_id,
            b"\x1b[2Jplain",
            super::AttachOutputChunkMeta {
                stream_start: 0,
                stream_end: 9,
                stream_gap: false,
                sync_update_active: false,
            },
        );

        assert!(matches!(
            outcome,
            super::AttachChunkApplyOutcome::Applied { had_data: true }
        ));
        assert_eq!(
            view_state
                .pane_mouse_protocol_hints
                .get(&pane_id)
                .map(|hint| hint.mode),
            Some(AttachMouseProtocolMode::PressRelease)
        );

        let outcome = super::apply_attach_output_chunk_protocol_only(
            &mut view_state,
            pane_id,
            b"\x1b[?1000l",
            super::AttachOutputChunkMeta {
                stream_start: 9,
                stream_end: 17,
                stream_gap: false,
                sync_update_active: false,
            },
        );

        assert!(matches!(
            outcome,
            super::AttachChunkApplyOutcome::Applied { had_data: true }
        ));
        assert_eq!(
            view_state
                .pane_mouse_protocol_hints
                .get(&pane_id)
                .map(|hint| hint.mode),
            Some(AttachMouseProtocolMode::None)
        );
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
        assert!(super::action_supports_repeat(&RuntimeAction::ScrollUpLine));
        assert!(super::action_supports_repeat(
            &RuntimeAction::ForwardToPane(b"x".to_vec())
        ));
    }

    #[test]
    fn action_supports_repeat_denies_mutating_actions() {
        assert!(!super::action_supports_repeat(
            &windows_close_active_pane_action()
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
        assert!(repeat.iter().all(|action| match action {
            AttachEventAction::PluginCommand {
                plugin_id,
                command_name,
                ..
            } => command_accepts_repeat(plugin_id, command_name),
            AttachEventAction::Ui(action) => super::action_supports_repeat(action),
            AttachEventAction::Send(_) => true,
            AttachEventAction::Detach
            | AttachEventAction::Redraw
            | AttachEventAction::Ignore
            | AttachEventAction::Mouse(_) => false,
        }));
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

        let now = Instant::now();
        record_attach_mouse_event(event, &mut view_state, now);

        assert_eq!(view_state.mouse.last_position, Some((3, 4)));
        assert_eq!(view_state.mouse.last_event_at, Some(now));
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
    fn wheel_policy_defaults_to_auto() {
        let view_state = attach_view_state_with_scrollback_fixture();

        assert_eq!(
            view_state.mouse.config.effective_wheel_propagation(),
            MouseWheelPropagation::Auto
        );
    }

    #[test]
    fn mouse_selection_drag_selects_visible_scrollback_text() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        let pane_id = focused_attach_pane_id(&view_state).expect("focused pane");
        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 2,
            modifiers: KeyModifiers::empty(),
        };

        assert!(maybe_begin_attach_mouse_selection_drag(
            &mut view_state,
            Some(pane_id),
            down,
        ));
        assert!(!view_state.scrollback_active);

        let drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 5,
            row: 3,
            modifiers: KeyModifiers::empty(),
        };
        assert!(handle_attach_mouse_selection_drag_at(
            &mut view_state,
            drag,
            Instant::now()
        ));
        assert!(view_state.scrollback_active);
        assert_eq!(
            view_state.selection_anchor,
            Some(AttachScrollbackPosition { row: 1, col: 1 })
        );
        assert_eq!(
            view_state.scrollback_cursor,
            Some(AttachScrollbackCursor { row: 2, col: 4 })
        );

        let up = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 5,
            row: 3,
            modifiers: KeyModifiers::empty(),
        };
        assert!(handle_attach_mouse_selection_drag_at(
            &mut view_state,
            up,
            Instant::now()
        ));
        assert!(view_state.scrollback_active);
        assert_eq!(view_state.mouse.selection_drag, None);
        assert!(view_state.selection_active());
    }

    #[test]
    fn mouse_selection_drag_ignores_single_click_without_drag() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        let pane_id = focused_attach_pane_id(&view_state).expect("focused pane");
        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 2,
            modifiers: KeyModifiers::empty(),
        };
        assert!(maybe_begin_attach_mouse_selection_drag(
            &mut view_state,
            Some(pane_id),
            down,
        ));

        let up = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 2,
            row: 2,
            modifiers: KeyModifiers::empty(),
        };
        assert!(handle_attach_mouse_selection_drag_at(
            &mut view_state,
            up,
            Instant::now()
        ));
        assert!(!view_state.scrollback_active);
        assert_eq!(view_state.selection_anchor, None);
    }

    #[test]
    fn mouse_selection_drag_respects_copy_release_config_without_clearing_on_failure() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        view_state.mouse.config.selection_release = MouseSelectionReleaseBehavior::Copy;
        let pane_id = focused_attach_pane_id(&view_state).expect("focused pane");
        view_state.mouse.selection_drag = Some(AttachMouseSelectionDrag {
            pane_id,
            anchor: AttachScrollbackPosition { row: 1, col: 1 },
            active: true,
        });
        assert!(enter_attach_scrollback(&mut view_state));
        view_state.selection_anchor = Some(AttachScrollbackPosition { row: 1, col: 1 });
        view_state.scrollback_cursor = Some(AttachScrollbackCursor { row: 2, col: 4 });

        let up = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 5,
            row: 3,
            modifiers: KeyModifiers::empty(),
        };
        assert!(handle_attach_mouse_selection_drag_at(
            &mut view_state,
            up,
            Instant::now()
        ));
        assert!(view_state.selection_active());
    }

    #[test]
    fn mouse_selection_drag_does_not_start_in_alternate_screen() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        let pane_id = focused_attach_pane_id(&view_state).expect("focused pane");
        let buffer = view_state
            .pane_buffers
            .get_mut(&pane_id)
            .expect("pane render buffer");
        append_pane_output(buffer, b"\x1b[?1049h");
        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 2,
            modifiers: KeyModifiers::empty(),
        };

        assert!(!maybe_begin_attach_mouse_selection_drag(
            &mut view_state,
            Some(pane_id),
            down,
        ));
    }

    #[test]
    fn mouse_selection_drag_does_not_start_when_clicks_always_forward() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        view_state.mouse.config.click_propagation = MouseClickPropagation::ForwardOnly;
        let pane_id = focused_attach_pane_id(&view_state).expect("focused pane");
        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 2,
            modifiers: KeyModifiers::empty(),
        };

        assert!(!maybe_begin_attach_mouse_selection_drag(
            &mut view_state,
            Some(pane_id),
            down,
        ));
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
        assert_eq!(protocol.mode, AttachMouseProtocolMode::PressRelease);
        assert_eq!(protocol.encoding, AttachMouseProtocolEncoding::Sgr);
    }

    #[test]
    fn attach_pane_mouse_protocol_uses_snapshot_hint_when_parser_mode_is_none() {
        let mut view_state = attach_view_state_with_scrollback_fixture();
        let pane_id = focused_attach_pane_id(&view_state).expect("focused pane id");

        view_state.pane_mouse_protocol_hints.insert(
            pane_id,
            AttachMouseProtocolState {
                mode: AttachMouseProtocolMode::AnyMotion,
                encoding: AttachMouseProtocolEncoding::Sgr,
            },
        );

        let protocol = attach_pane_mouse_protocol(&view_state, pane_id).expect("pane protocol");
        assert_eq!(protocol.mode, AttachMouseProtocolMode::AnyMotion);
        assert_eq!(protocol.encoding, AttachMouseProtocolEncoding::Sgr);
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
            AttachInputModeState {
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
            direction: bmux_attach_layout_protocol::PaneSplitDirection::Vertical,
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
                mode: AttachMouseProtocolMode::None,
                encoding: AttachMouseProtocolEncoding::Sgr,
            },
        );
        assert!(encoded.is_none());
    }

    #[test]
    fn mouse_protocol_mode_reports_event_rejects_move_without_any_motion_mode() {
        assert!(!mouse_protocol_mode_reports_event(
            AttachMouseProtocolMode::PressRelease,
            MouseEventKind::Moved,
        ));
        assert!(mouse_protocol_mode_reports_event(
            AttachMouseProtocolMode::AnyMotion,
            MouseEventKind::Moved,
        ));
    }

    #[test]
    fn mouse_protocol_mode_reports_event_rejects_release_in_press_mode() {
        assert!(!mouse_protocol_mode_reports_event(
            AttachMouseProtocolMode::Press,
            MouseEventKind::Up(MouseButton::Left),
        ));
        assert!(mouse_protocol_mode_reports_event(
            AttachMouseProtocolMode::Press,
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
                mode: AttachMouseProtocolMode::PressRelease,
                encoding: AttachMouseProtocolEncoding::Default,
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
                mode: AttachMouseProtocolMode::PressRelease,
                encoding: AttachMouseProtocolEncoding::Default,
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
                mode: AttachMouseProtocolMode::PressRelease,
                encoding: AttachMouseProtocolEncoding::Utf8,
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
                    layer: SurfaceLayer::Pane,
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
                terminal_grid: bmux_terminal_grid::TerminalGridStream::new(
                    90,
                    40,
                    bmux_terminal_grid::GridLimits::default(),
                )
                .expect("test grid dimensions are valid"),
                ..PaneRenderBuffer::default()
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
                    layer: SurfaceLayer::Pane,
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
                terminal_grid: bmux_terminal_grid::TerminalGridStream::new(
                    88,
                    38,
                    bmux_terminal_grid::GridLimits::default(),
                )
                .expect("test grid dimensions are valid"),
                ..PaneRenderBuffer::default()
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
        use bmux_plugin::{AttachRenderExtension, ExtensionRect, RenderDamage};
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

            fn render_surface(
                &self,
                _stdout: &mut dyn io::Write,
                _surface_id: Uuid,
                _surface_rect: &ExtensionRect,
                _damage: &RenderDamage,
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
                    layer: SurfaceLayer::Pane,
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
                terminal_grid: bmux_terminal_grid::TerminalGridStream::new(
                    36,
                    6,
                    bmux_terminal_grid::GridLimits::default(),
                )
                .expect("test grid dimensions are valid"),
                ..PaneRenderBuffer::default()
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
                        layer: SurfaceLayer::Pane,
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
                        layer: SurfaceLayer::FloatingPane,
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
    fn floating_drag_hit_testing_uses_border_not_content() {
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
                surfaces: vec![AttachSurface {
                    id: Uuid::new_v4(),
                    kind: AttachSurfaceKind::FloatingPane,
                    layer: SurfaceLayer::FloatingPane,
                    z: 1,
                    rect: AttachRect {
                        x: 2,
                        y: 2,
                        w: 8,
                        h: 5,
                    },
                    content_rect: AttachRect {
                        x: 3,
                        y: 3,
                        w: 6,
                        h: 3,
                    },
                    interactive_regions: Vec::new(),
                    opaque: true,
                    visible: true,
                    accepts_input: true,
                    cursor_owner: false,
                    pane_id: Some(floating_pane),
                }],
            },
            zoomed: false,
        });

        assert_eq!(
            attach_scene_floating_drag_surface_at(&view_state, 2, 2)
                .and_then(|surface| surface.pane_id),
            Some(floating_pane)
        );
        assert_eq!(
            attach_scene_floating_drag_surface_at(&view_state, 4, 4),
            None
        );
    }

    #[test]
    fn floating_drag_hit_testing_prefers_topmost_surface() {
        let session_id = Uuid::new_v4();
        let background_pane = Uuid::new_v4();
        let low_floating = Uuid::new_v4();
        let high_floating = Uuid::new_v4();
        let floating_surface = |pane_id, z| AttachSurface {
            id: Uuid::new_v4(),
            kind: AttachSurfaceKind::FloatingPane,
            layer: SurfaceLayer::FloatingPane,
            z,
            rect: AttachRect {
                x: 2,
                y: 2,
                w: 8,
                h: 5,
            },
            content_rect: AttachRect {
                x: 3,
                y: 3,
                w: 6,
                h: 3,
            },
            interactive_regions: Vec::new(),
            opaque: true,
            visible: true,
            accepts_input: true,
            cursor_owner: false,
            pane_id: Some(pane_id),
        };
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
                    floating_surface(low_floating, 1),
                    floating_surface(high_floating, 2),
                ],
            },
            zoomed: false,
        });

        assert_eq!(
            attach_scene_floating_drag_surface_at(&view_state, 2, 2)
                .and_then(|surface| surface.pane_id),
            Some(high_floating)
        );
    }

    #[test]
    fn floating_drag_position_clamps_top_left_and_applies_delta() {
        let drag = AttachMouseFloatingDrag {
            pane_id: Uuid::new_v4(),
            start_x: 5,
            start_y: 4,
            width: 8,
            height: 5,
            scene_max_x: 19,
            scene_max_y: 9,
            last_x: 5,
            last_y: 4,
            start_column: 10,
            start_row: 10,
        };

        assert_eq!(floating_drag_position(drag, 12, 11), (7, 5));
        assert_eq!(floating_drag_position(drag, 30, 30), (12, 5));
        assert_eq!(floating_drag_position(drag, 0, 0), (0, 0));
    }

    #[test]
    fn attach_scene_resize_separator_detects_vertical_boundary() {
        let session_id = Uuid::new_v4();
        let left_pane = Uuid::new_v4();
        let right_pane = Uuid::new_v4();
        let mut view_state = AttachViewState::new(AttachOpenInfo {
            context_id: None,
            session_id,
            can_write: true,
        });
        view_state.cached_layout_state = Some(AttachLayoutState {
            context_id: None,
            session_id,
            focused_pane_id: left_pane,
            panes: Vec::new(),
            layout_root: PaneLayoutNode::Leaf { pane_id: left_pane },
            scene: AttachScene {
                session_id,
                focus: AttachFocusTarget::Pane { pane_id: left_pane },
                surfaces: vec![
                    test_pane_surface(
                        left_pane,
                        AttachRect {
                            x: 0,
                            y: 0,
                            w: 10,
                            h: 8,
                        },
                    ),
                    test_pane_surface(
                        right_pane,
                        AttachRect {
                            x: 10,
                            y: 0,
                            w: 10,
                            h: 8,
                        },
                    ),
                ],
            },
            zoomed: false,
        });

        let now = Instant::now();
        let drag = attach_scene_resize_separator_at(&view_state, 9, 3, now)
            .expect("separator should be detected");
        assert!(drag.vertical.is_none());
        let horizontal = drag.horizontal.expect("horizontal drag axis");
        assert_eq!(horizontal.positive_target_pane_id, left_pane);
        assert_eq!(
            horizontal.positive_direction,
            bmux_windows_plugin_api::windows_commands::PaneResizeDirection::Right
        );
        assert_eq!(horizontal.negative_target_pane_id, right_pane);
        assert_eq!(
            horizontal.negative_direction,
            bmux_windows_plugin_api::windows_commands::PaneResizeDirection::Left
        );
        assert!(attach_scene_resize_separator_at(&view_state, 4, 3, now).is_none());
    }

    #[test]
    fn attach_scene_resize_separator_detects_horizontal_boundary() {
        let session_id = Uuid::new_v4();
        let top_pane = Uuid::new_v4();
        let bottom_pane = Uuid::new_v4();
        let mut view_state = AttachViewState::new(AttachOpenInfo {
            context_id: None,
            session_id,
            can_write: true,
        });
        view_state.cached_layout_state = Some(AttachLayoutState {
            context_id: None,
            session_id,
            focused_pane_id: top_pane,
            panes: Vec::new(),
            layout_root: PaneLayoutNode::Leaf { pane_id: top_pane },
            scene: AttachScene {
                session_id,
                focus: AttachFocusTarget::Pane { pane_id: top_pane },
                surfaces: vec![
                    test_pane_surface(
                        top_pane,
                        AttachRect {
                            x: 0,
                            y: 0,
                            w: 20,
                            h: 5,
                        },
                    ),
                    test_pane_surface(
                        bottom_pane,
                        AttachRect {
                            x: 0,
                            y: 5,
                            w: 20,
                            h: 5,
                        },
                    ),
                ],
            },
            zoomed: false,
        });

        let now = Instant::now();
        let drag = attach_scene_resize_separator_at(&view_state, 12, 5, now)
            .expect("separator should be detected");
        assert!(drag.horizontal.is_none());
        let vertical = drag.vertical.expect("vertical drag axis");
        assert_eq!(vertical.positive_target_pane_id, top_pane);
        assert_eq!(
            vertical.positive_direction,
            bmux_windows_plugin_api::windows_commands::PaneResizeDirection::Down
        );
        assert_eq!(vertical.negative_target_pane_id, bottom_pane);
        assert_eq!(
            vertical.negative_direction,
            bmux_windows_plugin_api::windows_commands::PaneResizeDirection::Up
        );
        assert!(attach_scene_resize_separator_at(&view_state, 12, 2, now).is_none());
    }

    #[test]
    fn attach_scene_resize_separator_detects_corner_boundary() {
        let session_id = Uuid::new_v4();
        let top_left = Uuid::new_v4();
        let top_right = Uuid::new_v4();
        let bottom_left = Uuid::new_v4();
        let bottom_right = Uuid::new_v4();
        let mut view_state = AttachViewState::new(AttachOpenInfo {
            context_id: None,
            session_id,
            can_write: true,
        });
        view_state.cached_layout_state = Some(AttachLayoutState {
            context_id: None,
            session_id,
            focused_pane_id: top_left,
            panes: Vec::new(),
            layout_root: PaneLayoutNode::Leaf { pane_id: top_left },
            scene: AttachScene {
                session_id,
                focus: AttachFocusTarget::Pane { pane_id: top_left },
                surfaces: vec![
                    test_pane_surface(
                        top_left,
                        AttachRect {
                            x: 0,
                            y: 0,
                            w: 10,
                            h: 5,
                        },
                    ),
                    test_pane_surface(
                        top_right,
                        AttachRect {
                            x: 10,
                            y: 0,
                            w: 10,
                            h: 5,
                        },
                    ),
                    test_pane_surface(
                        bottom_left,
                        AttachRect {
                            x: 0,
                            y: 5,
                            w: 10,
                            h: 5,
                        },
                    ),
                    test_pane_surface(
                        bottom_right,
                        AttachRect {
                            x: 10,
                            y: 5,
                            w: 10,
                            h: 5,
                        },
                    ),
                ],
            },
            zoomed: false,
        });

        let drag = attach_scene_resize_separator_at(&view_state, 9, 4, Instant::now())
            .expect("corner separator should be detected");
        let horizontal = drag.horizontal.expect("horizontal drag axis");
        let vertical = drag.vertical.expect("vertical drag axis");
        assert_eq!(horizontal.positive_target_pane_id, top_left);
        assert_eq!(horizontal.negative_target_pane_id, top_right);
        assert_eq!(vertical.positive_target_pane_id, top_left);
        assert_eq!(vertical.negative_target_pane_id, bottom_left);
    }

    #[test]
    fn resize_drag_axis_delta_maps_motion_to_target_direction_and_cells() {
        let left_pane = Uuid::new_v4();
        let right_pane = Uuid::new_v4();
        let drag = AttachMouseResizeAxisDrag {
            positive_target_pane_id: left_pane,
            positive_direction:
                bmux_windows_plugin_api::windows_commands::PaneResizeDirection::Right,
            negative_target_pane_id: right_pane,
            negative_direction:
                bmux_windows_plugin_api::windows_commands::PaneResizeDirection::Left,
        };

        let (target, direction, cells) =
            resize_drag_axis_delta(Some(drag), 3).expect("rightward drag should resize");
        assert_eq!(target, left_pane);
        assert_eq!(
            direction,
            bmux_windows_plugin_api::windows_commands::PaneResizeDirection::Right
        );
        assert_eq!(cells, 3);

        let (target, direction, cells) =
            resize_drag_axis_delta(Some(drag), -2).expect("leftward drag should resize");
        assert_eq!(target, right_pane);
        assert_eq!(
            direction,
            bmux_windows_plugin_api::windows_commands::PaneResizeDirection::Left
        );
        assert_eq!(cells, 2);
        assert!(resize_drag_axis_delta(Some(drag), 0).is_none());
        assert!(resize_drag_axis_delta(None, 2).is_none());
    }

    #[test]
    fn resize_drag_pending_delta_tracks_latest_unapplied_position() {
        let left_pane = Uuid::new_v4();
        let right_pane = Uuid::new_v4();
        let mut drag = AttachMouseResizeDrag {
            horizontal: Some(AttachMouseResizeAxisDrag {
                positive_target_pane_id: left_pane,
                positive_direction:
                    bmux_windows_plugin_api::windows_commands::PaneResizeDirection::Right,
                negative_target_pane_id: right_pane,
                negative_direction:
                    bmux_windows_plugin_api::windows_commands::PaneResizeDirection::Left,
            }),
            vertical: None,
            last_column: 10,
            last_row: 4,
            latest_column: 10,
            latest_row: 4,
            last_applied_at: Instant::now(),
        };

        assert!(!attach_mouse_resize_drag_has_pending_delta(&drag));
        drag.latest_column = 14;
        assert!(attach_mouse_resize_drag_has_pending_delta(&drag));
        drag.last_column = drag.latest_column;
        assert!(!attach_mouse_resize_drag_has_pending_delta(&drag));
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
            Some(AttachEventAction::PluginCommand { plugin_id, command_name, args })
                if plugin_id == "bmux.windows"
                    && command_name == "split-pane"
                    && args == &["--direction".to_string(), "vertical".to_string()]
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
            Some(AttachEventAction::PluginCommand { plugin_id, command_name, args })
                if plugin_id == "bmux.windows"
                    && command_name == "focus-pane-in-direction"
                    && args == &["--direction".to_string(), "prev".to_string()]
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
            Some(AttachEventAction::PluginCommand { plugin_id, command_name, args })
                if plugin_id == "bmux.windows"
                    && command_name == "focus-pane-in-direction"
                    && args == &["--direction".to_string(), "left".to_string()]
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
            Some(AttachEventAction::PluginCommand { plugin_id, command_name, args })
                if plugin_id == "bmux.windows"
                    && command_name == "focus-pane-in-direction"
                    && args == &["--direction".to_string(), "left".to_string()]
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

        clear_attach_selection_at(&mut view_state, false, Instant::now());
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

        confirm_attach_scrollback_at(&mut view_state, Instant::now());
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
                    layer: SurfaceLayer::Pane,
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
        assert_eq!(
            runtime.get("o"),
            Some(&"plugin:bmux.windows:focus-pane-in-direction --direction next".to_string())
        );
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
                && entry.action_name
                    == "plugin:bmux.windows:focus-pane-in-direction --direction next"
                && entry.action == focus_action("next")
        }));
        assert!(entries.iter().any(|entry| {
            entry.scope == AttachKeybindingScope::Global
                && entry.chord == "alt+h"
                && entry.action_name
                    == "plugin:bmux.windows:focus-pane-in-direction --direction left"
                && entry.action == focus_action("left")
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
            TerminalGeometry { cols: 80, rows: 24 },
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
            TerminalGeometry { cols: 80, rows: 24 },
        );
        assert!(!handled);
        assert_eq!(view_state.help_overlay_scroll, 5);
    }

    #[test]
    fn help_overlay_close_marks_overlay_redraw_without_full_pane_redraw() {
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
            TerminalGeometry { cols: 80, rows: 24 },
        );

        assert!(handled);
        assert!(!view_state.help_overlay_open);
        assert_eq!(view_state.help_overlay_scroll, 0);
        assert!(view_state.dirty.status_needs_redraw);
        assert!(view_state.dirty.overlay_needs_redraw);
        assert!(!view_state.dirty.full_pane_redraw);
    }

    #[test]
    fn retained_status_surface_uses_render_ops_payload() {
        let status_line = AttachStatusLine {
            rendered: "NORMAL".to_owned(),
            spans: vec![bmux_plugin::RenderTextSpan::new(
                "NORMAL",
                RenderStyle::new().bold(),
            )],
            tab_hitboxes: Vec::new(),
            drag_marker_col: Some(3),
        };

        let surface = retained_status_surface(&status_line, StatusPosition::Bottom, (20, 5))
            .expect("status surface");

        assert_eq!(surface.id, STATUS_SURFACE_ID);
        assert_eq!(surface.rect, DamageRect::new(0, 4, 20, 1));
        assert_eq!(surface.opacity, RetainedOpacity::Opaque);
        let RetainedSurfacePayload::RenderOps(ops) = surface.payload else {
            panic!("status should lower to render ops");
        };
        assert_eq!(ops.len(), 2);
    }

    #[test]
    fn retained_help_overlay_surface_uses_render_ops_payload() {
        let surface = AttachSurface {
            id: HELP_OVERLAY_SURFACE_ID,
            kind: AttachSurfaceKind::Overlay,
            layer: SurfaceLayer::Overlay,
            z: i32::MAX,
            rect: AttachRect {
                x: 2,
                y: 3,
                w: 20,
                h: 6,
            },
            content_rect: AttachRect {
                x: 2,
                y: 3,
                w: 20,
                h: 6,
            },
            interactive_regions: Vec::new(),
            opaque: true,
            visible: true,
            accepts_input: true,
            cursor_owner: true,
            pane_id: None,
        };

        let retained = retained_help_overlay_surface(&surface, &["line".to_owned()], 0);

        assert_eq!(retained.id, HELP_OVERLAY_SURFACE_ID);
        assert_eq!(retained.rect, DamageRect::new(2, 3, 20, 6));
        assert_eq!(retained.layer, retained_layer_order(SurfaceLayer::Overlay));
        let RetainedSurfacePayload::RenderOps(ops) = retained.payload else {
            panic!("help overlay should lower to render ops");
        };
        assert!(ops.iter().any(|op| matches!(op, RenderOp::Border { .. })));
    }

    #[test]
    fn retained_damage_overlay_surface_uses_transparent_render_ops_payload() {
        let view_state = attach_view_state_with_scrollback_fixture();
        let layout_state = view_state.cached_layout_state.expect("layout state");
        let pane_id = layout_state.focused_pane_id;
        let mut damage = bmux_attach_pipeline::FrameDamage::default();
        damage.mark_content_surface_rect(
            pane_id,
            DamageRect::new(0, 0, 2, 1),
            (10, 5),
            DamageCoalescingPolicy::default(),
        );

        let surface = retained_damage_overlay_surface(&layout_state.scene, &damage, (80, 24), 0, 1)
            .expect("damage overlay surface");

        assert_eq!(surface.id, DAMAGE_OVERLAY_SURFACE_ID);
        assert_eq!(surface.opacity, RetainedOpacity::Transparent);
        let RetainedSurfacePayload::RenderOps(ops) = surface.payload else {
            panic!("damage overlay should lower to render ops");
        };
        assert!(ops.iter().any(|op| matches!(op, RenderOp::Border { .. })));
    }

    #[test]
    fn frame_uses_synchronized_update_only_for_scene_or_overlay_damage() {
        let mut status_only = bmux_attach_pipeline::FrameDamage::default();
        status_only.mark_status();
        assert!(!frame_uses_synchronized_update(&status_only));

        let mut overlay = bmux_attach_pipeline::FrameDamage::default();
        overlay.mark_overlay();
        assert!(frame_uses_synchronized_update(&overlay));

        let mut content = bmux_attach_pipeline::FrameDamage::default();
        content.mark_content_surface(Uuid::from_u128(1));
        assert!(frame_uses_synchronized_update(&content));
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
    fn resize_attach_grids_applies_layout_size_before_snapshot_bytes() {
        let pane_id = uuid::Uuid::new_v4();
        let scene = AttachScene {
            session_id: uuid::Uuid::new_v4(),
            focus: AttachFocusTarget::Pane { pane_id },
            surfaces: vec![AttachSurface {
                id: pane_id,
                kind: AttachSurfaceKind::Pane,
                layer: SurfaceLayer::Pane,
                z: 0,
                rect: AttachRect {
                    x: 0,
                    y: 1,
                    w: 120,
                    h: 49,
                },
                // Content rect reflects the server's 1-cell border inset.
                content_rect: AttachRect {
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

        resize_attach_grids_for_scene_with_size(&mut pane_buffers, &scene, 120, 50);

        let buffer = pane_buffers
            .get_mut(&pane_id)
            .expect("pane buffer should exist");
        append_pane_output(&mut *buffer, b"\x1b[999;999H");
        let cursor = buffer.terminal_grid.grid().cursor();

        assert_eq!(
            cursor.row, 46,
            "cursor row should clamp to pane inner height"
        );
        assert_eq!(
            cursor.col, 117,
            "cursor col should clamp to pane inner width"
        );
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
    fn sync_attach_active_mode_marks_full_frame_on_mode_change() {
        let keymap = attach_keymap_from_config(&BmuxConfig::default());
        let mut view_state = AttachViewState::new(AttachOpenInfo {
            context_id: None,
            session_id: Uuid::new_v4(),
            can_write: true,
        });
        view_state.dirty.clear_frame_damage();
        view_state.dirty.status_needs_redraw = false;

        sync_attach_active_mode_from_processor(&mut view_state, &keymap, Some("inspect"));

        assert_eq!(view_state.active_mode_id, "inspect");
        assert!(view_state.dirty.full_pane_redraw);
        assert!(view_state.dirty.status_needs_redraw);
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
