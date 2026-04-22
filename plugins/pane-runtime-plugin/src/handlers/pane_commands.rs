//! Typed handlers for the `pane-runtime-commands` interface.
//!
//! Each handler decodes its `...Args` wire struct, dispatches through
//! the registered `SessionRuntimeManagerHandle`, and serializes the
//! BPDL result type. When the manager handle is not registered
//! (`session_runtime_handle()` returns `None`), the handler reports a
//! `PaneCommandError::Failed { reason }` / `SessionRuntimeCommandError::Failed`.

use bmux_ipc::{PaneFocusDirection, PaneLaunchCommand, PaneSelector, PaneSplitDirection};
use bmux_pane_runtime_plugin_api::pane_runtime_commands::{
    PaneAck, PaneCommandError, SessionAck, SessionRuntimeCommandError,
};
use bmux_session_models::SessionId;
use serde::{Deserialize, Serialize};
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
    pub delta_percent: i8,
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
pub struct PaneDirectInputArgs {
    pub session_id: Uuid,
    pub pane_id: Uuid,
    pub data: Vec<u8>,
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
#[derive(serde::Serialize)]
struct SessionPolicyCheckRequest {
    session_id: Uuid,
    context_id: Option<Uuid>,
    client_id: Uuid,
    principal_id: Uuid,
    action: String,
    plugin_id: Option<String>,
    capability: Option<String>,
    execution_class: Option<String>,
}

#[derive(serde::Deserialize)]
struct SessionPolicyCheckResponse {
    allowed: bool,
    #[serde(default)]
    reason: Option<String>,
}

/// Perform the `bmux.sessions.policy / session-policy-query/v1 / check`
/// policy query for a mutating pane/session operation. Returns `Ok(())`
/// when the policy allows the operation (or no policy provider is
/// registered), or a `PaneCommandError::Denied` when the policy
/// responds with `allowed = false`.
fn ensure_session_mutation_allowed(
    ctx: &bmux_plugin_sdk::NativeServiceContext,
    session_id: SessionId,
    action: &str,
) -> Result<(), PaneCommandError> {
    use bmux_plugin::ServiceCaller;

    let client_id = ctx
        .caller_client_id
        .ok_or_else(|| failed_command("policy check requires a caller client id"))?;
    let principal_id = bmux_plugin::global_plugin_state_registry()
        .get::<bmux_client_state::ClientPrincipalHandle>()
        .and_then(|arc| arc.read().ok().map(|g| (*g).clone()))
        .and_then(|handle| handle.0.get(bmux_session_models::ClientId(client_id)))
        .unwrap_or_else(Uuid::nil);

    let request = SessionPolicyCheckRequest {
        session_id: session_id.0,
        context_id: None,
        client_id,
        principal_id,
        action: action.to_string(),
        plugin_id: None,
        capability: None,
        execution_class: None,
    };

    // The policy service may not be registered in tests or headless
    // tooling. Treat a missing provider as "allowed" (matches server's
    // legacy behavior where `check_session_policy` returns `None` when
    // no resolver is installed).
    match ctx.call_service::<SessionPolicyCheckRequest, SessionPolicyCheckResponse>(
        "bmux.sessions.policy",
        bmux_plugin_sdk::ServiceKind::Query,
        "session-policy-query/v1",
        "check",
        &request,
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
        Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation { .. }) => Ok(()),
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

fn target_selector(target: Option<Uuid>) -> Option<PaneSelector> {
    target.map(PaneSelector::ById)
}

/// Publish a wire event through the registered `WireEventSinkHandle`.
/// When no sink is registered (tests/headless tooling) the publish
/// is silently dropped.
fn publish_wire_event(event: bmux_ipc::Event) {
    if let Some(sink) = bmux_plugin::global_plugin_state_registry()
        .get::<bmux_plugin_sdk::WireEventSinkHandle>()
        .and_then(|arc| arc.read().ok().map(|g| (*g).clone()))
    {
        let _ = sink.0.publish(event);
    }
}

/// Bump a session's attach-view revision and publish the scene
/// component update. Mirrors the server's
/// `emit_attach_view_changed_for_layout` helper so plugin-side
/// handlers keep parity with the old IPC-handler behavior.
fn emit_attach_view_changed_scene(session_id: SessionId) {
    let Some(handle) = super::session_runtime_handle() else {
        return;
    };
    let Some(revision) = handle.0.bump_attach_view_revision(session_id) else {
        return;
    };
    publish_wire_event(bmux_ipc::Event::AttachViewChanged {
        context_id: None,
        session_id: session_id.0,
        revision,
        components: vec![bmux_ipc::AttachViewComponent::Scene],
    });
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
    handle
        .0
        .resize_pane(
            session_id,
            target_selector(req.target),
            i16::from(req.delta_percent),
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
            publish_wire_event(bmux_ipc::Event::ClientDetached { id: session_id.0 });
        }
        publish_wire_event(bmux_ipc::Event::SessionRemoved { id: session_id.0 });
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
    publish_wire_event(bmux_ipc::Event::PaneRestarted {
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

pub fn pane_direct_input(
    req: PaneDirectInputArgs,
    ctx: &bmux_plugin_sdk::NativeServiceContext,
) -> Result<PaneAck, PaneCommandError> {
    let handle = super::session_runtime_handle()
        .ok_or_else(|| failed_command("pane-runtime manager handle not registered"))?;
    let session_id = SessionId(req.session_id);
    ensure_session_mutation_allowed(ctx, session_id, "pane.direct_input")?;
    handle
        .0
        .write_input_to_pane(session_id, req.pane_id, req.data)
        .map_err(|e| failed_command(e.to_string()))?;
    Ok(PaneAck {
        session_id: req.session_id,
        pane_id: req.pane_id,
    })
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
    let context_handle = global_plugin_state_registry()
        .get::<bmux_context_state::ContextStateHandle>()
        .and_then(|arc| arc.read().ok().map(|g| (*g).clone()))
        .ok_or_else(|| failed_session("context state handle not registered"))?;

    // Duplicate-name check mirrors server's create_session_runtime helper.
    if let Some(requested_name) = req.name.as_deref()
        && session_handle
            .0
            .list_sessions()
            .iter()
            .any(|s| s.name.as_deref() == Some(requested_name))
    {
        return Err(SessionRuntimeCommandError::NameAlreadyExists {
            name: requested_name.to_string(),
        });
    }

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

    // Create+bind a context for the new session. The caller client id
    // is not available in this handler context (the typed invoke
    // context uses `NativeServiceContext` which does carry it, but we
    // accept the caller-less shape here because every code path that
    // reaches `new-session-with-runtime` in production goes through
    // either the sessions-plugin shim or a direct CLI call, both of
    // which pass through a `NativeServiceContext`). When the caller
    // id is absent we fall back to `Uuid::nil` which the context
    // state accepts.
    let caller_client_id = bmux_session_models::ClientId(uuid::Uuid::nil());
    let context = context_handle.0.create(
        caller_client_id,
        req.name.clone(),
        std::collections::BTreeMap::new(),
    );
    if let Err(message) = context_handle.0.bind_session(context.id, session_id) {
        let _ = context_handle
            .0
            .remove_context_by_id(context.id, Some(caller_client_id));
        let _ = session_handle.0.remove_session(session_id);
        if let Some(removed_runtime) = runtime_handle.0.remove_runtime(session_id) {
            runtime_handle.0.shutdown_removed_runtime(removed_runtime);
        }
        return Err(failed_session(format!(
            "failed creating context for new session: {message}"
        )));
    }

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
    let wire_sink = global_plugin_state_registry()
        .get::<bmux_plugin_sdk::WireEventSinkHandle>()
        .and_then(|arc| arc.read().ok().map(|g| (*g).clone()));

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

    if let Some(sink) = wire_sink {
        if had_attached_clients {
            let _ = sink
                .0
                .publish(bmux_ipc::Event::ClientDetached { id: session_id.0 });
        }
        let _ = sink
            .0
            .publish(bmux_ipc::Event::SessionRemoved { id: session_id.0 });
    }

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
