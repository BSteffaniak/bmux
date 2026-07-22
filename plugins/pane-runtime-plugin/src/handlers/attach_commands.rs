//! Typed handlers for the `attach-runtime-commands` interface.
//!
//! Port of the server's former `Request::Attach`, `Request::AttachContext`,
//! `Request::AttachOpen`, `Request::AttachInput`, `Request::AttachOutput`,
//! `Request::AttachSetViewport`, `Request::SetClientAttachPolicy`, and
//! `Request::Detach` IPC handlers. The plugin owns the full
//! orchestration: session-manager membership updates, follow-state
//! selection sync, attach-token lifecycle, runtime begin/end attach,
//! and wire-event emission.

use bmux_attach_token_state::{AttachGrant, AttachTokenValidationError};
use bmux_context_state::ContextSelector as PrimitiveContextSelector;
use bmux_pane_runtime_plugin_api::attach_runtime_commands::{
    AttachCommandError, AttachGrant as AttachGrantRecord, AttachOutput as AttachOutputRecord,
    AttachReady, AttachRetargetReady, AttachViewportSet, ContextSelector, SessionSelector,
};
use bmux_pane_runtime_plugin_api::pane_runtime_events::{self, PaneEvent};
use bmux_pane_runtime_state::SessionRuntimeError;
use bmux_plugin::global_plugin_state_registry;
use bmux_plugin_sdk::NativeServiceContext;
use bmux_session_models::{ClientId, SessionId};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

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
    #[serde(with = "bmux_plugin_sdk::codec::serde_bytes_vec")]
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
    #[serde(alias = "cell_pixel_w")]
    pub cell_pixel_width: u16,
    #[serde(alias = "cell_pixel_h")]
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
    #[serde(alias = "cell_pixel_w")]
    pub cell_pixel_width: u16,
    #[serde(alias = "cell_pixel_h")]
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

fn session_write_allowed(
    ctx: &NativeServiceContext,
    session_id: SessionId,
    client_id: ClientId,
) -> Result<bool, AttachCommandError> {
    let principal_id = bmux_plugin::global_plugin_state_registry()
        .get::<bmux_client_state::ClientPrincipalHandle>()
        .and_then(|arc| arc.read().ok().map(|g| (*g).clone()))
        .and_then(|handle| handle.0.get(client_id))
        .unwrap_or_else(Uuid::nil);

    let mut client = bmux_plugin::ServiceCallerDispatchClient::new(ctx);
    match bmux_plugin::block_on_typed_dispatch(
        bmux_permissions_plugin_api::session_policy_state::client::check(
            &mut client,
            session_id.0,
            None,
            client_id.0,
            principal_id,
            "pane.direct_input".to_string(),
            None,
            None,
            None,
        ),
    ) {
        Ok(response) => Ok(response.allowed),
        Err(err) if err.to_string().contains("unsupported") => Ok(true),
        Err(err) => Err(failed(format!("session policy check failed: {err}"))),
    }
}

fn publish_pane_event(event: PaneEvent) {
    let _ = bmux_plugin::global_event_bus().emit(&pane_runtime_events::EVENT_KIND, event);
}

#[allow(
    clippy::too_many_arguments,
    reason = "handler forwards the wire retarget request's viewport dimensions to the runtime atomically"
)]
fn retarget_attach_stream(
    runtime: &bmux_pane_runtime_state::SessionRuntimeManagerHandle,
    follow: &bmux_client_state::FollowStateHandle,
    client_id: ClientId,
    next_session_id: SessionId,
    cols: u16,
    rows: u16,
    status_top_inset: u16,
    status_bottom_inset: u16,
    cell_pixel_width: u16,
    cell_pixel_height: u16,
) -> Result<(u16, u16, u16, u16), AttachCommandError> {
    let previous_stream = follow.0.attached_stream_session(client_id);
    let previous_to_detach = previous_stream.filter(|prev| *prev != next_session_id);
    let retarget_result = match runtime.0.retarget_attach_stream(
        previous_to_detach,
        next_session_id,
        client_id,
        cols,
        rows,
        status_top_inset,
        status_bottom_inset,
        cell_pixel_width,
        cell_pixel_height,
    ) {
        Ok(viewport) => Ok(viewport),
        Err(SessionRuntimeError::NotFound) => {
            let _ = runtime.0.start_runtime(next_session_id);
            runtime.0.retarget_attach_stream(
                previous_to_detach,
                next_session_id,
                client_id,
                cols,
                rows,
                status_top_inset,
                status_bottom_inset,
                cell_pixel_width,
                cell_pixel_height,
            )
        }
        Err(SessionRuntimeError::Closed) => {
            if let Some(removed) = runtime.0.remove_runtime(next_session_id) {
                runtime.0.shutdown_removed_runtime(removed);
            }
            let _ = runtime.0.start_runtime(next_session_id);
            runtime.0.retarget_attach_stream(
                previous_to_detach,
                next_session_id,
                client_id,
                cols,
                rows,
                status_top_inset,
                status_bottom_inset,
                cell_pixel_width,
                cell_pixel_height,
            )
        }
        Err(err) => Err(err),
    };

    match retarget_result {
        Ok(viewport) => {
            if let Some(prev) = previous_to_detach {
                runtime
                    .0
                    .set_client_write_permission(prev, client_id, false);
                publish_pane_event(PaneEvent::ClientDetached { session_id: prev.0 });
            }
            follow
                .0
                .set_attached_stream_session(client_id, Some(next_session_id));
            publish_pane_event(PaneEvent::ClientAttached {
                session_id: next_session_id.0,
            });
            // Retargeting changes which session's focused pane is relevant to
            // attach-local consumers (decoration, status bars). Republish the
            // full focus snapshot even when the pane focus itself did not
            // mutate so subscribers can reconcile after tab/window switches.
            super::publish_focus_state_snapshot();
            Ok(viewport)
        }
        Err(SessionRuntimeError::NotFound | SessionRuntimeError::Closed) => {
            Err(AttachCommandError::SessionNotFound)
        }
        Err(SessionRuntimeError::NotAttached) => Err(failed("failed opening attach stream")),
    }
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
    if let Some(id) = selector.id {
        let sid = SessionId(id);
        return manager.contains(sid).then_some(sid);
    }
    selector.name.as_ref().and_then(|name| {
        manager
            .list_sessions()
            .into_iter()
            .find(|info| info.name.as_deref() == Some(name.as_str()))
            .map(|info| info.id)
    })
}

fn context_selector_to_ipc(
    selector: &ContextSelector,
) -> Result<PrimitiveContextSelector, AttachCommandError> {
    if let Some(id) = selector.id {
        Ok(PrimitiveContextSelector::ById(id))
    } else if let Some(name) = selector.name.clone() {
        Ok(PrimitiveContextSelector::ByName(name))
    } else {
        Err(failed("attach-context requires a context selector"))
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
        .select_for_client(client_id, &context_selector_to_ipc(&req.selector)?)
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
        runtime
            .0
            .set_client_write_permission(prev, client_id, false);
        publish_pane_event(PaneEvent::ClientDetached { session_id: prev.0 });
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
            let can_write = session_write_allowed(ctx, session_id, client_id)?;
            runtime
                .0
                .set_client_write_permission(session_id, client_id, can_write);
            follow
                .0
                .set_attached_stream_session(client_id, Some(session_id));
            let context_id = context_state()?
                .0
                .current_for_client(client_id)
                .map(|c| c.id);
            publish_pane_event(PaneEvent::ClientAttached {
                session_id: session_id.0,
            });
            // Publish focus state so the newly-attached client's
            // consumers (decoration, future status plugins) observe
            // the current focused pane without an extra round-trip.
            super::publish_focus_state_snapshot();
            Ok(AttachReady {
                session_id: session_id.0,
                context_id,
                can_write,
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
    if !runtime.0.client_can_write(session_id, client_id) {
        return Err(AttachCommandError::Denied {
            reason: "client does not have write permission for this attach stream".to_string(),
        });
    }
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
    let client_id = caller_client_id(ctx)?;

    let manager = session_manager()?;
    let contexts = context_state()?;
    let follow = follow_state()?;
    let runtime = super::session_runtime_handle()
        .ok_or_else(|| failed("pane-runtime manager handle not registered"))?;

    let selected_target = follow.0.selected_target(client_id);
    let (context_id, next_session_id, selection_already_applied) =
        if let Some((Some(selected_context_id), Some(selected_session_id))) = selected_target
            && selected_context_id == req.context_id
        {
            (selected_context_id, selected_session_id, true)
        } else {
            let context = contexts
                .0
                .select_for_client(client_id, &PrimitiveContextSelector::ById(req.context_id))
                .map_err(|_| AttachCommandError::ContextNotFound)?;
            let Some(selected_session_id) = contexts.0.current_session_for_client(client_id) else {
                return Err(failed("context has no attached runtime"));
            };
            (context.id, selected_session_id, false)
        };

    // Validate the session exists in the manager.
    if !manager.0.contains(next_session_id) {
        return Err(AttachCommandError::SessionNotFound);
    }

    // Track previous session for client migration if switching.
    let previous_session = selected_target.and_then(|(_, session_id)| session_id);
    if let Some(prev) = previous_session
        && prev != next_session_id
    {
        manager.0.remove_client(prev, &client_id);
    }

    // Ensure client is registered with the target session.
    manager.0.add_client(next_session_id, client_id);

    if !selection_already_applied {
        follow
            .0
            .set_selected_target(client_id, Some(context_id), Some(next_session_id));
    }

    let (cols, rows, top, bottom) = retarget_attach_stream(
        &runtime,
        &follow,
        client_id,
        next_session_id,
        req.cols,
        req.rows,
        req.status_top_inset,
        req.status_bottom_inset,
        req.cell_pixel_width,
        req.cell_pixel_height,
    )?;
    let can_write = req.can_write && session_write_allowed(ctx, next_session_id, client_id)?;
    runtime
        .0
        .set_client_write_permission(next_session_id, client_id, can_write);

    Ok(AttachRetargetReady {
        session_id: next_session_id.0,
        context_id: Some(context_id),
        can_write,
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
        runtime
            .0
            .set_client_write_permission(stream_session, client_id, false);
        follow.0.set_attached_stream_session(client_id, None);
        publish_pane_event(PaneEvent::ClientDetached {
            session_id: stream_session.0,
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
