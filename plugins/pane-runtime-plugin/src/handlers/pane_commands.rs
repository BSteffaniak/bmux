//! Typed handlers for the `pane-runtime-commands` interface.
//!
//! Each handler decodes its `...Args` wire struct, dispatches through
//! the registered `SessionRuntimeManagerHandle`, and serializes the
//! BPDL result type. When the manager handle is not registered
//! (`session_runtime_handle()` returns `None`), the handler reports a
//! `PaneCommandError::Failed { reason }` / `SessionRuntimeCommandError::Failed`.

use bmux_attach_layout_protocol::{
    PaneFocusDirection, PaneLaunchCommand, PaneSelector, PaneSplitDirection, PaneState,
};
use bmux_pane_runtime_plugin_api::pane_runtime_commands::{
    FloatingPaneAck, PaneAck, PaneCommandError, SessionAck, SessionRuntimeCommandError,
};
use bmux_pane_runtime_plugin_api::pane_runtime_events::{self, AttachViewComponent, PaneEvent};
use bmux_pane_runtime_state::{
    FloatingPaneLayer, FloatingPaneScope, LayoutRect, PaneResizeDirection, SessionRuntimeError,
};
use bmux_session_models::{ClientId, SessionId};
use bmux_sessions_plugin_api::sessions_events::{self, SessionEvent};
use serde::{Deserialize, Serialize};
use tracing::warn;
use uuid::Uuid;

// ── Wire-format argument structs ─────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SplitPaneArgs {
    pub session_id: Uuid,
    #[serde(default)]
    pub target: Option<Uuid>,
    pub direction: String,
    pub ratio_percent: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchPaneArgs {
    pub session_id: Uuid,
    #[serde(default)]
    pub target: Option<Uuid>,
    pub direction: String,
    pub ratio_percent: u8,
    #[serde(default)]
    pub name: Option<String>,
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FocusPaneArgs {
    pub session_id: Uuid,
    #[serde(default)]
    pub target: Option<Uuid>,
    pub direction: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResizePaneArgs {
    pub session_id: Uuid,
    #[serde(default)]
    pub target: Option<Uuid>,
    pub direction: String,
    #[serde(default = "default_resize_cells")]
    pub cells: u16,
}

const fn default_resize_cells() -> u16 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClosePaneArgs {
    pub session_id: Uuid,
    #[serde(default)]
    pub target: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestartPaneArgs {
    pub session_id: Uuid,
    #[serde(default)]
    pub target: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZoomPaneArgs {
    pub session_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateFloatingPaneArgs {
    pub session_id: Uuid,
    #[serde(default)]
    pub target: Option<Uuid>,
    #[serde(default)]
    pub anchor_pane_id: Option<Uuid>,
    #[serde(default)]
    pub context_id: Option<Uuid>,
    #[serde(default)]
    pub client_id: Option<Uuid>,
    #[serde(default)]
    pub x: Option<u16>,
    #[serde(default)]
    pub y: Option<u16>,
    #[serde(default)]
    pub w: Option<u16>,
    #[serde(default)]
    pub h: Option<u16>,
    #[serde(default)]
    pub z: Option<i32>,
    #[serde(default)]
    pub layer: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub program: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FloatingPaneTargetArgs {
    pub session_id: Uuid,
    pub pane_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoveFloatingPaneArgs {
    pub session_id: Uuid,
    pub pane_id: Uuid,
    pub x: u16,
    pub y: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResizeFloatingPaneArgs {
    pub session_id: Uuid,
    pub pane_id: Uuid,
    pub w: u16,
    pub h: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetFloatingPaneZArgs {
    pub session_id: Uuid,
    pub pane_id: Uuid,
    pub z: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetFloatingPaneLayerArgs {
    pub session_id: Uuid,
    pub pane_id: Uuid,
    pub layer: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneDirectInputArgs {
    pub session_id: Uuid,
    pub pane_id: Uuid,
    #[serde(with = "bmux_plugin_sdk::codec::serde_bytes_vec")]
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetClientWritePermissionArgs {
    pub session_id: Uuid,
    pub client_id: Uuid,
    pub allowed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewSessionArgs {
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KillSessionArgs {
    pub session_id: Uuid,
    pub force_local: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreSessionArgs {
    pub session_id: Uuid,
    pub snapshot_payload: Vec<u8>,
}

// ── Handler bodies ───────────────────────────────────────────────

fn failed_command(reason: impl Into<String>) -> PaneCommandError {
    PaneCommandError::Failed {
        reason: reason.into(),
    }
}

fn failed_session(reason: impl Into<String>) -> SessionRuntimeCommandError {
    SessionRuntimeCommandError::Failed {
        reason: reason.into(),
    }
}

/// Wire shape matching server's `SessionPolicyCheckRequest`.
/// Perform the `bmux.sessions.policy / session-policy-state / check`
/// policy query for a mutating pane/session operation. Returns `Ok(())`
/// when the policy allows the operation (or no policy provider is
/// registered), or a `PaneCommandError::Denied` when the policy
/// responds with `allowed = false`.
fn ensure_session_mutation_allowed(
    ctx: &bmux_plugin_sdk::NativeServiceContext,
    session_id: SessionId,
    action: &str,
) -> Result<(), PaneCommandError> {
    let client_id = ctx
        .caller_client_id
        .ok_or_else(|| failed_command("policy check requires a caller client id"))?;
    let principal_id = bmux_plugin::global_plugin_state_registry()
        .get::<bmux_client_state::ClientPrincipalHandle>()
        .and_then(|arc| arc.read().ok().map(|g| (*g).clone()))
        .and_then(|handle| handle.0.get(bmux_session_models::ClientId(client_id)))
        .unwrap_or_else(Uuid::nil);

    // The policy service may not be registered in tests or headless
    // tooling. Treat a missing provider as "allowed" (matches server's
    // legacy behavior where `check_session_policy` returns `None` when
    // no resolver is installed).
    let mut client = bmux_plugin::ServiceCallerDispatchClient::new(ctx);
    match bmux_plugin::block_on_typed_dispatch(
        bmux_permissions_plugin_api::session_policy_state::client::check(
            &mut client,
            session_id.0,
            None,
            client_id,
            principal_id,
            action.to_string(),
            None,
            None,
            None,
        ),
    ) {
        Ok(response) => {
            if response.allowed {
                Ok(())
            } else {
                Err(PaneCommandError::Denied {
                    reason: response
                        .reason
                        .unwrap_or_else(|| "session policy denied for this operation".to_string()),
                })
            }
        }
        Err(err) if err.to_string().contains("unsupported") => Ok(()),
        Err(err) => Err(failed_command(format!(
            "session policy check failed: {err}"
        ))),
    }
}

fn parse_split_direction(raw: &str) -> Result<PaneSplitDirection, PaneCommandError> {
    match raw {
        "horizontal" => Ok(PaneSplitDirection::Horizontal),
        "vertical" => Ok(PaneSplitDirection::Vertical),
        other => Err(failed_command(format!(
            "invalid split direction: '{other}' (expected horizontal|vertical)"
        ))),
    }
}

fn parse_focus_direction(raw: &str) -> Result<Option<PaneFocusDirection>, PaneCommandError> {
    match raw {
        "next" => Ok(Some(PaneFocusDirection::Next)),
        "prev" | "previous" => Ok(Some(PaneFocusDirection::Prev)),
        "" => Ok(None),
        other => Err(failed_command(format!(
            "invalid focus direction: '{other}'"
        ))),
    }
}

fn parse_resize_direction(raw: &str) -> Result<PaneResizeDirection, PaneCommandError> {
    match raw {
        "increase" => Ok(PaneResizeDirection::Increase),
        "decrease" => Ok(PaneResizeDirection::Decrease),
        "left" => Ok(PaneResizeDirection::Left),
        "right" => Ok(PaneResizeDirection::Right),
        "up" => Ok(PaneResizeDirection::Up),
        "down" => Ok(PaneResizeDirection::Down),
        other => Err(failed_command(format!(
            "invalid resize direction: '{other}' (expected increase|decrease|left|right|up|down)"
        ))),
    }
}

fn parse_floating_scope(raw: Option<&str>) -> Result<FloatingPaneScope, PaneCommandError> {
    match raw.unwrap_or("per_window") {
        "per_pane" | "pane" => Ok(FloatingPaneScope::PerPane),
        "per_window" | "per_tab" | "window" | "tab" => Ok(FloatingPaneScope::PerWindow),
        "per_session" | "session" => Ok(FloatingPaneScope::PerSession),
        "client_global" | "client" => Ok(FloatingPaneScope::ClientGlobal),
        "server_global" | "global" | "server" => Ok(FloatingPaneScope::ServerGlobal),
        other => Err(failed_command(format!(
            "invalid floating pane scope: '{other}'"
        ))),
    }
}

fn parse_floating_layer(raw: Option<&str>) -> Result<FloatingPaneLayer, PaneCommandError> {
    match raw.unwrap_or("floating_pane") {
        "pane" => Ok(FloatingPaneLayer::Pane),
        "overlay" => Ok(FloatingPaneLayer::Overlay),
        "floating" | "floating_pane" => Ok(FloatingPaneLayer::FloatingPane),
        "tooltip" | "top" => Ok(FloatingPaneLayer::Tooltip),
        other => Err(failed_command(format!(
            "invalid floating pane layer: '{other}'"
        ))),
    }
}

fn floating_ack(
    session_id: SessionId,
    summary: bmux_pane_runtime_state::FloatingPaneRuntimeSummary,
) -> FloatingPaneAck {
    FloatingPaneAck {
        session_id: session_id.0,
        pane_id: summary.pane_id,
        surface_id: summary.id,
    }
}

fn target_selector(target: Option<Uuid>) -> Option<PaneSelector> {
    target.map(PaneSelector::ById)
}

fn publish_session_removed_event(session_id: SessionId) {
    let _ = bmux_plugin::global_event_bus().emit(
        &sessions_events::EVENT_KIND,
        SessionEvent::Removed {
            session_id: session_id.0,
        },
    );
}

fn publish_pane_event(event: PaneEvent) {
    let _ = bmux_plugin::global_event_bus().emit(&pane_runtime_events::EVENT_KIND, event);
}

/// Bump a session's attach-view revision and publish the scene
/// component update. Mirrors the server's
/// `emit_attach_view_changed_for_layout` helper so plugin-side
/// handlers keep parity with the old IPC-handler behavior.
///
/// Also republishes the pane-runtime-focus state channel so
/// subscribers (decoration plugin, future status bars) see the
/// current focused pane without polling.
fn emit_attach_view_changed_scene(session_id: SessionId) {
    let Some(handle) = super::session_runtime_handle() else {
        return;
    };
    let Some(revision) = handle.0.bump_attach_view_revision(session_id) else {
        return;
    };
    publish_pane_event(PaneEvent::AttachViewChanged {
        context_id: None,
        session_id: session_id.0,
        revision,
        components: vec![AttachViewComponent::Scene],
    });
    super::publish_focus_state_snapshot();
}

fn emit_attach_view_changed_scene_all() {
    let Some(handle) = super::session_runtime_handle() else {
        return;
    };
    for session_id in handle.0.active_session_ids() {
        emit_attach_view_changed_scene(session_id);
    }
}

pub fn split_pane(
    req: &SplitPaneArgs,
    ctx: &bmux_plugin_sdk::NativeServiceContext,
) -> Result<PaneAck, PaneCommandError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed_command("pane-runtime manager handle not registered"))?;
    let direction = parse_split_direction(&req.direction)?;
    let session_id = SessionId(req.session_id);
    ensure_session_mutation_allowed(ctx, session_id, "pane.split")?;
    let pane_id = handle
        .0
        .split_pane(session_id, target_selector(req.target), direction)
        .map_err(|e| failed_command(e.to_string()))?;
    emit_attach_view_changed_scene(session_id);
    Ok(PaneAck {
        session_id: req.session_id,
        pane_id,
    })
}

pub fn launch_pane(
    req: LaunchPaneArgs,
    ctx: &bmux_plugin_sdk::NativeServiceContext,
) -> Result<PaneAck, PaneCommandError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed_command("pane-runtime manager handle not registered"))?;
    let direction = parse_split_direction(&req.direction)?;
    let command = PaneLaunchCommand {
        program: req.program,
        args: req.args,
        cwd: req.cwd,
        env: std::collections::BTreeMap::new(),
    };
    let session_id = SessionId(req.session_id);
    ensure_session_mutation_allowed(ctx, session_id, "pane.launch")?;
    let pane_id = handle
        .0
        .launch_pane(
            session_id,
            target_selector(req.target),
            direction,
            req.name,
            command,
        )
        .map_err(|e| failed_command(e.to_string()))?;
    emit_attach_view_changed_scene(session_id);
    Ok(PaneAck {
        session_id: req.session_id,
        pane_id,
    })
}

pub fn focus_pane(
    req: &FocusPaneArgs,
    ctx: &bmux_plugin_sdk::NativeServiceContext,
) -> Result<PaneAck, PaneCommandError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed_command("pane-runtime manager handle not registered"))?;
    let session_id = SessionId(req.session_id);
    ensure_session_mutation_allowed(ctx, session_id, "pane.focus")?;
    let pane_id = match (req.target, parse_focus_direction(&req.direction)?) {
        (Some(t), None) => handle
            .0
            .focus_pane_target(session_id, &PaneSelector::ById(t)),
        (None, Some(dir)) => handle.0.focus_pane(session_id, dir),
        (None, None) => handle
            .0
            .focus_pane_target(session_id, &PaneSelector::Active),
        (Some(_), Some(_)) => {
            return Err(failed_command(
                "focus-pane cannot use target and direction together",
            ));
        }
    }
    .map_err(|e| failed_command(e.to_string()))?;
    emit_attach_view_changed_scene(session_id);
    Ok(PaneAck {
        session_id: req.session_id,
        pane_id,
    })
}

pub fn resize_pane(
    req: &ResizePaneArgs,
    ctx: &bmux_plugin_sdk::NativeServiceContext,
) -> Result<SessionAck, PaneCommandError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed_command("pane-runtime manager handle not registered"))?;
    let session_id = SessionId(req.session_id);
    ensure_session_mutation_allowed(ctx, session_id, "pane.resize")?;
    let direction = parse_resize_direction(&req.direction)?;
    handle
        .0
        .resize_pane(
            session_id,
            target_selector(req.target),
            direction,
            req.cells.max(1),
        )
        .map_err(|e| failed_command(e.to_string()))?;
    emit_attach_view_changed_scene(session_id);
    Ok(SessionAck {
        session_id: req.session_id,
    })
}

pub fn close_pane(
    req: &ClosePaneArgs,
    ctx: &bmux_plugin_sdk::NativeServiceContext,
) -> Result<PaneAck, PaneCommandError> {
    use bmux_plugin::global_plugin_state_registry;

    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed_command("pane-runtime manager handle not registered"))?;
    let session_id = SessionId(req.session_id);
    ensure_session_mutation_allowed(ctx, session_id, "pane.close")?;
    if handle
        .0
        .list_panes(session_id)
        .map_err(|e| failed_command(e.to_string()))?
        .len()
        <= 1
    {
        return Err(PaneCommandError::Denied {
            reason: "cannot close the final pane without choosing a new target or quitting"
                .to_string(),
        });
    }
    let (pane_id, removed_runtime) = handle
        .0
        .close_pane(session_id, target_selector(req.target))
        .map_err(|e| failed_command(e.to_string()))?;

    // When closing the last pane removes the whole session, tear down
    // every associated piece (session-manager entry, context
    // mappings, follow-state selections, attach tokens) and emit the
    // session-level wire events. Mirrors the server's legacy
    // `Request::ClosePane` orchestration.
    let session_closed = removed_runtime.is_some();
    if let Some(removed) = removed_runtime {
        let had_attached_clients = !removed.attached_clients.is_empty();
        handle.0.shutdown_removed_runtime(removed);

        if let Some(session_handle) = global_plugin_state_registry()
            .get::<bmux_session_state::SessionManagerHandle>()
            .and_then(|arc| arc.read().ok().map(|g| (*g).clone()))
        {
            let _ = session_handle.0.remove_session(session_id);
        }
        if let Some(context_handle) = global_plugin_state_registry()
            .get::<bmux_context_state::ContextStateHandle>()
            .and_then(|arc| arc.read().ok().map(|g| (*g).clone()))
        {
            let _ = context_handle.0.remove_contexts_for_session(session_id);
        }
        if let Some(follow_handle) = global_plugin_state_registry()
            .get::<bmux_client_state::FollowStateHandle>()
            .and_then(|arc| arc.read().ok().map(|g| (*g).clone()))
        {
            follow_handle.0.clear_selections_for_session(session_id);
        }
        if let Some(attach_tokens) = global_plugin_state_registry()
            .get::<bmux_attach_token_state::AttachTokenManagerHandle>()
            .and_then(|arc| arc.read().ok().map(|g| (*g).clone()))
        {
            attach_tokens.0.remove_for_session(session_id);
        }

        if had_attached_clients {
            publish_pane_event(PaneEvent::ClientDetached {
                session_id: session_id.0,
            });
        }
        publish_session_removed_event(session_id);
    }

    if !session_closed {
        emit_attach_view_changed_scene(session_id);
    }

    Ok(PaneAck {
        session_id: req.session_id,
        pane_id,
    })
}

pub fn restart_pane(
    req: &RestartPaneArgs,
    ctx: &bmux_plugin_sdk::NativeServiceContext,
) -> Result<PaneAck, PaneCommandError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed_command("pane-runtime manager handle not registered"))?;
    let session_id = SessionId(req.session_id);
    ensure_session_mutation_allowed(ctx, session_id, "pane.restart")?;
    let pane_id = handle
        .0
        .restart_pane(session_id, target_selector(req.target))
        .map_err(|e| failed_command(e.to_string()))?;
    publish_pane_event(PaneEvent::Restarted {
        session_id: session_id.0,
        pane_id,
    });
    emit_attach_view_changed_scene(session_id);
    Ok(PaneAck {
        session_id: req.session_id,
        pane_id,
    })
}

pub fn zoom_pane(
    req: &ZoomPaneArgs,
    ctx: &bmux_plugin_sdk::NativeServiceContext,
) -> Result<PaneAck, PaneCommandError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed_command("pane-runtime manager handle not registered"))?;
    let session_id = SessionId(req.session_id);
    ensure_session_mutation_allowed(ctx, session_id, "pane.zoom")?;
    let (pane_id, _zoomed) = handle
        .0
        .toggle_zoom(session_id)
        .map_err(|e| failed_command(e.to_string()))?;
    emit_attach_view_changed_scene(session_id);
    Ok(PaneAck {
        session_id: req.session_id,
        pane_id,
    })
}

pub fn create_floating_pane(
    req: CreateFloatingPaneArgs,
    ctx: &bmux_plugin_sdk::NativeServiceContext,
) -> Result<FloatingPaneAck, PaneCommandError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed_command("pane-runtime manager handle not registered"))?;
    let session_id = SessionId(req.session_id);
    ensure_session_mutation_allowed(ctx, session_id, "pane.floating.create")?;
    let command = req.program.map(|program| PaneLaunchCommand {
        program,
        args: req.args,
        cwd: req.cwd,
        env: std::collections::BTreeMap::new(),
    });
    let summary = handle
        .0
        .create_floating_pane(
            session_id,
            target_selector(req.target),
            LayoutRect {
                x: req.x.unwrap_or(2),
                y: req.y.unwrap_or(2),
                w: req.w.unwrap_or(80).max(1),
                h: req.h.unwrap_or(24).max(1),
            },
            parse_floating_scope(req.scope.as_deref())?,
            parse_floating_layer(req.layer.as_deref())?,
            req.z.unwrap_or(0),
            req.name,
            command,
            req.anchor_pane_id,
            req.context_id,
            req.client_id.or(ctx.caller_client_id).map(ClientId),
        )
        .map_err(|e| failed_command(e.to_string()))?;
    emit_attach_view_changed_scene_all();
    Ok(floating_ack(session_id, summary))
}

pub fn move_floating_pane(
    req: &MoveFloatingPaneArgs,
    ctx: &bmux_plugin_sdk::NativeServiceContext,
) -> Result<FloatingPaneAck, PaneCommandError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed_command("pane-runtime manager handle not registered"))?;
    let session_id = SessionId(req.session_id);
    ensure_session_mutation_allowed(ctx, session_id, "pane.floating.move")?;
    let summary = handle
        .0
        .move_floating_pane(session_id, req.pane_id, req.x, req.y)
        .map_err(|e| failed_command(e.to_string()))?;
    emit_attach_view_changed_scene_all();
    Ok(floating_ack(session_id, summary))
}

pub fn resize_floating_pane(
    req: &ResizeFloatingPaneArgs,
    ctx: &bmux_plugin_sdk::NativeServiceContext,
) -> Result<FloatingPaneAck, PaneCommandError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed_command("pane-runtime manager handle not registered"))?;
    let session_id = SessionId(req.session_id);
    ensure_session_mutation_allowed(ctx, session_id, "pane.floating.resize")?;
    let summary = handle
        .0
        .resize_floating_pane(session_id, req.pane_id, req.w.max(1), req.h.max(1))
        .map_err(|e| failed_command(e.to_string()))?;
    emit_attach_view_changed_scene_all();
    Ok(floating_ack(session_id, summary))
}

fn mutate_floating_target(
    req: &FloatingPaneTargetArgs,
    ctx: &bmux_plugin_sdk::NativeServiceContext,
    action: &str,
    mutate: impl FnOnce(
        &bmux_pane_runtime_state::SessionRuntimeManagerHandle,
        SessionId,
        Uuid,
    ) -> anyhow::Result<bmux_pane_runtime_state::FloatingPaneRuntimeSummary>,
) -> Result<FloatingPaneAck, PaneCommandError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed_command("pane-runtime manager handle not registered"))?;
    let session_id = SessionId(req.session_id);
    ensure_session_mutation_allowed(ctx, session_id, action)?;
    let summary =
        mutate(&handle, session_id, req.pane_id).map_err(|e| failed_command(e.to_string()))?;
    emit_attach_view_changed_scene_all();
    Ok(floating_ack(session_id, summary))
}

pub fn focus_floating_pane(
    req: &FloatingPaneTargetArgs,
    ctx: &bmux_plugin_sdk::NativeServiceContext,
) -> Result<FloatingPaneAck, PaneCommandError> {
    mutate_floating_target(
        req,
        ctx,
        "pane.floating.focus",
        |handle, session_id, pane_id| handle.0.focus_floating_pane(session_id, pane_id),
    )
}

pub fn raise_floating_pane(
    req: &FloatingPaneTargetArgs,
    ctx: &bmux_plugin_sdk::NativeServiceContext,
) -> Result<FloatingPaneAck, PaneCommandError> {
    mutate_floating_target(
        req,
        ctx,
        "pane.floating.raise",
        |handle, session_id, pane_id| handle.0.raise_floating_pane(session_id, pane_id),
    )
}

pub fn lower_floating_pane(
    req: &FloatingPaneTargetArgs,
    ctx: &bmux_plugin_sdk::NativeServiceContext,
) -> Result<FloatingPaneAck, PaneCommandError> {
    mutate_floating_target(
        req,
        ctx,
        "pane.floating.lower",
        |handle, session_id, pane_id| handle.0.lower_floating_pane(session_id, pane_id),
    )
}

pub fn set_floating_pane_z(
    req: &SetFloatingPaneZArgs,
    ctx: &bmux_plugin_sdk::NativeServiceContext,
) -> Result<FloatingPaneAck, PaneCommandError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed_command("pane-runtime manager handle not registered"))?;
    let session_id = SessionId(req.session_id);
    ensure_session_mutation_allowed(ctx, session_id, "pane.floating.set_z")?;
    let summary = handle
        .0
        .set_floating_pane_z(session_id, req.pane_id, req.z)
        .map_err(|e| failed_command(e.to_string()))?;
    emit_attach_view_changed_scene_all();
    Ok(floating_ack(session_id, summary))
}

pub fn set_floating_pane_layer(
    req: &SetFloatingPaneLayerArgs,
    ctx: &bmux_plugin_sdk::NativeServiceContext,
) -> Result<FloatingPaneAck, PaneCommandError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed_command("pane-runtime manager handle not registered"))?;
    let session_id = SessionId(req.session_id);
    ensure_session_mutation_allowed(ctx, session_id, "pane.floating.set_layer")?;
    let summary = handle
        .0
        .set_floating_pane_layer(
            session_id,
            req.pane_id,
            parse_floating_layer(Some(&req.layer))?,
        )
        .map_err(|e| failed_command(e.to_string()))?;
    emit_attach_view_changed_scene_all();
    Ok(floating_ack(session_id, summary))
}

pub fn close_floating_pane(
    req: &FloatingPaneTargetArgs,
    ctx: &bmux_plugin_sdk::NativeServiceContext,
) -> Result<FloatingPaneAck, PaneCommandError> {
    let ack = close_pane(
        &ClosePaneArgs {
            session_id: req.session_id,
            target: Some(req.pane_id),
        },
        ctx,
    )?;
    emit_attach_view_changed_scene_all();
    Ok(FloatingPaneAck {
        session_id: ack.session_id,
        pane_id: ack.pane_id,
        surface_id: Uuid::nil(),
    })
}

pub fn pane_direct_input(
    req: PaneDirectInputArgs,
    ctx: &bmux_plugin_sdk::NativeServiceContext,
) -> Result<PaneAck, PaneCommandError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed_command("pane-runtime manager handle not registered"))?;
    let session_id = SessionId(req.session_id);
    let client_id = ctx
        .caller_client_id
        .map(bmux_session_models::ClientId)
        .ok_or_else(|| {
            warn!(session_id = %req.session_id, pane_id = %req.pane_id, "pane direct input rejected: missing caller client id");
            failed_command("pane direct input requires a caller client id")
        })?;
    if !handle.0.client_can_write(session_id, client_id) {
        warn!(session_id = %req.session_id, pane_id = %req.pane_id, client_id = %client_id.0, "pane direct input rejected: client lacks write permission");
        return Err(PaneCommandError::Denied {
            reason: "client does not have write permission for this attach stream".to_string(),
        });
    }
    match handle
        .0
        .write_input_to_pane(session_id, client_id, req.pane_id, req.data)
    {
        Ok(_) => {}
        Err(SessionRuntimeError::Closed)
            if handle.0.pane_state(session_id, req.pane_id) == Some(PaneState::Exited) => {}
        Err(error) => {
            warn!(session_id = %req.session_id, pane_id = %req.pane_id, client_id = %client_id.0, %error, "pane direct input failed");
            return Err(failed_command(error.to_string()));
        }
    }
    Ok(PaneAck {
        session_id: req.session_id,
        pane_id: req.pane_id,
    })
}

pub fn set_client_write_permission(
    req: &SetClientWritePermissionArgs,
) -> Result<u8, PaneCommandError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed_command("pane-runtime manager handle not registered"))?;
    handle.0.set_client_write_permission(
        SessionId(req.session_id),
        bmux_session_models::ClientId(req.client_id),
        req.allowed,
    );
    Ok(u8::from(req.allowed))
}

pub fn new_session_with_runtime(
    req: &NewSessionArgs,
) -> Result<SessionAck, SessionRuntimeCommandError> {
    use bmux_plugin::global_plugin_state_registry;

    let runtime_handle = super::session_runtime_handle()
        .ok_or_else(|| failed_session("pane-runtime manager handle not registered"))?;
    let session_handle = global_plugin_state_registry()
        .get::<bmux_session_state::SessionManagerHandle>()
        .and_then(|arc| arc.read().ok().map(|g| (*g).clone()))
        .ok_or_else(|| failed_session("session manager handle not registered"))?;

    // Sessions are identified by UUID. Names are display hints that
    // may be duplicated deliberately by the caller — the handler
    // does not reject a session just because another session shares
    // its name. The `name-already-exists` error variant was removed
    // from the BPDL wire type for this reason.

    let session_id = session_handle
        .0
        .create_session(req.name.clone())
        .map_err(|error| failed_session(format!("failed creating session: {error:#}")))?;

    if let Err(error) = runtime_handle.0.start_runtime(session_id) {
        let _ = session_handle.0.remove_session(session_id);
        return Err(failed_session(format!(
            "failed creating session runtime: {error:#}"
        )));
    }

    // NOTE: this handler deliberately does NOT create a context for the
    // new session. Context lifecycle is owned by `bmux.contexts`
    // plugin's `create_context_local`, which is the caller that drives
    // this function via `sessions-commands::new-session` ->
    // `pane-runtime-commands::new-session-with-runtime`. Creating a
    // second context here (with a nil caller client id) produced a
    // "shadow tab" duplicated per keystroke (see the bmux
    // `context_state.create` trace). Keep this function focused on
    // session + runtime allocation; let the caller own context
    // creation.

    Ok(SessionAck {
        session_id: session_id.0,
    })
}

pub fn kill_session_runtime(
    req: &KillSessionArgs,
) -> Result<SessionAck, SessionRuntimeCommandError> {
    use bmux_plugin::global_plugin_state_registry;

    let runtime_handle = super::session_runtime_handle()
        .ok_or_else(|| failed_session("pane-runtime manager handle not registered"))?;
    let session_handle = global_plugin_state_registry()
        .get::<bmux_session_state::SessionManagerHandle>()
        .and_then(|arc| arc.read().ok().map(|g| (*g).clone()))
        .ok_or_else(|| failed_session("session manager handle not registered"))?;
    let context_handle = global_plugin_state_registry()
        .get::<bmux_context_state::ContextStateHandle>()
        .and_then(|arc| arc.read().ok().map(|g| (*g).clone()))
        .ok_or_else(|| failed_session("context state handle not registered"))?;
    let attach_token_handle = global_plugin_state_registry()
        .get::<bmux_attach_token_state::AttachTokenManagerHandle>()
        .and_then(|arc| arc.read().ok().map(|g| (*g).clone()))
        .ok_or_else(|| failed_session("attach-token manager handle not registered"))?;
    let follow_handle = global_plugin_state_registry()
        .get::<bmux_client_state::FollowStateHandle>()
        .and_then(|arc| arc.read().ok().map(|g| (*g).clone()))
        .ok_or_else(|| failed_session("follow state handle not registered"))?;
    let session_id = SessionId(req.session_id);
    let _ = req.force_local; // admin-principal gate + remote tear-down remain server-side for now.

    if session_handle.0.remove_session(session_id).is_err() {
        return Err(SessionRuntimeCommandError::SessionNotFound);
    }

    let _removed_contexts = context_handle.0.remove_contexts_for_session(session_id);
    follow_handle.0.clear_selections_for_session(session_id);

    let Some(removed_runtime) = runtime_handle.0.remove_runtime(session_id) else {
        return Err(failed_session(format!(
            "failed stopping session runtime: session {} not found",
            session_id.0
        )));
    };

    let had_attached_clients = !removed_runtime.attached_clients.is_empty();
    runtime_handle.0.shutdown_removed_runtime(removed_runtime);
    attach_token_handle.0.remove_for_session(session_id);

    if had_attached_clients {
        publish_pane_event(PaneEvent::ClientDetached {
            session_id: session_id.0,
        });
    }

    publish_session_removed_event(session_id);

    Ok(SessionAck {
        session_id: req.session_id,
    })
}

pub fn restore_session_runtime() -> Result<SessionAck, SessionRuntimeCommandError> {
    Err(failed_session(
        "restore-session-runtime is driven by the snapshot orchestrator on startup; \
         clients should not invoke it directly",
    ))
}
