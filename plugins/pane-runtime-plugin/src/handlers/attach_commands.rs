//! Typed handlers for the `attach-runtime-commands` interface.
//!
//! Port of the server's former `Request::Attach`, `Request::AttachContext`,
//! `Request::AttachOpen`, `Request::AttachInput`, `Request::AttachOutput`,
//! `Request::AttachSetViewport`, `Request::SetClientAttachPolicy`, and
//! `Request::Detach` IPC handlers. The plugin owns the full
//! orchestration: session-manager membership updates, follow-state
//! selection sync, attach-token lifecycle, runtime begin/end attach,
//! and wire-event emission.

use bmux_attach_token_state::AttachTokenValidationError;
use bmux_ipc::{AttachGrant, ContextSelector, Event, SessionSelector};
use bmux_pane_runtime_plugin_api::attach_runtime_commands::{
    AttachCommandError, AttachGrant as AttachGrantRecord, AttachOutput as AttachOutputRecord,
    AttachReady, AttachRetargetReady, AttachViewportSet,
};
use bmux_pane_runtime_state::SessionRuntimeError;
use bmux_plugin::global_plugin_state_registry;
use bmux_plugin_sdk::{NativeServiceContext, WireEventSinkHandle};
use bmux_session_models::{ClientId, SessionId};
use serde::{Deserialize, Serialize};
use std::time::Instant;
use uuid::Uuid;

const ATTACH_PHASE_MARKER: &str = "[bmux-attach-phase-json]";

// ── Wire-format argument structs ─────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachSessionArgs {
    pub selector: SessionSelector,
    #[serde(default)]
    pub can_write: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachContextArgs {
    pub selector: ContextSelector,
    #[serde(default)]
    pub can_write: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachOpenArgs {
    pub session_id: Uuid,
    pub attach_token: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachInputArgs {
    pub session_id: Uuid,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachOutputArgs {
    pub session_id: Uuid,
    pub max_bytes: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachSetViewportArgs {
    pub session_id: Uuid,
    pub cols: u16,
    pub rows: u16,
    pub status_top_inset: u16,
    pub status_bottom_inset: u16,
    pub cell_pixel_width: u16,
    pub cell_pixel_height: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachRetargetContextArgs {
    pub context_id: Uuid,
    #[serde(default)]
    pub can_write: bool,
    pub cols: u16,
    pub rows: u16,
    pub status_top_inset: u16,
    pub status_bottom_inset: u16,
    pub cell_pixel_width: u16,
    pub cell_pixel_height: u16,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SetClientAttachPolicyArgs {
    pub allow_detach: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
pub struct DetachArgs;

// ── Helpers ──────────────────────────────────────────────────────

fn failed(reason: impl Into<String>) -> AttachCommandError {
    AttachCommandError::Failed {
        reason: reason.into(),
    }
}

fn caller_client_id(ctx: &NativeServiceContext) -> Result<ClientId, AttachCommandError> {
    ctx.caller_client_id
        .map(ClientId)
        .ok_or_else(|| failed("attach operation requires a caller client id"))
}

fn publish_event(event: Event) {
    if let Some(sink) = global_plugin_state_registry()
        .get::<WireEventSinkHandle>()
        .and_then(|arc| arc.read().ok().map(|g| (*g).clone()))
    {
        let _ = sink.0.publish(event);
    }
}

fn emit_attach_phase_timing(payload: &serde_json::Value) {
    if std::env::var_os("BMUX_ATTACH_PHASE_TIMING").is_none() {
        return;
    }
    eprintln!("{ATTACH_PHASE_MARKER}{payload}");
}

struct AttachRetargetTiming {
    context_id: Uuid,
    selected_session_id: SessionId,
    previous_session: Option<SessionId>,
    previous_stream: Option<SessionId>,
    runtime_start_attempted: bool,
    handle_lookup_us: u128,
    context_select_us: u128,
    membership_us: u128,
    runtime_check_us: u128,
    stream_detach_us: u128,
    stream_begin_us: u128,
    focus_publish_us: u128,
    viewport_set_us: u128,
    total_us: u128,
}

struct AttachStreamTiming {
    previous_stream: Option<SessionId>,
    runtime_start_attempted: bool,
    stream_detach_us: u128,
    stream_begin_us: u128,
    focus_publish_us: u128,
}

fn emit_attach_retarget_timing(timing: &AttachRetargetTiming) {
    emit_attach_phase_timing(&serde_json::json!({
        "phase": "attach.retarget_service",
        "context_id": timing.context_id,
        "selected_session_id": timing.selected_session_id.0,
        "previous_session_id": timing.previous_session.map(|id| id.0),
        "previous_stream_session_id": timing.previous_stream.map(|id| id.0),
        "runtime_start_attempted": timing.runtime_start_attempted,
        "handle_lookup_us": timing.handle_lookup_us,
        "context_select_us": timing.context_select_us,
        "membership_us": timing.membership_us,
        "runtime_check_us": timing.runtime_check_us,
        "stream_detach_us": timing.stream_detach_us,
        "stream_begin_us": timing.stream_begin_us,
        "focus_publish_us": timing.focus_publish_us,
        "viewport_set_us": timing.viewport_set_us,
        "total_us": timing.total_us,
    }));
}

fn begin_attach_stream_for_retarget(
    runtime: &bmux_pane_runtime_state::SessionRuntimeManagerHandle,
    follow: &bmux_client_state::FollowStateHandle,
    client_id: ClientId,
    next_session_id: SessionId,
    mut runtime_start_attempted: bool,
) -> Result<AttachStreamTiming, AttachCommandError> {
    let detach_started = Instant::now();
    let previous_stream = follow.0.attached_stream_session(client_id);
    if let Some(prev) = previous_stream
        && prev != next_session_id
    {
        runtime.0.end_attach(prev, client_id);
        publish_event(Event::ClientDetached { id: prev.0 });
    }
    let stream_detach_us = detach_started.elapsed().as_micros();

    let begin_started = Instant::now();
    let begin_result = match runtime.0.begin_attach(next_session_id, client_id) {
        Ok(()) => Ok(()),
        Err(SessionRuntimeError::NotFound) => {
            runtime_start_attempted = true;
            let _ = runtime.0.start_runtime(next_session_id);
            runtime.0.begin_attach(next_session_id, client_id)
        }
        Err(SessionRuntimeError::Closed) => {
            if let Some(removed) = runtime.0.remove_runtime(next_session_id) {
                runtime.0.shutdown_removed_runtime(removed);
            }
            runtime_start_attempted = true;
            let _ = runtime.0.start_runtime(next_session_id);
            runtime.0.begin_attach(next_session_id, client_id)
        }
        Err(err) => Err(err),
    };
    let stream_begin_us = begin_started.elapsed().as_micros();

    let publish_started = Instant::now();
    match begin_result {
        Ok(()) => {
            follow
                .0
                .set_attached_stream_session(client_id, Some(next_session_id));
            publish_event(Event::ClientAttached {
                id: next_session_id.0,
            });
            super::publish_focus_state_snapshot();
        }
        Err(SessionRuntimeError::NotFound | SessionRuntimeError::Closed) => {
            return Err(AttachCommandError::SessionNotFound);
        }
        Err(SessionRuntimeError::NotAttached) => {
            return Err(failed("failed opening attach stream"));
        }
    }
    Ok(AttachStreamTiming {
        previous_stream,
        runtime_start_attempted,
        stream_detach_us,
        stream_begin_us,
        focus_publish_us: publish_started.elapsed().as_micros(),
    })
}

fn session_manager() -> Result<bmux_session_state::SessionManagerHandle, AttachCommandError> {
    global_plugin_state_registry()
        .get::<bmux_session_state::SessionManagerHandle>()
        .and_then(|arc| arc.read().ok().map(|g| (*g).clone()))
        .ok_or_else(|| failed("session manager handle not registered"))
}

fn context_state() -> Result<bmux_context_state::ContextStateHandle, AttachCommandError> {
    global_plugin_state_registry()
        .get::<bmux_context_state::ContextStateHandle>()
        .and_then(|arc| arc.read().ok().map(|g| (*g).clone()))
        .ok_or_else(|| failed("context state handle not registered"))
}

fn follow_state() -> Result<bmux_client_state::FollowStateHandle, AttachCommandError> {
    global_plugin_state_registry()
        .get::<bmux_client_state::FollowStateHandle>()
        .and_then(|arc| arc.read().ok().map(|g| (*g).clone()))
        .ok_or_else(|| failed("follow state handle not registered"))
}

fn attach_token_handle()
-> Result<bmux_attach_token_state::AttachTokenManagerHandle, AttachCommandError> {
    global_plugin_state_registry()
        .get::<bmux_attach_token_state::AttachTokenManagerHandle>()
        .and_then(|arc| arc.read().ok().map(|g| (*g).clone()))
        .ok_or_else(|| failed("attach-token manager handle not registered"))
}

fn resolve_session_by_selector(
    manager: &dyn bmux_session_state::SessionManagerReader,
    selector: &SessionSelector,
) -> Option<SessionId> {
    match selector {
        SessionSelector::ById(id) => {
            let sid = SessionId(*id);
            if manager.contains(sid) {
                Some(sid)
            } else {
                None
            }
        }
        SessionSelector::ByName(name) => manager
            .list_sessions()
            .into_iter()
            .find(|info| info.name.as_deref() == Some(name.as_str()))
            .map(|info| info.id),
    }
}

fn to_api_grant(grant: &AttachGrant) -> AttachGrantRecord {
    AttachGrantRecord {
        token: grant.attach_token,
        session_id: grant.session_id,
        context_id: grant.context_id,
        expires_epoch_ms: grant.expires_at_epoch_ms,
    }
}

// ── Handler bodies ───────────────────────────────────────────────

pub fn attach_session(
    req: &AttachSessionArgs,
    ctx: &NativeServiceContext,
) -> Result<AttachGrantRecord, AttachCommandError> {
    let client_id = caller_client_id(ctx)?;
    let manager = session_manager()?;
    let follow = follow_state()?;

    let Some(next_session_id) = resolve_session_by_selector(&*manager.0, &req.selector) else {
        return Err(AttachCommandError::SessionNotFound);
    };

    // Transition client membership off the old session if changing.
    let previous_session = follow.0.selected_session(client_id);
    if let Some(prev) = previous_session
        && prev != next_session_id
    {
        manager.0.remove_client(prev, &client_id);
    }

    if !manager.0.contains(next_session_id) {
        // Session vanished between selector resolution and the add
        // attempt; prune any stale context mappings.
        let _ = context_state()?
            .0
            .remove_contexts_for_session(next_session_id);
        return Err(AttachCommandError::SessionNotFound);
    }

    manager.0.add_client(next_session_id, client_id);

    // Update FollowState. Preserve any existing selected context
    // unless one maps to the chosen session.
    let selected_context = context_state()?
        .0
        .context_for_session(next_session_id)
        .or_else(|| follow.0.selected_context(client_id));
    follow
        .0
        .set_selected_target(client_id, selected_context, Some(next_session_id));

    // Issue the grant with context decoration.
    let mut grant = attach_token_handle()?.0.issue(next_session_id);
    grant.context_id = selected_context;
    Ok(to_api_grant(&grant))
}

pub fn attach_context(
    req: &AttachContextArgs,
    ctx: &NativeServiceContext,
) -> Result<AttachGrantRecord, AttachCommandError> {
    let client_id = caller_client_id(ctx)?;
    let manager = session_manager()?;
    let contexts = context_state()?;
    let follow = follow_state()?;

    let context = contexts
        .0
        .select_for_client(client_id, &req.selector)
        .map_err(|m| failed(m.to_string()))?;

    let Some(next_session_id) = contexts.0.current_session_for_client(client_id) else {
        return Err(failed("context has no attached runtime"));
    };

    let previous_session = follow.0.selected_session(client_id);
    if let Some(prev) = previous_session
        && prev != next_session_id
    {
        manager.0.remove_client(prev, &client_id);
    }

    if !manager.0.contains(next_session_id) {
        let _ = contexts.0.remove_contexts_for_session(next_session_id);
        return Err(AttachCommandError::SessionNotFound);
    }

    manager.0.add_client(next_session_id, client_id);
    follow
        .0
        .set_selected_target(client_id, Some(context.id), Some(next_session_id));

    let mut grant = attach_token_handle()?.0.issue(next_session_id);
    grant.context_id = Some(context.id);
    Ok(to_api_grant(&grant))
}

pub fn attach_open(
    req: &AttachOpenArgs,
    ctx: &NativeServiceContext,
) -> Result<AttachReady, AttachCommandError> {
    let client_id = caller_client_id(ctx)?;
    let session_id = SessionId(req.session_id);
    let runtime = super::session_runtime_handle()
        .ok_or_else(|| failed("pane-runtime manager handle not registered"))?;
    let tokens = attach_token_handle()?;
    let follow = follow_state()?;

    // Ensure the session runtime exists (start it if the session
    // manager has the entry but the runtime is missing).
    let manager = session_manager()?;
    if !manager.0.contains(session_id) {
        return Err(AttachCommandError::SessionNotFound);
    }
    if !runtime.0.session_exists(session_id)
        && let Err(err) = runtime.0.start_runtime(session_id)
    {
        return Err(failed(format!(
            "failed restarting missing session runtime {}: {err:#}",
            session_id.0
        )));
    }

    match tokens.0.consume(session_id, req.attach_token) {
        Ok(()) => {}
        Err(AttachTokenValidationError::NotFound | AttachTokenValidationError::SessionMismatch) => {
            return Err(AttachCommandError::InvalidGrant);
        }
        Err(AttachTokenValidationError::Expired) => {
            return Err(AttachCommandError::ExpiredGrant);
        }
    }

    // End any previous attach stream for this client.
    let previous_stream = follow.0.attached_stream_session(client_id);
    if let Some(prev) = previous_stream
        && prev != session_id
    {
        runtime.0.end_attach(prev, client_id);
        publish_event(Event::ClientDetached { id: prev.0 });
    }

    // Begin attach with restart-on-NotFound/Closed semantics.
    let begin_result = match runtime.0.begin_attach(session_id, client_id) {
        Ok(()) => Ok(()),
        Err(SessionRuntimeError::NotFound) => {
            let _ = runtime.0.start_runtime(session_id);
            runtime.0.begin_attach(session_id, client_id)
        }
        Err(SessionRuntimeError::Closed) => {
            if let Some(removed) = runtime.0.remove_runtime(session_id) {
                runtime.0.shutdown_removed_runtime(removed);
            }
            let _ = runtime.0.start_runtime(session_id);
            runtime.0.begin_attach(session_id, client_id)
        }
        Err(err) => Err(err),
    };

    match begin_result {
        Ok(()) => {
            follow
                .0
                .set_attached_stream_session(client_id, Some(session_id));
            let context_id = context_state()?
                .0
                .current_for_client(client_id)
                .map(|c| c.id);
            publish_event(Event::ClientAttached { id: session_id.0 });
            // Publish focus state so the newly-attached client's
            // consumers (decoration, future status plugins) observe
            // the current focused pane without an extra round-trip.
            super::publish_focus_state_snapshot();
            Ok(AttachReady {
                session_id: session_id.0,
                context_id,
                can_write: true,
            })
        }
        Err(SessionRuntimeError::NotFound | SessionRuntimeError::Closed) => {
            Err(AttachCommandError::SessionNotFound)
        }
        Err(SessionRuntimeError::NotAttached) => Err(failed("failed opening attach stream")),
    }
}

pub fn attach_input(
    req: AttachInputArgs,
    ctx: &NativeServiceContext,
) -> Result<
    bmux_pane_runtime_plugin_api::attach_runtime_commands::AttachInputAccepted,
    AttachCommandError,
> {
    let client_id = caller_client_id(ctx)?;
    let session_id = SessionId(req.session_id);
    let runtime = super::session_runtime_handle()
        .ok_or_else(|| failed("pane-runtime manager handle not registered"))?;
    let data_len = req.data.len();
    match runtime.0.write_input(session_id, client_id, req.data) {
        Ok((bytes, _pane_id)) => Ok(
            bmux_pane_runtime_plugin_api::attach_runtime_commands::AttachInputAccepted {
                bytes: u32::try_from(bytes)
                    .unwrap_or_else(|_| u32::try_from(data_len).unwrap_or(u32::MAX)),
            },
        ),
        Err(SessionRuntimeError::NotFound | SessionRuntimeError::Closed) => {
            Err(AttachCommandError::SessionNotFound)
        }
        Err(SessionRuntimeError::NotAttached) => {
            Err(failed("client is not attached to session runtime"))
        }
    }
}

pub fn attach_output(
    req: &AttachOutputArgs,
    ctx: &NativeServiceContext,
) -> Result<AttachOutputRecord, AttachCommandError> {
    let client_id = caller_client_id(ctx)?;
    let session_id = SessionId(req.session_id);
    let runtime = super::session_runtime_handle()
        .ok_or_else(|| failed("pane-runtime manager handle not registered"))?;
    match runtime
        .0
        .read_output(session_id, client_id, req.max_bytes as usize)
    {
        Ok(data) => Ok(AttachOutputRecord { data }),
        Err(SessionRuntimeError::NotFound | SessionRuntimeError::Closed) => {
            Err(AttachCommandError::SessionNotFound)
        }
        Err(SessionRuntimeError::NotAttached) => {
            Err(failed("client is not attached to session runtime"))
        }
    }
}

pub fn attach_set_viewport(
    req: &AttachSetViewportArgs,
    ctx: &NativeServiceContext,
) -> Result<AttachViewportSet, AttachCommandError> {
    let client_id = caller_client_id(ctx)?;
    let session_id = SessionId(req.session_id);
    let runtime = super::session_runtime_handle()
        .ok_or_else(|| failed("pane-runtime manager handle not registered"))?;
    let (cols, rows, top, bottom) = runtime
        .0
        .set_attach_viewport(
            session_id,
            client_id,
            req.cols,
            req.rows,
            req.status_top_inset,
            req.status_bottom_inset,
            req.cell_pixel_width,
            req.cell_pixel_height,
        )
        .map_err(|e| failed(format!("failed setting attach viewport: {e:?}")))?;
    let context_id = context_state()?
        .0
        .current_for_client(client_id)
        .map(|c| c.id);
    Ok(AttachViewportSet {
        session_id: req.session_id,
        cols,
        rows,
        status_top_inset: top,
        status_bottom_inset: bottom,
        context_id,
    })
}

pub fn attach_retarget_context(
    req: &AttachRetargetContextArgs,
    ctx: &NativeServiceContext,
) -> Result<AttachRetargetReady, AttachCommandError> {
    let total_started = Instant::now();
    let client_id = caller_client_id(ctx)?;

    let handle_started = Instant::now();
    let manager = session_manager()?;
    let contexts = context_state()?;
    let follow = follow_state()?;
    let runtime = super::session_runtime_handle()
        .ok_or_else(|| failed("pane-runtime manager handle not registered"))?;
    let handle_lookup_us = handle_started.elapsed().as_micros();

    let select_started = Instant::now();
    let context = contexts
        .0
        .select_for_client(client_id, &ContextSelector::ById(req.context_id))
        .map_err(|m| failed(m.to_string()))?;
    let Some(next_session_id) = contexts.0.current_session_for_client(client_id) else {
        return Err(failed("context has no attached runtime"));
    };
    let context_select_us = select_started.elapsed().as_micros();

    let membership_started = Instant::now();
    let previous_session = follow.0.selected_session(client_id);
    if let Some(prev) = previous_session
        && prev != next_session_id
    {
        manager.0.remove_client(prev, &client_id);
    }

    if !manager.0.contains(next_session_id) {
        let _ = contexts.0.remove_contexts_for_session(next_session_id);
        return Err(AttachCommandError::SessionNotFound);
    }

    manager.0.add_client(next_session_id, client_id);
    follow
        .0
        .set_selected_target(client_id, Some(context.id), Some(next_session_id));
    let membership_us = membership_started.elapsed().as_micros();

    let runtime_started = Instant::now();
    let mut runtime_start_attempted = false;
    if !runtime.0.session_exists(next_session_id) {
        runtime_start_attempted = true;
        if let Err(err) = runtime.0.start_runtime(next_session_id) {
            return Err(failed(format!(
                "failed restarting missing session runtime {}: {err:#}",
                next_session_id.0
            )));
        }
    }
    let runtime_check_us = runtime_started.elapsed().as_micros();

    let stream_timing = begin_attach_stream_for_retarget(
        &runtime,
        &follow,
        client_id,
        next_session_id,
        runtime_start_attempted,
    )?;

    let viewport_started = Instant::now();
    let (cols, rows, top, bottom) = runtime
        .0
        .set_attach_viewport(
            next_session_id,
            client_id,
            req.cols,
            req.rows,
            req.status_top_inset,
            req.status_bottom_inset,
            req.cell_pixel_width,
            req.cell_pixel_height,
        )
        .map_err(|e| failed(format!("failed setting attach viewport: {e:?}")))?;
    let viewport_set_us = viewport_started.elapsed().as_micros();

    emit_attach_retarget_timing(&AttachRetargetTiming {
        context_id: req.context_id,
        selected_session_id: next_session_id,
        previous_session,
        previous_stream: stream_timing.previous_stream,
        runtime_start_attempted: stream_timing.runtime_start_attempted,
        handle_lookup_us,
        context_select_us,
        membership_us,
        runtime_check_us,
        stream_detach_us: stream_timing.stream_detach_us,
        stream_begin_us: stream_timing.stream_begin_us,
        focus_publish_us: stream_timing.focus_publish_us,
        viewport_set_us,
        total_us: total_started.elapsed().as_micros(),
    });

    Ok(AttachRetargetReady {
        session_id: next_session_id.0,
        context_id: Some(context.id),
        can_write: req.can_write,
        cols,
        rows,
        status_top_inset: top,
        status_bottom_inset: bottom,
    })
}

pub fn set_client_attach_policy(
    req: SetClientAttachPolicyArgs,
    ctx: &NativeServiceContext,
) -> Result<u8, AttachCommandError> {
    let client_id = caller_client_id(ctx)?;
    follow_state()?
        .0
        .set_attach_detach_allowed(client_id, req.allow_detach);
    Ok(u8::from(req.allow_detach))
}

pub fn detach(ctx: &NativeServiceContext) -> Result<u8, AttachCommandError> {
    let client_id = caller_client_id(ctx)?;
    let follow = follow_state()?;
    if !follow.0.attach_detach_allowed(client_id) {
        return Err(failed("detach is disabled for this connection"));
    }
    let runtime = super::session_runtime_handle()
        .ok_or_else(|| failed("pane-runtime manager handle not registered"))?;
    if let Some(stream_session) = follow.0.attached_stream_session(client_id) {
        runtime.0.end_attach(stream_session, client_id);
        follow.0.set_attached_stream_session(client_id, None);
        publish_event(Event::ClientDetached {
            id: stream_session.0,
        });
    }
    // Clear the selected session so future follow-state lookups see
    // the detach.
    if let Some(selected) = follow.0.selected_session(client_id) {
        let manager = session_manager()?;
        manager.0.remove_client(selected, &client_id);
        follow.0.set_selected_target(client_id, None, None);
    }
    Ok(0)
}
