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
use bmux_pane_runtime_plugin_api::pane_runtime_events::PaneEvent;
use bmux_pane_runtime_state::{RuntimeStartPending, SessionRuntimeError};
use bmux_plugin_sdk::{NativeServiceContext, ServiceResponse};
use bmux_session_models::{ClientId, SessionId};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// Observer identity is absorbing while its authenticated connection is live,
// including after detach. Only verified disconnected UUIDs (never reused by the
// transport) may be reclaimed; missing lifecycle authority preserves the bound.
#[derive(Default)]
pub(crate) struct ObserverIdentities {
    clients: std::collections::BTreeMap<ClientId, (Uuid, SessionId)>,
    pending_expiry: std::collections::BTreeMap<ClientId, u64>,
}
impl ObserverIdentities {
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }

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
    switchy_time::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
pub(crate) fn mark_test_observer(
    host: &crate::runtime::PaneRuntimeHost,
    client: ClientId,
    session: SessionId,
) {
    host.observers()
        .lock()
        .unwrap()
        .clients
        .insert(client, (Uuid::nil(), session));
}

pub(crate) fn observer_identity(host: &crate::runtime::PaneRuntimeHost, client: ClientId) -> bool {
    host.observers()
        .lock()
        .map_or(true, |state| state.clients.contains_key(&client))
}

pub(crate) fn observer_operation_allowed(
    host: &crate::runtime::PaneRuntimeHost,
    client: ClientId,
    interface: &str,
    operation: &str,
) -> bool {
    if !observer_identity(host, client) {
        return true;
    }
    match interface {
        "attach-runtime-state/v2" | "pane-runtime-state" => true,
        "pane-runtime-pointer-grid-v2" => matches!(
            operation,
            "capabilities" | "snapshots" | "snapshot" | "deltas" | "windows"
        ),
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
    host: &crate::runtime::PaneRuntimeHost,
    req: &ObserverGrantArgs,
    ctx: &NativeServiceContext,
) -> Result<AttachGrantRecord, AttachCommandError> {
    let client = caller_client_id(ctx)?;
    let manager = session_manager(host)?;
    let follow = follow_state(host)?;
    if host
        .observers()
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
    let tokens = attach_token_handle(host)?;
    let mut observers = host
        .observers()
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
    host: &crate::runtime::PaneRuntimeHost,
    req: &AttachOpenArgs,
    ctx: &NativeServiceContext,
) -> Result<AttachReady, AttachCommandError> {
    let client = caller_client_id(ctx)?;
    let session = SessionId(req.session_id);
    let tokens = attach_token_handle(host)?;
    let mut observers = host
        .observers()
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
    crate::runtime::begin_observer_attach(host, session, client)
        .map_err(|error| failed(format!("observer open failed: {error:?}")))?;
    // Deliberately no selection, membership, follow, focus, or layout events.
    let runtime = host
        .registered_session_runtime()
        .ok_or_else(|| failed("runtime unavailable"))?;
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

/// Why an attach handler that may start a session runtime stopped.
#[derive(Debug)]
pub enum AttachStop {
    /// A domain answer for the caller.
    Rejected(AttachCommandError),
    /// The caller's deadline passed while the session's runtime was starting.
    /// The start goes on, so a later attach (the client's resync) finds it.
    StillStarting(RuntimeStartPending),
}

impl From<AttachCommandError> for AttachStop {
    fn from(error: AttachCommandError) -> Self {
        Self::Rejected(error)
    }
}

/// A typed handler's answer: a deadline stop is a service-level timeout,
/// never a domain rejection, so the client retries instead of giving up.
pub type AttachOutcome<T> = Result<Result<T, AttachCommandError>, ServiceResponse>;

fn answer<T>(result: Result<T, AttachStop>) -> AttachOutcome<T> {
    match result {
        Ok(value) => Ok(Ok(value)),
        Err(AttachStop::Rejected(error)) => Ok(Err(error)),
        Err(AttachStop::StillStarting(pending)) => {
            Err(ServiceResponse::deadline_exceeded(pending.to_string()))
        }
    }
}

/// When the caller of a handler stops waiting for its reply, read when the
/// handler starts (the host passes the time left).
#[derive(Debug, Clone, Copy)]
struct CallerDeadline(Option<switchy_time::Instant>);

impl CallerDeadline {
    fn of(ctx: &NativeServiceContext) -> Self {
        let now = switchy_time::Instant::now();
        Self(
            ctx.cancellation
                .deadline()
                .and_then(|budget| now.checked_add(budget)),
        )
    }

    fn remaining(self) -> Option<std::time::Duration> {
        self.0
            .map(|deadline| deadline.saturating_duration_since(switchy_time::Instant::now()))
    }
}

/// Start `session_id`'s runtime, waiting no longer than the caller does.
/// Only a start still running then stops the handler; any other failure is
/// left to the attach that follows (it reports the session missing).
fn start_runtime_by(
    runtime: &bmux_pane_runtime_state::SessionRuntimeManagerHandle,
    session_id: SessionId,
    deadline: CallerDeadline,
) -> Result<anyhow::Result<()>, AttachStop> {
    match runtime.0.start_runtime_within(session_id, deadline.remaining()) {
        Err(error) => match error.downcast::<RuntimeStartPending>() {
            Ok(pending) => Err(AttachStop::StillStarting(pending)),
            Err(error) => Ok(Err(error)),
        },
        Ok(()) => Ok(Ok(())),
    }
}

fn caller_client_id(ctx: &NativeServiceContext) -> Result<ClientId, AttachCommandError> {
    ctx.caller_client_id
        .map(ClientId)
        .ok_or_else(|| failed("attach operation requires a caller client id"))
}

fn session_write_allowed(
    host: &crate::runtime::PaneRuntimeHost,
    ctx: &NativeServiceContext,
    session_id: SessionId,
    client_id: ClientId,
) -> Result<bool, AttachCommandError> {
    session_action_allowed(host, ctx, session_id, client_id, "pane.direct_input")
}

/// The session policy's answer for `action` by this caller, or `true` when no
/// permissions plugin is enabled (single-user behavior).
fn session_action_allowed(
    host: &crate::runtime::PaneRuntimeHost,
    ctx: &NativeServiceContext,
    session_id: SessionId,
    client_id: ClientId,
    action: &str,
) -> Result<bool, AttachCommandError> {
    if !ctx
        .enabled_plugins
        .iter()
        .any(|id| id == "bmux.permissions")
    {
        return Ok(true);
    }
    let principal_id = host
        .states()
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
            action.to_string(),
            None,
            None,
            None,
        ),
    ) {
        Ok(response) => Ok(response.allowed),
        Err(err) => Err(failed(format!("session policy check failed: {err}"))),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "handler forwards the wire retarget request's viewport dimensions to the runtime atomically"
)]
fn retarget_attach_stream(
    host: &crate::runtime::PaneRuntimeHost,
    runtime: &bmux_pane_runtime_state::SessionRuntimeManagerHandle,
    follow: &bmux_client_state::FollowStateHandle,
    deadline: CallerDeadline,
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
) -> Result<(u16, u16, u16, u16, u16, u16), AttachStop> {
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
            let _ = start_runtime_by(runtime, next_session_id, deadline)?;
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
            let _ = start_runtime_by(runtime, next_session_id, deadline)?;
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
        Err(error @ SessionRuntimeError::ResponseBudgetExceeded) => {
            Err(failed(error.to_string()).into())
        }
        Ok(viewport) => {
            if let Some(prev) = previous_to_detach {
                runtime
                    .0
                    .set_client_write_permission(prev, client_id, false);
                host.publish_pane_event(PaneEvent::ClientDetached { session_id: prev.0 });
            }
            follow
                .0
                .set_attached_stream_session(client_id, Some(next_session_id));
            host.publish_pane_event(PaneEvent::ClientAttached {
                session_id: next_session_id.0,
            });
            // Retargeting changes which session's focused pane is relevant to
            // attach-local consumers (decoration, status bars). Republish the
            // full focus snapshot even when the pane focus itself did not
            // mutate so subscribers can reconcile after tab/window switches.
            super::publish_focus_state_snapshot(host);
            Ok(viewport)
        }
        Err(SessionRuntimeError::NotFound | SessionRuntimeError::Closed) => {
            Err(AttachCommandError::SessionNotFound.into())
        }
        Err(SessionRuntimeError::NotAttached) => Err(failed("failed opening attach stream").into()),
    }
}

fn session_manager(
    host: &crate::runtime::PaneRuntimeHost,
) -> Result<bmux_session_state::SessionManagerHandle, AttachCommandError> {
    host.states()
        .get::<bmux_session_state::SessionManagerHandle>()
        .and_then(|arc| arc.read().ok().map(|g| (*g).clone()))
        .ok_or_else(|| failed("session manager handle not registered"))
}

fn context_state(
    host: &crate::runtime::PaneRuntimeHost,
) -> Result<bmux_context_state::ContextStateHandle, AttachCommandError> {
    host.states()
        .get::<bmux_context_state::ContextStateHandle>()
        .and_then(|arc| arc.read().ok().map(|g| (*g).clone()))
        .ok_or_else(|| failed("context state handle not registered"))
}

fn follow_state(
    host: &crate::runtime::PaneRuntimeHost,
) -> Result<bmux_client_state::FollowStateHandle, AttachCommandError> {
    host.states()
        .get::<bmux_client_state::FollowStateHandle>()
        .and_then(|arc| arc.read().ok().map(|g| (*g).clone()))
        .ok_or_else(|| failed("follow state handle not registered"))
}

fn attach_token_handle(
    host: &crate::runtime::PaneRuntimeHost,
) -> Result<bmux_attach_token_state::AttachTokenManagerHandle, AttachCommandError> {
    host.states()
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

fn reserve_selection<C: bmux_plugin::ServiceCaller + Sync + ?Sized>(
    caller: &C,
) -> Result<bmux_clients_plugin_api::clients_selection_state_v1::Selection, AttachCommandError> {
    let mut client = bmux_plugin::ServiceCallerDispatchClient::new(caller);
    let current = bmux_plugin::block_on_typed_dispatch(
        bmux_clients_plugin_api::clients_selection_state_v1::client::current(&mut client),
    )
    .map_err(|error| failed(error.to_string()))?
    .map_err(|error| failed(format!("selection unavailable: {error:?}")))?;
    let begun = bmux_plugin::block_on_typed_dispatch(
        bmux_clients_plugin_api::clients_selection_commands_v1::client::begin(
            &mut client,
            current.revision,
        ),
    );
    match begun {
        Ok(reserved) => reserved.map_err(|error| failed(format!("selection conflict: {error:?}"))),
        Err(error) => {
            // The begin may have applied without its reply reaching us (the host gave up
            // waiting). A reservation nobody owns would suspend this client's input and
            // refuse every later selection until it disconnects, so release it: recovery
            // only applies to a suspended selection at exactly the revision our begin made.
            if let Some(reserved_revision) = current.revision.checked_add(1) {
                let _ = bmux_plugin::block_on_typed_dispatch(
                    bmux_clients_plugin_api::clients_selection_commands_v1::client::recover(
                        &mut client,
                        reserved_revision,
                    ),
                );
            }
            Err(failed(error.to_string()))
        }
    }
}

fn resolve_failed_selection<T>(
    ctx: &NativeServiceContext,
    revision: u64,
    result: Result<T, AttachCommandError>,
) -> Result<T, AttachCommandError> {
    match result {
        Ok(value) => Ok(value),
        Err(original) => {
            release_selection(ctx, revision, &original)?;
            Err(original)
        }
    }
}

/// Release the reservation at `revision` after `original` stopped the
/// selection; an error says the release did not complete.
fn release_selection(
    ctx: &NativeServiceContext,
    revision: u64,
    original: &dyn std::fmt::Debug,
) -> Result<(), AttachCommandError> {
    let mut client = bmux_plugin::ServiceCallerDispatchClient::new(ctx);
    let recovery = bmux_plugin::block_on_typed_dispatch(
        bmux_clients_plugin_api::clients_selection_commands_v1::client::recover(
            &mut client,
            revision,
        ),
    );
    match recovery {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(bmux_clients_plugin_api::clients_selection_state_v1::SelectionError::Conflict)) => {
            Err(failed(format!(
                "selection failed ({original:?}); recovery conflicted with newer selection"
            )))
        }
        other => Err(failed(format!(
            "selection failed ({original:?}); recovery incomplete, input may remain suspended: {other:?}"
        ))),
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
    host: &crate::runtime::PaneRuntimeHost,
    req: &AttachSessionArgs,
    ctx: &NativeServiceContext,
) -> Result<AttachGrantRecord, AttachCommandError> {
    let client_id = caller_client_id(ctx)?;
    if observer_identity(host, client_id) {
        return Err(failed("observer connection is read-only"));
    }
    let manager = session_manager(host)?;
    let follow = follow_state(host)?;

    let Some(next_session_id) = resolve_session_by_selector(&*manager.0, &req.selector) else {
        return Err(AttachCommandError::SessionNotFound);
    };

    // Resolve every fallible dependency before changing membership or selection.
    // Missing runtime state is a recovery condition, not permission to delete
    // durable context mappings.
    let contexts = context_state(host)?;
    let tokens = attach_token_handle(host)?;
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
    host: &crate::runtime::PaneRuntimeHost,
    req: &AttachContextArgs,
    ctx: &NativeServiceContext,
) -> Result<AttachGrantRecord, AttachCommandError> {
    let client_id = caller_client_id(ctx)?;
    let manager = session_manager(host)?;
    let contexts = context_state(host)?;
    let follow = follow_state(host)?;

    let context_id = contexts
        .0
        .resolve_id(&context_selector_to_ipc(&req.selector)?)
        .map_err(|_| AttachCommandError::ContextNotFound)?;
    let snapshot = contexts.0.snapshot();
    let Some(next_session_id) = snapshot.session_by_context.get(&context_id).copied() else {
        return Err(failed("context has no attached runtime"));
    };
    let tokens = attach_token_handle(host)?;
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
    host: &crate::runtime::PaneRuntimeHost,
    req: &AttachOpenArgs,
    ctx: &NativeServiceContext,
) -> AttachOutcome<AttachReady> {
    answer(open_attach(host, req, ctx))
}

fn open_attach(
    host: &crate::runtime::PaneRuntimeHost,
    req: &AttachOpenArgs,
    ctx: &NativeServiceContext,
) -> Result<AttachReady, AttachStop> {
    let deadline = CallerDeadline::of(ctx);
    let client_id = caller_client_id(ctx)?;
    if observer_identity(host, client_id) {
        return Err(failed("observer connection is read-only").into());
    }
    let session_id = SessionId(req.session_id);
    let runtime = host
        .registered_session_runtime()
        .ok_or_else(|| failed("pane-runtime manager handle not registered"))?;
    let tokens = attach_token_handle(host)?;
    if host
        .observers()
        .lock()
        .map_err(|_| failed("observer identities unavailable"))?
        .clients
        .values()
        .any(|(token, _)| *token == req.attach_token)
    {
        return Err(AttachCommandError::InvalidGrant.into());
    }
    let follow = follow_state(host)?;

    // Ensure the session runtime exists (start it if the session
    // manager has the entry but the runtime is missing).
    let manager = session_manager(host)?;
    if !manager.0.contains(session_id) {
        return Err(AttachCommandError::SessionNotFound.into());
    }
    if !runtime.0.session_exists(session_id)
        && let Err(err) = start_runtime_by(&runtime, session_id, deadline)?
    {
        return Err(failed(format!(
            "failed restarting missing session runtime {}: {err:#}",
            session_id.0
        ))
        .into());
    }

    match tokens.0.consume(session_id, req.attach_token) {
        Ok(()) => {}
        Err(AttachTokenValidationError::NotFound | AttachTokenValidationError::SessionMismatch) => {
            return Err(AttachCommandError::InvalidGrant.into());
        }
        Err(AttachTokenValidationError::Expired) => {
            return Err(AttachCommandError::ExpiredGrant.into());
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
        host.publish_pane_event(PaneEvent::ClientDetached { session_id: prev.0 });
    }

    // Begin attach with restart-on-NotFound/Closed semantics.
    let begin_result = match runtime.0.begin_attach(session_id, client_id) {
        Ok(()) => Ok(()),
        Err(SessionRuntimeError::NotFound) => {
            let _ = start_runtime_by(&runtime, session_id, deadline)?;
            runtime.0.begin_attach(session_id, client_id)
        }
        Err(SessionRuntimeError::Closed) => {
            if let Some(removed) = runtime.0.remove_runtime(session_id) {
                runtime.0.shutdown_removed_runtime(removed);
            }
            let _ = start_runtime_by(&runtime, session_id, deadline)?;
            runtime.0.begin_attach(session_id, client_id)
        }
        Err(err) => Err(err),
    };

    match begin_result {
        Err(error @ SessionRuntimeError::ResponseBudgetExceeded) => {
            Err(failed(error.to_string()).into())
        }
        Ok(()) => {
            let can_write = session_write_allowed(host, ctx, session_id, client_id)?;
            runtime
                .0
                .set_client_write_permission(session_id, client_id, can_write);
            follow
                .0
                .set_attached_stream_session(client_id, Some(session_id));
            let context_id = context_state(host)?
                .0
                .current_for_client(client_id)
                .map(|c| c.id);
            host.publish_pane_event(PaneEvent::ClientAttached {
                session_id: session_id.0,
            });
            // Publish focus state so the newly-attached client's
            // consumers (decoration, future status plugins) observe
            // the current focused pane without an extra round-trip.
            super::publish_focus_state_snapshot(host);
            Ok(AttachReady {
                session_id: session_id.0,
                context_id,
                can_write,
            })
        }
        Err(SessionRuntimeError::NotFound | SessionRuntimeError::Closed) => {
            Err(AttachCommandError::SessionNotFound.into())
        }
        Err(SessionRuntimeError::NotAttached) => Err(failed("failed opening attach stream").into()),
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

pub fn input_producer_barriers(
    host: &crate::runtime::PaneRuntimeHost,
    ctx: &NativeServiceContext,
) -> Result<
    Vec<bmux_pane_runtime_plugin_api::attach_runtime_commands::InputProducerBarrierV2>,
    AttachCommandError,
> {
    let client = caller_client_id(ctx)?;
    Ok(crate::private_input::pending_barriers(host, client.0))
}

#[derive(Deserialize)]
pub struct AcknowledgeInputProducerBarrierArgs {
    pub pane_id: Uuid,
    pub nonce: Uuid,
    pub generation: u64,
    pub supported: bool,
}

pub fn acknowledge_input_producer_barrier(
    host: &crate::runtime::PaneRuntimeHost,
    req: &AcknowledgeInputProducerBarrierArgs,
    ctx: &NativeServiceContext,
) -> Result<bool, AttachCommandError> {
    let client = caller_client_id(ctx)?;
    Ok(crate::private_input::acknowledge_producer(
        host,
        req.pane_id,
        req.nonce,
        req.generation,
        client.0,
        req.supported,
    ))
}

/// Public, secret-free capture suppression request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuppressPaneInputCaptureArgs {
    pub session_id: Uuid,
    pub pane_id: Uuid,
}

/// Acknowledge recording suppression under authenticated attach write ownership.
pub fn suppress_pane_input_capture(
    host: &crate::runtime::PaneRuntimeHost,
    req: &SuppressPaneInputCaptureArgs,
    ctx: &NativeServiceContext,
) -> Result<bool, AttachCommandError> {
    let client_id = caller_client_id(ctx)?;
    let session_id = SessionId(req.session_id);
    let runtime = host
        .registered_session_runtime()
        .ok_or_else(|| failed("pane-runtime manager handle not registered"))?;
    validate_input_selection(
        follow_state(host)?.0.selected_session(client_id),
        session_id,
    )?;
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
    if !crate::private_input::suppress(host, req.pane_id) {
        return Err(failed("capture suppression barrier unavailable"));
    }
    Ok(true)
}

pub fn attach_input(
    host: &crate::runtime::PaneRuntimeHost,
    req: AttachInputArgs,
    ctx: &NativeServiceContext,
) -> Result<
    bmux_pane_runtime_plugin_api::attach_runtime_commands::AttachInputAccepted,
    AttachCommandError,
> {
    let client_id = caller_client_id(ctx)?;
    let session_id = SessionId(req.session_id);
    let runtime = host
        .registered_session_runtime()
        .ok_or_else(|| failed("pane-runtime manager handle not registered"))?;
    validate_input_selection(
        follow_state(host)?.0.selected_session(client_id),
        session_id,
    )?;
    host.ensure_client_has_input_target(client_id)
        .map_err(|reason| AttachCommandError::Denied {
            reason: reason.to_string(),
        })?;
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
    host: &crate::runtime::PaneRuntimeHost,
    req: &AttachOutputArgs,
    ctx: &NativeServiceContext,
) -> Result<AttachOutputRecord, AttachCommandError> {
    let client_id = caller_client_id(ctx)?;
    let session_id = SessionId(req.session_id);
    let runtime = host
        .registered_session_runtime()
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
    host: &crate::runtime::PaneRuntimeHost,
    req: &ObserveViewportArgs,
    ctx: &NativeServiceContext,
) -> Result<AttachViewportSet, AttachCommandError> {
    let client_id = caller_client_id(ctx)?;
    let viewport = crate::runtime::observe_viewport(
        host,
        SessionId(req.session_id),
        client_id,
        req.cols,
        req.rows,
    )
    .map_err(|error| failed(format!("viewport observation failed: {error:?}")))?;
    Ok(AttachViewportSet {
        session_id: req.session_id,
        cols: viewport.cols,
        rows: viewport.rows,
        top_inset: viewport.top_inset,
        right_inset: viewport.right_inset,
        bottom_inset: viewport.bottom_inset,
        left_inset: viewport.left_inset,
        context_id: context_state(host)?
            .0
            .current_for_client(client_id)
            .map(|context| context.id),
    })
}

pub fn attach_set_viewport(
    host: &crate::runtime::PaneRuntimeHost,
    req: &AttachSetViewportArgs,
    ctx: &NativeServiceContext,
) -> Result<AttachViewportSet, AttachCommandError> {
    let client_id = caller_client_id(ctx)?;
    let session_id = SessionId(req.session_id);
    // Sizing the session's panes is input; an attached caller the session
    // policy does not let resize (a view-only device) observes the shared
    // geometry instead, exactly as `observe-viewport` does.
    if !session_action_allowed(host, ctx, session_id, client_id, "pane.resize")? {
        return observe_viewport(
            host,
            &ObserveViewportArgs {
                session_id: req.session_id,
                cols: req.cols,
                rows: req.rows,
            },
            ctx,
        );
    }
    let runtime = host
        .registered_session_runtime()
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
    let context_id = context_state(host)?
        .0
        .current_for_client(client_id)
        .map(|c| c.id);
    host.emit_attach_view_changed_for_layout(session_id);
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

/// The client's selection before a retarget attempt: its context, or
/// whether it had been cleared (no input target).
type PreviousSelection = (Option<Uuid>, bool);

fn restore_context_selection(
    contexts: &bmux_context_state::ContextStateHandle,
    client_id: ClientId,
    (previous_context_id, previously_cleared): PreviousSelection,
) {
    if let Some(previous_context_id) = previous_context_id {
        let _ = contexts.0.select_for_client(
            client_id,
            &PrimitiveContextSelector::ById(previous_context_id),
        );
    } else if previously_cleared {
        // A failed retarget must not hand a cleared client an input target.
        contexts.0.clear_selection_for_client(client_id);
    } else {
        contexts.0.disconnect_client(client_id);
    }
}

pub fn attach_retarget_context(
    host: &crate::runtime::PaneRuntimeHost,
    req: &AttachRetargetContextArgs,
    ctx: &NativeServiceContext,
) -> AttachOutcome<AttachRetargetReady> {
    answer(retarget_context(host, req, ctx))
}

fn retarget_context(
    host: &crate::runtime::PaneRuntimeHost,
    req: &AttachRetargetContextArgs,
    ctx: &NativeServiceContext,
) -> Result<AttachRetargetReady, AttachStop> {
    let deadline = CallerDeadline::of(ctx);
    let client_id = caller_client_id(ctx)?;

    let manager = session_manager(host)?;
    let contexts = context_state(host)?;
    let follow = follow_state(host)?;
    let runtime = host
        .registered_session_runtime()
        .ok_or_else(|| failed("pane-runtime manager handle not registered"))?;

    let selected_target = follow.0.selected_target(client_id);
    let previous_selection = (
        contexts
            .0
            .snapshot()
            .selected_by_client
            .get(&client_id)
            .copied(),
        contexts.0.selection_cleared_for_client(client_id),
    );
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
                restore_context_selection(&contexts, client_id, previous_selection);
                return Err(failed("context has no attached runtime").into());
            };
            (context.id, selected_session_id, false)
        };
    // Every failure below undoes the context selection this handler applied.
    let undo = || {
        if !selection_already_applied {
            restore_context_selection(&contexts, client_id, previous_selection);
        }
    };
    let undo_selection = |error: AttachCommandError| {
        undo();
        error
    };

    // Validate the session exists in the manager.
    if !manager.0.contains(next_session_id) {
        return Err(undo_selection(AttachCommandError::SessionNotFound).into());
    }

    let previous_session = selected_target.and_then(|(_, session_id)| session_id);
    let write_allowed =
        session_write_allowed(host, ctx, next_session_id, client_id).map_err(undo_selection)?;
    let can_write = req.can_write && write_allowed;

    let reservation = reserve_selection(ctx).map_err(undo_selection)?;
    let retargeted = match retarget_attach_stream(
        host,
        &runtime,
        &follow,
        deadline,
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
        Err(stop) => {
            // Nothing was retargeted (a runtime still starting included): hand
            // the selection back; only a release that failed outranks `stop`.
            undo();
            release_selection(ctx, reservation.revision, &stop)?;
            return Err(stop);
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
    host.emit_attach_view_changed_for_layout(next_session_id);

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
    host: &crate::runtime::PaneRuntimeHost,
    req: SetClientAttachPolicyArgs,
    ctx: &NativeServiceContext,
) -> Result<u8, AttachCommandError> {
    let client_id = caller_client_id(ctx)?;
    follow_state(host)?
        .0
        .set_attach_detach_allowed(client_id, req.allow_detach);
    Ok(u8::from(req.allow_detach))
}

pub fn detach(
    host: &crate::runtime::PaneRuntimeHost,
    ctx: &NativeServiceContext,
) -> Result<u8, AttachCommandError> {
    let client_id = caller_client_id(ctx)?;
    if observer_identity(host, client_id) {
        let session = host
            .observers()
            .lock()
            .map_err(|_| failed("observer identities unavailable"))?
            .clients
            .get(&client_id)
            .map(|(_, session)| *session);
        if let Some(session) = session {
            let runtime = host
                .registered_session_runtime()
                .ok_or_else(|| failed("runtime unavailable"))?;
            runtime.0.end_attach(session, client_id);
        }
        return Ok(0);
    }
    let follow = follow_state(host)?;
    if !follow.0.attach_detach_allowed(client_id) {
        return Err(failed("detach is disabled for this connection"));
    }
    let runtime = host
        .registered_session_runtime()
        .ok_or_else(|| failed("pane-runtime manager handle not registered"))?;
    if let Some(stream_session) = follow.0.attached_stream_session(client_id) {
        runtime.0.end_attach(stream_session, client_id);
        runtime
            .0
            .set_client_write_permission(stream_session, client_id, false);
        follow.0.set_attached_stream_session(client_id, None);
        host.publish_pane_event(PaneEvent::ClientDetached {
            session_id: stream_session.0,
        });
    }
    // Clear the selected session so future follow-state lookups see
    // the detach.
    if let Some(selected) = follow.0.selected_session(client_id) {
        let manager = session_manager(host)?;
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

    /// A clients-selection provider whose `begin` applies (the selection moves to revision 5)
    /// but, when `begin_lost`, its reply never arrives; records each `recover` revision.
    struct SelectionHost {
        begin_lost: bool,
        recovered: std::sync::Mutex<Vec<u64>>,
    }

    impl bmux_plugin::ServiceCaller for SelectionHost {
        fn execute_kernel_request(
            &self,
            _: bmux_ipc::Request,
        ) -> bmux_plugin_sdk::Result<bmux_ipc::ResponsePayload> {
            panic!("selection uses generated services")
        }

        fn call_service_raw(
            &self,
            _: &str,
            _: bmux_plugin_sdk::ServiceKind,
            interface: &str,
            operation: &str,
            payload: Vec<u8>,
        ) -> bmux_plugin_sdk::Result<Vec<u8>> {
            use bmux_clients_plugin_api::{
                clients_selection_commands_v1 as commands, clients_selection_state_v1 as state,
            };
            let checked = bmux_plugin_sdk::service_contract::is_checked(&payload);
            let payload = if checked {
                let contract = <bmux_clients_plugin_api::Contract as bmux_plugin_sdk::PluginContract>::service_contract_for(interface).expect("generated contract");
                bmux_plugin_sdk::service_contract::decode_for(interface, contract, &payload)?
                    .to_vec()
            } else {
                payload
            };
            let selection = |revision, suspended| state::Selection {
                revision,
                context_id: None,
                session_id: None,
                suspended,
            };
            let response = match operation {
                "current" => {
                    bmux_plugin_sdk::encode_service_message(&Ok::<_, state::SelectionError>(
                        selection(4, false),
                    ))?
                }
                "begin" if self.begin_lost => {
                    return Err(bmux_plugin_sdk::PluginError::HostTimeout {
                        details: "reply unconfirmed".into(),
                    });
                }
                "begin" => bmux_plugin_sdk::encode_service_message(
                    &Ok::<_, state::SelectionError>(selection(5, true)),
                )?,
                "recover" => {
                    let request: commands::client::RecoverRequest =
                        bmux_plugin_sdk::decode_service_message(&payload)?;
                    self.recovered
                        .lock()
                        .unwrap()
                        .push(request.expected_revision);
                    bmux_plugin_sdk::encode_service_message(&Ok::<_, state::SelectionError>(
                        selection(6, false),
                    ))?
                }
                other => panic!("unexpected selection call {interface}/{other}"),
            };
            Ok(if checked {
                bmux_plugin_sdk::service_contract::encode_response(&response)
            } else {
                response
            })
        }
    }

    #[test]
    fn a_runtime_still_starting_answers_a_timeout_not_a_rejection() {
        let session_id = SessionId(Uuid::nil());
        let stopped = answer::<()>(Err(AttachStop::StillStarting(RuntimeStartPending {
            session_id,
        })))
        .unwrap_err();
        assert!(stopped.error.as_ref().unwrap().is_deadline_exceeded());
        assert_eq!(
            answer::<()>(Err(AttachCommandError::SessionNotFound.into())).unwrap(),
            Err(AttachCommandError::SessionNotFound)
        );
        let mut ctx = test_context();
        assert!(CallerDeadline::of(&ctx).remaining().is_none());
        ctx.cancellation = ctx
            .cancellation
            .with_remaining(Some(std::time::Duration::from_secs(5)));
        let remaining = CallerDeadline::of(&ctx).remaining().unwrap();
        assert!(remaining <= std::time::Duration::from_secs(5));
        assert!(remaining > std::time::Duration::from_secs(4));
    }

    #[test]
    fn a_reservation_whose_reply_was_lost_is_released_not_left_suspended() {
        let lost = SelectionHost {
            begin_lost: true,
            recovered: std::sync::Mutex::new(Vec::new()),
        };
        assert!(reserve_selection(&lost).is_err());
        // Only the reservation this begin would have made (revision 4 + 1) is released.
        assert_eq!(*lost.recovered.lock().unwrap(), [5]);

        let confirmed = SelectionHost {
            begin_lost: false,
            recovered: std::sync::Mutex::new(Vec::new()),
        };
        assert_eq!(reserve_selection(&confirmed).unwrap().revision, 5);
        assert!(confirmed.recovered.lock().unwrap().is_empty());
    }

    #[test]
    fn observer_cleanup_requires_disconnect_not_detach_and_missing_provider_retains_fence() {
        let live_detached = ClientId(crate::ids::new_uuid());
        let retired = ClientId(crate::ids::new_uuid());
        let session = SessionId(crate::ids::new_uuid());
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
        let live = ClientId(crate::ids::new_uuid());
        let session = SessionId(crate::ids::new_uuid());
        let mut identities = ObserverIdentities::default();
        let live_token = crate::ids::new_uuid();
        identities.clients.insert(live, (live_token, session));
        identities.pending_expiry.insert(live, 100);
        let follow = ConnectionFixture(live);
        // More than the admission cap over time must not exhaust the registry.
        for _ in 0..5000 {
            let retired = ClientId(crate::ids::new_uuid());
            identities
                .clients
                .insert(retired, (crate::ids::new_uuid(), session));
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
        let owner = ClientId(crate::ids::new_uuid());
        let other = ClientId(crate::ids::new_uuid());
        let session = SessionId(crate::ids::new_uuid());
        let token = crate::ids::new_uuid();
        let mut identities = ObserverIdentities::default();
        identities.clients.insert(owner, (token, session));
        for (client, request_token, request_session) in [
            (other, token, session),
            (owner, crate::ids::new_uuid(), session),
            (owner, token, SessionId(crate::ids::new_uuid())),
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
        let host = crate::runtime::PaneRuntimeHost::detached();
        let host = &host;
        let observer = ClientId(crate::ids::new_uuid());
        let ordinary = ClientId(crate::ids::new_uuid());
        mark_test_observer(host, observer, SessionId(crate::ids::new_uuid()));
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
            assert!(!observer_operation_allowed(
                host, observer, interface, operation
            ));
            assert!(observer_operation_allowed(
                host, ordinary, interface, operation
            ));
        }
        assert!(observer_operation_allowed(
            host,
            observer,
            "attach-runtime-state/v2",
            "attach-snapshot"
        ));
        assert!(observer_operation_allowed(
            host,
            observer,
            "attach-runtime-commands",
            "observe-viewport"
        ));
        assert!(observer_operation_allowed(
            host,
            observer,
            "attach-runtime-commands",
            "detach"
        ));
        // Detach/grant consumption must not erase the connection's fence.
        assert!(observer_identity(host, observer));
    }

    fn test_context() -> NativeServiceContext {
        use bmux_plugin_sdk::{
            ApiVersion, HostConnectionInfo, HostMetadata, HostScope, ProviderId, RegisteredService,
            ServiceKind, ServiceRequest,
        };
        NativeServiceContext {
            host_service_port: None,
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
        }
    }

    #[test]
    fn absent_permissions_is_local_fallback_but_enabled_missing_principal_fails_closed() {
        let host = crate::runtime::PaneRuntimeHost::detached();
        let host = &host;
        let mut context = test_context();
        let session = SessionId(crate::ids::new_uuid());
        let client = ClientId(crate::ids::new_uuid());
        assert!(session_write_allowed(host, &context, session, client).unwrap());
        context.enabled_plugins.push("bmux.permissions".into());
        assert!(session_write_allowed(host, &context, session, client).is_err());
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
