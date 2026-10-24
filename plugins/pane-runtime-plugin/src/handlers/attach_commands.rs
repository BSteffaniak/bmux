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

// Observer identity is absorbing while its authenticated connection is live,
// including after detach. Only verified disconnected UUIDs (never reused by the
// transport) may be reclaimed; missing lifecycle authority preserves the bound.
#[derive(Default)]
struct ObserverIdentities {
    clients: std::collections::BTreeMap<ClientId, (Uuid, SessionId)>,
    pending_expiry: std::collections::BTreeMap<ClientId, u64>,
}
impl ObserverIdentities {
    fn reclaim_disconnected(
        &mut self,
        connected: Option<&dyn bmux_client_state::FollowStateReader>,
    ) {
        if let Some(connected) = connected {
            self.clients
                .retain(|client, _| connected.is_connected(*client));
            self.pending_expiry
                .retain(|client, _| self.clients.contains_key(client));
        }
    }

    fn expire_pending(&mut self, now_epoch_ms: u64) {
        self.pending_expiry.retain(|client, expires| {
            if *expires > now_epoch_ms {
                return true;
            }
            if let Some((token, _)) = self.clients.get_mut(client) {
                *token = Uuid::nil();
            }
            false
        });
    }

    fn consume_grant(
        &mut self,
        client: ClientId,
        token: Uuid,
        session: SessionId,
        consume: impl FnOnce() -> Result<(), AttachTokenValidationError>,
    ) -> Result<(), AttachCommandError> {
        if token.is_nil() || self.clients.get(&client) != Some(&(token, session)) {
            return Err(AttachCommandError::InvalidGrant);
        }
        consume().map_err(|error| match error {
            AttachTokenValidationError::Expired => AttachCommandError::ExpiredGrant,
            _ => AttachCommandError::InvalidGrant,
        })?;
        self.pending_expiry.remove(&client);
        self.clients.insert(client, (Uuid::nil(), session));
        Ok(())
    }
}

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(u64::MAX)
}

static OBSERVERS: std::sync::OnceLock<std::sync::Mutex<ObserverIdentities>> =
    std::sync::OnceLock::new();

#[cfg(test)]
pub(crate) fn mark_test_observer(client: ClientId, session: SessionId) {
    OBSERVERS
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .clients
        .insert(client, (Uuid::nil(), session));
}

pub(crate) fn observer_identity(client: ClientId) -> bool {
    OBSERVERS
        .get_or_init(Default::default)
        .lock()
        .map_or(true, |state| state.clients.contains_key(&client))
}

pub(crate) fn observer_operation_allowed(
    client: ClientId,
    interface: &str,
    operation: &str,
) -> bool {
    if !observer_identity(client) {
        return true;
    }
    match interface {
        "attach-runtime-state" | "pane-runtime-state" => true,
        "attach-runtime-commands" => matches!(
            operation,
            "observer-grant" | "observer-open" | "observe-viewport" | "attach-output" | "detach"
        ),
        _ => false,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObserverGrantArgs {
    pub selector: SessionSelector,
}

pub fn observer_grant(
    req: &ObserverGrantArgs,
    ctx: &NativeServiceContext,
) -> Result<AttachGrantRecord, AttachCommandError> {
    let client = caller_client_id(ctx)?;
    let manager = session_manager()?;
    let follow = follow_state()?;
    if OBSERVERS
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| failed("observer identities unavailable"))?
        .clients
        .contains_key(&client)
    {
        return Err(failed("observer connection already granted"));
    }
    // Require a dedicated connection, not a downgrade of an interactive attach.
    if follow.0.selected_session(client).is_some()
        || follow.0.attached_stream_session(client).is_some()
    {
        return Err(failed("observer requires a dedicated connection"));
    }
    let session = resolve_session_by_selector(&*manager.0, &req.selector)
        .ok_or(AttachCommandError::SessionNotFound)?;
    let tokens = attach_token_handle()?;
    let mut observers = OBSERVERS
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| failed("observer identities unavailable"))?;
    observers.reclaim_disconnected(Some(&*follow.0));
    observers.expire_pending(epoch_ms());
    if !follow.0.is_connected(client) {
        return Err(failed("observer requires a live authenticated connection"));
    }
    if observers.clients.contains_key(&client) {
        return Err(failed("observer connection already granted"));
    }
    if observers.clients.len() >= 4096 {
        return Err(failed("observer identity limit"));
    }
    // Same host capability authorization as attach-session. Existing normal
    // attach has no separate read-policy check; do not substitute write policy.
    let grant = tokens.0.issue(session);
    observers
        .pending_expiry
        .insert(client, grant.expires_at_epoch_ms);
    observers
        .clients
        .insert(client, (grant.attach_token, session));
    Ok(to_api_grant(&grant))
}

pub fn observer_open(
    req: &AttachOpenArgs,
    ctx: &NativeServiceContext,
) -> Result<AttachReady, AttachCommandError> {
    let client = caller_client_id(ctx)?;
    let session = SessionId(req.session_id);
    let tokens = attach_token_handle()?;
    let mut observers = OBSERVERS
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| failed("observer identities unavailable"))?;
    if observers
        .pending_expiry
        .get(&client)
        .is_some_and(|expires| *expires <= epoch_ms())
    {
        observers.expire_pending(epoch_ms());
        return Err(AttachCommandError::ExpiredGrant);
    }
    observers.consume_grant(client, req.attach_token, session, || {
        tokens.0.consume(session, req.attach_token)
    })?;
    // Keep the absorbing identity fence but destroy the grant binding on use.
    drop(observers);
    crate::runtime::begin_observer_attach(session, client)
        .map_err(|error| failed(format!("observer open failed: {error:?}")))?;
    // Deliberately no selection, membership, follow, focus, or layout events.
    let runtime = super::session_runtime_handle().ok_or_else(|| failed("runtime unavailable"))?;
    runtime
        .0
        .set_client_write_permission(session, client, false);
    Ok(AttachReady {
        session_id: session.0,
        context_id: None,
        can_write: false,
    })
}

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
    pub top_inset: u16,
    pub right_inset: u16,
    pub bottom_inset: u16,
    pub left_inset: u16,
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
    pub top_inset: u16,
    pub right_inset: u16,
    pub bottom_inset: u16,
    pub left_inset: u16,
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
    if !ctx
        .enabled_plugins
        .iter()
        .any(|id| id == "bmux.permissions")
    {
        return Ok(true);
    }
    let principal_id = bmux_plugin::global_plugin_state_registry()
        .get::<bmux_client_state::ClientPrincipalHandle>()
        .and_then(|arc| arc.read().ok().map(|g| (*g).clone()))
        .and_then(|handle| handle.0.get(client_id))
        .ok_or_else(|| failed("session policy principal is unavailable"))?;

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
    top_inset: u16,
    right_inset: u16,
    bottom_inset: u16,
    left_inset: u16,
    cell_pixel_width: u16,
    cell_pixel_height: u16,
) -> Result<(u16, u16, u16, u16, u16, u16), AttachCommandError> {
    let previous_stream = follow.0.attached_stream_session(client_id);
    let previous_to_detach = previous_stream.filter(|prev| *prev != next_session_id);
    let retarget_result = match runtime.0.retarget_attach_stream(
        previous_to_detach,
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
        Ok(viewport) => Ok(viewport),
        Err(SessionRuntimeError::NotFound) => {
            let _ = runtime.0.start_runtime(next_session_id);
            runtime.0.retarget_attach_stream(
                previous_to_detach,
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
                top_inset,
                right_inset,
                bottom_inset,
                left_inset,
                cell_pixel_width,
                cell_pixel_height,
            )
        }
        Err(err) => Err(err),
    };

    match retarget_result {
        Err(error @ SessionRuntimeError::ResponseBudgetExceeded) => Err(failed(error.to_string())),
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

fn reserve_selection(
    ctx: &NativeServiceContext,
) -> Result<bmux_clients_plugin_api::clients_selection_state_v1::Selection, AttachCommandError> {
    let mut client = bmux_plugin::ServiceCallerDispatchClient::new(ctx);
    let current = bmux_plugin::block_on_typed_dispatch(
        bmux_clients_plugin_api::clients_selection_state_v1::client::current(&mut client),
    )
    .map_err(|error| failed(error.to_string()))?
    .map_err(|error| failed(format!("selection unavailable: {error:?}")))?;
    bmux_plugin::block_on_typed_dispatch(
        bmux_clients_plugin_api::clients_selection_commands_v1::client::begin(
            &mut client,
            current.revision,
        ),
    )
    .map_err(|error| failed(error.to_string()))?
    .map_err(|error| failed(format!("selection conflict: {error:?}")))
}

fn resolve_failed_selection<T>(
    ctx: &NativeServiceContext,
    revision: u64,
    result: Result<T, AttachCommandError>,
) -> Result<T, AttachCommandError> {
    match result {
        Ok(value) => Ok(value),
        Err(original) => {
            let mut client = bmux_plugin::ServiceCallerDispatchClient::new(ctx);
            let recovery = bmux_plugin::block_on_typed_dispatch(
                bmux_clients_plugin_api::clients_selection_commands_v1::client::recover(
                    &mut client,
                    revision,
                ),
            );
            match recovery {
                Ok(Ok(_)) => Err(original),
                Ok(Err(
                    bmux_clients_plugin_api::clients_selection_state_v1::SelectionError::Conflict,
                )) => Err(failed(format!(
                    "selection failed ({original:?}); recovery conflicted with newer selection"
                ))),
                other => Err(failed(format!(
                    "selection failed ({original:?}); recovery incomplete, input may remain suspended: {other:?}"
                ))),
            }
        }
    }
}

fn commit_selection(
    ctx: &NativeServiceContext,
    revision: u64,
    context: Option<Uuid>,
    session: SessionId,
) -> Result<(), AttachCommandError> {
    let mut client = bmux_plugin::ServiceCallerDispatchClient::new(ctx);
    let result = bmux_plugin::block_on_typed_dispatch(
        bmux_clients_plugin_api::clients_selection_commands_v1::client::commit(
            &mut client,
            revision,
            context,
            Some(session.0),
        ),
    )
    .map_err(|error| failed(error.to_string()))
    .and_then(|result| {
        result.map_err(|error| failed(format!("selection commit failed: {error:?}")))
    })
    .map(|_| ());
    resolve_failed_selection(ctx, revision, result)
}

pub fn attach_session(
    req: &AttachSessionArgs,
    ctx: &NativeServiceContext,
) -> Result<AttachGrantRecord, AttachCommandError> {
    let client_id = caller_client_id(ctx)?;
    if observer_identity(client_id) {
        return Err(failed("observer connection is read-only"));
    }
    let manager = session_manager()?;
    let follow = follow_state()?;

    let Some(next_session_id) = resolve_session_by_selector(&*manager.0, &req.selector) else {
        return Err(AttachCommandError::SessionNotFound);
    };

    // Resolve every fallible dependency before changing membership or selection.
    // Missing runtime state is a recovery condition, not permission to delete
    // durable context mappings.
    let contexts = context_state()?;
    let tokens = attach_token_handle()?;
    if !manager.0.contains(next_session_id) {
        return Err(AttachCommandError::SessionNotFound);
    }
    let selected_context = contexts.0.context_for_session(next_session_id);
    let previous_session = follow.0.selected_session(client_id);
    let reservation = reserve_selection(ctx)?;
    manager.0.add_client(next_session_id, client_id);
    if let Some(previous) = previous_session.filter(|id| *id != next_session_id) {
        manager.0.remove_client(previous, &client_id);
    }
    commit_selection(ctx, reservation.revision, selected_context, next_session_id)?;

    // Issue the grant with context decoration.
    let mut grant = tokens.0.issue(next_session_id);
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

    let context_id = contexts
        .0
        .resolve_id(&context_selector_to_ipc(&req.selector)?)
        .map_err(|_| AttachCommandError::ContextNotFound)?;
    let snapshot = contexts.0.snapshot();
    let Some(next_session_id) = snapshot.session_by_context.get(&context_id).copied() else {
        return Err(failed("context has no attached runtime"));
    };
    let tokens = attach_token_handle()?;
    if !manager.0.contains(next_session_id) {
        return Err(AttachCommandError::SessionNotFound);
    }
    let previous_session = follow.0.selected_session(client_id);
    let reservation = reserve_selection(ctx)?;
    let context = resolve_failed_selection(
        ctx,
        reservation.revision,
        contexts
            .0
            .select_for_client(client_id, &PrimitiveContextSelector::ById(context_id))
            .map_err(|message| failed(message.to_string())),
    )?;
    if let Some(prev) = previous_session
        && prev != next_session_id
    {
        manager.0.remove_client(prev, &client_id);
    }

    manager.0.add_client(next_session_id, client_id);
    commit_selection(ctx, reservation.revision, Some(context.id), next_session_id)?;

    let mut grant = tokens.0.issue(next_session_id);
    grant.context_id = Some(context.id);
    Ok(to_api_grant(&grant))
}

pub fn attach_open(
    req: &AttachOpenArgs,
    ctx: &NativeServiceContext,
) -> Result<AttachReady, AttachCommandError> {
    let client_id = caller_client_id(ctx)?;
    if observer_identity(client_id) {
        return Err(failed("observer connection is read-only"));
    }
    let session_id = SessionId(req.session_id);
    let runtime = super::session_runtime_handle()
        .ok_or_else(|| failed("pane-runtime manager handle not registered"))?;
    let tokens = attach_token_handle()?;
    if OBSERVERS
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| failed("observer identities unavailable"))?
        .clients
        .values()
        .any(|(token, _)| *token == req.attach_token)
    {
        return Err(AttachCommandError::InvalidGrant);
    }
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
        Err(error @ SessionRuntimeError::ResponseBudgetExceeded) => Err(failed(error.to_string())),
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

fn validate_input_selection(
    selected: Option<SessionId>,
    requested: SessionId,
) -> Result<(), AttachCommandError> {
    if selected == Some(requested) {
        return Ok(());
    }
    Err(AttachCommandError::Denied {
        reason: "input target no longer matches the caller selection".to_string(),
    })
}

/// Public, secret-free capture suppression request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuppressPaneInputCaptureArgs {
    pub session_id: Uuid,
    pub pane_id: Uuid,
}

/// Acknowledge recording suppression under authenticated attach write ownership.
pub fn suppress_pane_input_capture(
    req: &SuppressPaneInputCaptureArgs,
    ctx: &NativeServiceContext,
) -> Result<bool, AttachCommandError> {
    let client_id = caller_client_id(ctx)?;
    let session_id = SessionId(req.session_id);
    let runtime = super::session_runtime_handle()
        .ok_or_else(|| failed("pane-runtime manager handle not registered"))?;
    validate_input_selection(follow_state()?.0.selected_session(client_id), session_id)?;
    if !runtime.0.client_can_write(session_id, client_id) {
        return Err(AttachCommandError::Denied {
            reason: "capture suppression requires attach write ownership".into(),
        });
    }
    let panes = runtime
        .0
        .list_panes(session_id)
        .map_err(|_| AttachCommandError::SessionNotFound)?;
    if !panes.iter().any(|pane| pane.id == req.pane_id) {
        return Err(AttachCommandError::SessionNotFound);
    }
    if !crate::private_input::suppress(req.pane_id) {
        return Err(failed("capture suppression barrier unavailable"));
    }
    Ok(true)
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
    validate_input_selection(follow_state()?.0.selected_session(client_id), session_id)?;
    if !runtime.0.client_can_write(session_id, client_id) {
        return Err(AttachCommandError::Denied {
            reason: "client does not have write permission for this attach stream".to_string(),
        });
    }
    let data_len = req.data.len();
    match runtime.0.write_input(session_id, client_id, req.data) {
        Err(error @ SessionRuntimeError::ResponseBudgetExceeded) => Err(failed(error.to_string())),
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
        Err(error @ SessionRuntimeError::ResponseBudgetExceeded) => Err(failed(error.to_string())),
        Err(SessionRuntimeError::NotFound | SessionRuntimeError::Closed) => {
            Err(AttachCommandError::SessionNotFound)
        }
        Err(SessionRuntimeError::NotAttached) => {
            Err(failed("client is not attached to session runtime"))
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObserveViewportArgs {
    pub session_id: Uuid,
    pub cols: u16,
    pub rows: u16,
}

pub fn observe_viewport(
    req: &ObserveViewportArgs,
    ctx: &NativeServiceContext,
) -> Result<AttachViewportSet, AttachCommandError> {
    let client_id = caller_client_id(ctx)?;
    let viewport =
        crate::runtime::observe_viewport(SessionId(req.session_id), client_id, req.cols, req.rows)
            .map_err(|error| failed(format!("viewport observation failed: {error:?}")))?;
    Ok(AttachViewportSet {
        session_id: req.session_id,
        cols: viewport.cols,
        rows: viewport.rows,
        top_inset: viewport.top_inset,
        right_inset: viewport.right_inset,
        bottom_inset: viewport.bottom_inset,
        left_inset: viewport.left_inset,
        context_id: context_state()?
            .0
            .current_for_client(client_id)
            .map(|context| context.id),
    })
}

pub fn attach_set_viewport(
    req: &AttachSetViewportArgs,
    ctx: &NativeServiceContext,
) -> Result<AttachViewportSet, AttachCommandError> {
    let client_id = caller_client_id(ctx)?;
    let session_id = SessionId(req.session_id);
    let runtime = super::session_runtime_handle()
        .ok_or_else(|| failed("pane-runtime manager handle not registered"))?;
    let (cols, rows, top, right, bottom, left) = runtime
        .0
        .set_attach_viewport(
            session_id,
            client_id,
            req.cols,
            req.rows,
            req.top_inset,
            req.right_inset,
            req.bottom_inset,
            req.left_inset,
            req.cell_pixel_width,
            req.cell_pixel_height,
        )
        .map_err(|e| failed(format!("failed setting attach viewport: {e:?}")))?;
    let context_id = context_state()?
        .0
        .current_for_client(client_id)
        .map(|c| c.id);
    crate::runtime::emit_attach_view_changed_for_layout(session_id);
    Ok(AttachViewportSet {
        session_id: req.session_id,
        cols,
        rows,
        top_inset: top,
        right_inset: right,
        bottom_inset: bottom,
        left_inset: left,
        context_id,
    })
}

fn restore_context_selection(
    contexts: &bmux_context_state::ContextStateHandle,
    client_id: ClientId,
    previous_context_id: Option<Uuid>,
) {
    if let Some(previous_context_id) = previous_context_id {
        let _ = contexts.0.select_for_client(
            client_id,
            &PrimitiveContextSelector::ById(previous_context_id),
        );
    } else {
        contexts.0.disconnect_client(client_id);
    }
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
    let previous_context_id = contexts
        .0
        .snapshot()
        .selected_by_client
        .get(&client_id)
        .copied();
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
                restore_context_selection(&contexts, client_id, previous_context_id);
                return Err(failed("context has no attached runtime"));
            };
            (context.id, selected_session_id, false)
        };

    // Validate the session exists in the manager.
    if !manager.0.contains(next_session_id) {
        if !selection_already_applied {
            restore_context_selection(&contexts, client_id, previous_context_id);
        }
        return Err(AttachCommandError::SessionNotFound);
    }

    let previous_session = selected_target.and_then(|(_, session_id)| session_id);
    let can_write = match session_write_allowed(ctx, next_session_id, client_id) {
        Ok(allowed) => req.can_write && allowed,
        Err(error) => {
            if !selection_already_applied {
                restore_context_selection(&contexts, client_id, previous_context_id);
            }
            return Err(error);
        }
    };

    let reservation = reserve_selection(ctx)?;
    let retargeted = match retarget_attach_stream(
        &runtime,
        &follow,
        client_id,
        next_session_id,
        req.cols,
        req.rows,
        req.top_inset,
        req.right_inset,
        req.bottom_inset,
        req.left_inset,
        req.cell_pixel_width,
        req.cell_pixel_height,
    ) {
        Ok(viewport) => viewport,
        Err(error) => {
            if !selection_already_applied {
                restore_context_selection(&contexts, client_id, previous_context_id);
            }
            return resolve_failed_selection(ctx, reservation.revision, Err(error));
        }
    };
    let (cols, rows, top, right, bottom, left) = retargeted;
    if let Some(prev) = previous_session
        && prev != next_session_id
    {
        manager.0.remove_client(prev, &client_id);
    }
    manager.0.add_client(next_session_id, client_id);
    commit_selection(ctx, reservation.revision, Some(context_id), next_session_id)?;
    runtime
        .0
        .set_client_write_permission(next_session_id, client_id, can_write);
    crate::runtime::emit_attach_view_changed_for_layout(next_session_id);

    Ok(AttachRetargetReady {
        session_id: next_session_id.0,
        context_id: Some(context_id),
        can_write,
        cols,
        rows,
        top_inset: top,
        right_inset: right,
        bottom_inset: bottom,
        left_inset: left,
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
    if observer_identity(client_id) {
        let session = OBSERVERS
            .get_or_init(Default::default)
            .lock()
            .map_err(|_| failed("observer identities unavailable"))?
            .clients
            .get(&client_id)
            .map(|(_, session)| *session);
        if let Some(session) = session {
            let runtime =
                super::session_runtime_handle().ok_or_else(|| failed("runtime unavailable"))?;
            runtime.0.end_attach(session, client_id);
        }
        return Ok(0);
    }
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

#[cfg(test)]
mod selection_tests {
    use super::*;
    struct ConnectionFixture(ClientId);
    impl bmux_client_state::FollowStateReader for ConnectionFixture {
        fn selected_session(&self, _: ClientId) -> Option<SessionId> {
            None
        }
        fn selected_context(&self, _: ClientId) -> Option<Uuid> {
            None
        }
        fn follow_target(&self, _: ClientId) -> Option<bmux_client_state::FollowEntry> {
            None
        }
        fn list_clients(&self) -> Vec<bmux_client_state::ClientSummary> {
            vec![]
        }
        fn selected_target(&self, _: ClientId) -> Option<(Option<Uuid>, Option<SessionId>)> {
            None
        }
        fn is_connected(&self, client: ClientId) -> bool {
            client == self.0
        }
        fn attached_stream_session(&self, _: ClientId) -> Option<SessionId> {
            None
        }
        fn attach_detach_allowed(&self, _: ClientId) -> bool {
            true
        }
    }

    #[test]
    fn observer_cleanup_requires_disconnect_not_detach_and_missing_provider_retains_fence() {
        let live_detached = ClientId(Uuid::new_v4());
        let retired = ClientId(Uuid::new_v4());
        let session = SessionId(Uuid::new_v4());
        let mut identities = ObserverIdentities::default();
        identities
            .clients
            .insert(live_detached, (Uuid::nil(), session));
        identities.clients.insert(retired, (Uuid::nil(), session));
        identities.reclaim_disconnected(None);
        assert_eq!(identities.clients.len(), 2);
        let follow = ConnectionFixture(live_detached);
        assert!(
            bmux_client_state::FollowStateReader::attached_stream_session(&follow, live_detached)
                .is_none()
        );
        identities.reclaim_disconnected(Some(&follow));
        assert!(identities.clients.contains_key(&live_detached));
        assert!(!identities.clients.contains_key(&retired));
    }

    #[test]
    fn observer_teardown_reclaims_capacity_and_expiry_only_removes_pending_grant() {
        let live = ClientId(Uuid::new_v4());
        let session = SessionId(Uuid::new_v4());
        let mut identities = ObserverIdentities::default();
        let live_token = Uuid::new_v4();
        identities.clients.insert(live, (live_token, session));
        identities.pending_expiry.insert(live, 100);
        let follow = ConnectionFixture(live);
        // More than the admission cap over time must not exhaust the registry.
        for _ in 0..5000 {
            let retired = ClientId(Uuid::new_v4());
            identities
                .clients
                .insert(retired, (Uuid::new_v4(), session));
            identities.pending_expiry.insert(retired, 100);
            identities.reclaim_disconnected(Some(&follow));
            assert_eq!(identities.clients.len(), 1);
            assert_eq!(identities.pending_expiry.len(), 1);
        }
        identities.expire_pending(99);
        assert_eq!(identities.clients[&live].0, live_token);
        identities.expire_pending(100);
        assert!(identities.pending_expiry.is_empty());
        assert!(identities.clients.contains_key(&live));
        assert!(identities.clients[&live].0.is_nil());
        assert!(matches!(
            identities.consume_grant(live, live_token, session, || panic!("expired grant")),
            Err(AttachCommandError::InvalidGrant)
        ));
        // Missing authority must not clear even an expired connection fence.
        identities.reclaim_disconnected(None);
        assert!(identities.clients.contains_key(&live));
    }

    #[test]
    fn observer_grant_preserves_owner_session_expiry_and_single_use() {
        let owner = ClientId(Uuid::new_v4());
        let other = ClientId(Uuid::new_v4());
        let session = SessionId(Uuid::new_v4());
        let token = Uuid::new_v4();
        let mut identities = ObserverIdentities::default();
        identities.clients.insert(owner, (token, session));
        for (client, request_token, request_session) in [
            (other, token, session),
            (owner, Uuid::new_v4(), session),
            (owner, token, SessionId(Uuid::new_v4())),
        ] {
            assert!(matches!(
                identities.consume_grant(client, request_token, request_session, || panic!(
                    "invalid owner must not consume grant"
                )),
                Err(AttachCommandError::InvalidGrant)
            ));
        }
        assert!(matches!(
            identities.consume_grant(owner, token, session, || Err(
                AttachTokenValidationError::Expired
            )),
            Err(AttachCommandError::ExpiredGrant)
        ));
        assert!(matches!(
            identities.consume_grant(owner, token, session, || Err(
                AttachTokenValidationError::SessionMismatch
            )),
            Err(AttachCommandError::InvalidGrant)
        ));
        identities
            .consume_grant(owner, token, session, || Ok(()))
            .unwrap();
        assert!(identities.clients.contains_key(&owner));
        assert!(matches!(
            identities.consume_grant(owner, token, session, || panic!("replayed grant")),
            Err(AttachCommandError::InvalidGrant)
        ));
        assert!(matches!(
            identities.consume_grant(owner, Uuid::nil(), session, || panic!("nil grant")),
            Err(AttachCommandError::InvalidGrant)
        ));
    }

    #[test]
    fn observer_route_fence_is_absorbing_and_owner_specific() {
        let observer = ClientId(Uuid::new_v4());
        let ordinary = ClientId(Uuid::new_v4());
        mark_test_observer(observer, SessionId(Uuid::new_v4()));
        for (interface, operation) in [
            ("attach-runtime-commands", "attach-input"),
            ("attach-runtime-commands", "attach-set-viewport"),
            ("attach-runtime-commands", "attach-open"),
            ("attach-runtime-commands", "attach-session"),
            ("attach-runtime-commands", "attach-context"),
            ("attach-runtime-commands", "attach-retarget-context"),
            ("pane-runtime-commands", "focus-pane"),
            ("pane-runtime-commands", "pane-set-pty-size"),
        ] {
            assert!(!observer_operation_allowed(observer, interface, operation));
            assert!(observer_operation_allowed(ordinary, interface, operation));
        }
        assert!(observer_operation_allowed(
            observer,
            "attach-runtime-state",
            "attach-snapshot"
        ));
        assert!(observer_operation_allowed(
            observer,
            "attach-runtime-commands",
            "observe-viewport"
        ));
        assert!(observer_operation_allowed(
            observer,
            "attach-runtime-commands",
            "detach"
        ));
        // Detach/grant consumption must not erase the connection's fence.
        assert!(observer_identity(observer));
    }

    #[test]
    fn absent_permissions_is_local_fallback_but_enabled_missing_principal_fails_closed() {
        use bmux_plugin_sdk::{
            ApiVersion, HostConnectionInfo, HostMetadata, HostScope, ProviderId, RegisteredService,
            ServiceKind, ServiceRequest,
        };
        let mut context = NativeServiceContext {
            plugin_id: "bmux.pane_runtime".into(),
            request: ServiceRequest {
                caller_plugin_id: String::new(),
                service: RegisteredService {
                    capability: HostScope::new("bmux.pane_runtime.write").unwrap(),
                    kind: ServiceKind::Command,
                    interface_id: "attach-runtime-commands".into(),
                    provider: ProviderId::Plugin("bmux.pane_runtime".into()),
                },
                operation: "attach-open".into(),
                payload: vec![],
            },
            required_capabilities: vec![],
            provided_capabilities: vec![],
            services: vec![],
            available_capabilities: vec![],
            enabled_plugins: vec![],
            plugin_search_roots: vec![],
            host: HostMetadata {
                product_name: "bmux".into(),
                product_version: "test".into(),
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
        };
        let session = SessionId(Uuid::new_v4());
        let client = ClientId(Uuid::new_v4());
        assert!(session_write_allowed(&context, session, client).unwrap());
        context.enabled_plugins.push("bmux.permissions".into());
        assert!(session_write_allowed(&context, session, client).is_err());
    }

    #[test]
    fn stale_and_suspended_input_targets_are_rejected() {
        let old = SessionId(Uuid::from_u128(1));
        let current = SessionId(Uuid::from_u128(2));
        assert!(validate_input_selection(Some(current), current).is_ok());
        assert!(matches!(
            validate_input_selection(Some(current), old),
            Err(AttachCommandError::Denied { .. })
        ));
        assert!(matches!(
            validate_input_selection(None, old),
            Err(AttachCommandError::Denied { .. })
        ));
    }
}
