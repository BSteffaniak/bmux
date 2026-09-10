//! bmux clients plugin — typed owner of per-client identity and view state.
//!
//! Provides typed services that reach the server's client state
//! directly via the IPC kernel-bridge escape hatch
//! (`ServiceCaller::execute_kernel_request`).

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

pub mod follow_state;
mod selection;
pub use follow_state::FollowState;

use bmux_client_state::{
    ClientSummary as PrimitiveClientSummary, FollowEntry, FollowStateHandle, FollowStateReader,
    FollowStateSnapshot, FollowStateWriter, FollowTargetGoneUpdate, FollowTargetUpdate,
};
use bmux_clients_plugin_api::clients_commands::{
    self, ClientAck, ClientsCommandsService, SetCurrentSessionError, SetFollowingError,
};
use bmux_clients_plugin_api::clients_events::{self, ClientEvent};
use bmux_clients_plugin_api::clients_selection_commands_v1;
use bmux_clients_plugin_api::clients_selection_state_v1::{Selection, SelectionError};
use bmux_clients_plugin_api::clients_state::{
    self, ClientQueryError, ClientSummary, ClientsStateService,
};
use bmux_contexts_plugin_api::contexts_commands;
use bmux_contexts_plugin_api::contexts_state::ContextSelector;
use bmux_plugin::{
    ServiceCaller, TypedServiceCaller, global_event_bus, global_plugin_state_registry,
};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{
    PluginEventKind, StatefulPlugin, StatefulPluginError, StatefulPluginHandle,
    StatefulPluginResult, StatefulPluginSnapshot, TypedServiceRegistrationContext,
    TypedServiceRegistry,
};
use bmux_session_models::{ClientId, SessionId};
use bmux_session_state::SessionManagerHandle;
use bmux_sessions_plugin_api::sessions_commands;
use bmux_snapshot_runtime::StatefulPluginRegistry;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use uuid::Uuid;

/// Adapter wrapping the plugin's `Arc<RwLock<FollowState>>` and
/// implementing the domain-agnostic [`FollowStateReader`] +
/// [`FollowStateWriter`] traits from `bmux_client_state`.
///
/// Registered as a [`FollowStateHandle`] in the plugin state registry
/// alongside the concrete `Arc<RwLock<FollowState>>` so consumers can
/// read follow-state through the trait surface without naming the
/// concrete plugin-owned type.
struct FollowStateAdapter {
    inner: Arc<RwLock<FollowState>>,
}

const fn dispatch_client<C: ServiceCaller + Sync + ?Sized>(
    caller: &C,
) -> bmux_plugin::ServiceCallerDispatchClient<'_, C> {
    bmux_plugin::ServiceCallerDispatchClient::new(caller)
}

impl FollowStateAdapter {
    fn with_read<T>(&self, f: impl FnOnce(&FollowState) -> T, fallback: T) -> T {
        self.inner.read().map_or(fallback, |guard| f(&guard))
    }

    fn with_write<T>(&self, f: impl FnOnce(&mut FollowState) -> T, fallback: T) -> T {
        self.inner
            .write()
            .map_or(fallback, |mut guard| f(&mut guard))
    }
}

impl FollowStateReader for FollowStateAdapter {
    fn selected_session(&self, client_id: ClientId) -> Option<SessionId> {
        self.with_read(
            |state| {
                state
                    .selected_target(client_id)
                    .and_then(|(_, session)| session)
            },
            None,
        )
    }

    fn selected_context(&self, client_id: ClientId) -> Option<Uuid> {
        self.with_read(
            |state| {
                state
                    .selected_target(client_id)
                    .and_then(|(context, _)| context)
            },
            None,
        )
    }

    fn follow_target(&self, client_id: ClientId) -> Option<FollowEntry> {
        self.with_read(|state| state.follows.get(&client_id).copied(), None)
    }

    fn list_clients(&self) -> Vec<PrimitiveClientSummary> {
        self.with_read(FollowState::list_clients, Vec::new())
    }

    fn selected_target(&self, client_id: ClientId) -> Option<(Option<Uuid>, Option<SessionId>)> {
        self.with_read(|state| state.selected_target(client_id), None)
    }

    fn is_connected(&self, client_id: ClientId) -> bool {
        self.with_read(|state| state.connected_clients.contains(&client_id), false)
    }

    fn attached_stream_session(&self, client_id: ClientId) -> Option<SessionId> {
        self.with_read(|state| state.attached_stream_session(client_id), None)
    }

    fn attach_detach_allowed(&self, client_id: ClientId) -> bool {
        self.with_read(|state| state.attach_detach_allowed(client_id), true)
    }
}

impl FollowStateWriter for FollowStateAdapter {
    fn connect_client(&self, client_id: ClientId) {
        self.with_write(|state| state.connect_client(client_id), ());
    }

    fn disconnect_client(&self, client_id: ClientId) -> Vec<FollowTargetGoneUpdate> {
        let updates = self.with_write(|state| state.disconnect_client(client_id), Vec::new());
        for update in &updates {
            let _ = global_event_bus().emit(
                &clients_events::EVENT_KIND,
                ClientEvent::FollowTargetGone {
                    follower_client_id: update.follower_client_id.0,
                    former_leader_client_id: update.former_leader_client_id.0,
                },
            );
        }
        updates
    }

    fn set_selected_target(
        &self,
        client_id: ClientId,
        context_id: Option<Uuid>,
        session_id: Option<SessionId>,
    ) {
        self.with_write(
            |state| state.set_selected_target(client_id, context_id, session_id),
            (),
        );
    }

    fn clear_all_selections(&self) {
        self.with_write(
            |state| {
                let clients: Vec<ClientId> = state.connected_clients.iter().copied().collect();
                for client_id in clients {
                    state.selected_contexts.insert(client_id, None);
                    state.selected_sessions.insert(client_id, None);
                }
            },
            (),
        );
    }

    fn sync_followers_from_leader(
        &self,
        leader_client_id: ClientId,
        selected_context: Option<Uuid>,
        selected_session: Option<SessionId>,
    ) -> Vec<FollowTargetUpdate> {
        self.with_write(
            |state| {
                state.sync_followers_from_leader(
                    leader_client_id,
                    selected_context,
                    selected_session,
                )
            },
            Vec::new(),
        )
    }

    fn start_follow(
        &self,
        follower_client_id: ClientId,
        leader_client_id: ClientId,
        global: bool,
    ) -> Result<(Option<Uuid>, Option<SessionId>), &'static str> {
        self.with_write(
            |state| state.start_follow(follower_client_id, leader_client_id, global),
            Err("follow-state lock poisoned"),
        )
    }

    fn stop_follow(&self, follower_client_id: ClientId) -> bool {
        self.with_write(|state| state.stop_follow(follower_client_id), false)
    }

    fn clear_all_follow_state(&self) {
        self.with_write(
            |state| {
                state.follows.clear();
                state.selected_contexts.clear();
                state.selected_sessions.clear();
            },
            (),
        );
    }

    fn clear_selections_for_session(&self, session_id: SessionId) {
        self.with_write(
            |state| {
                let affected_clients: Vec<ClientId> = state
                    .selected_sessions
                    .iter()
                    .filter_map(|(client_id, selected)| {
                        (*selected == Some(session_id)).then_some(*client_id)
                    })
                    .collect();

                for client_id in &affected_clients {
                    state.set_selected_target(*client_id, None, None);
                }
                for client_id in affected_clients {
                    let _ = state.sync_followers_from_leader(client_id, None, None);
                }
            },
            (),
        );
    }

    fn snapshot(&self) -> FollowStateSnapshot {
        self.with_read(
            |state| FollowStateSnapshot {
                connected_clients: state.connected_clients.clone(),
                selected_contexts: state.selected_contexts.clone(),
                selected_sessions: state.selected_sessions.clone(),
                follows: state
                    .follows
                    .iter()
                    .map(|(id, entry)| (*id, (*entry).into()))
                    .collect(),
                attached_stream_sessions: state.attached_stream_sessions.clone(),
                attach_detach_allowed: state.attach_detach_allowed.clone(),
            },
            FollowStateSnapshot::default(),
        )
    }

    fn restore_snapshot(&self, snapshot: FollowStateSnapshot) {
        self.with_write(
            |state| {
                state.connected_clients = snapshot.connected_clients;
                state.selected_contexts = snapshot.selected_contexts;
                state.selected_sessions = snapshot.selected_sessions;
                state.follows = snapshot
                    .follows
                    .into_iter()
                    .map(|(id, entry)| (id, entry.into()))
                    .collect();
                state.attached_stream_sessions = snapshot.attached_stream_sessions;
                state.attach_detach_allowed = snapshot.attach_detach_allowed;
            },
            (),
        );
    }

    fn set_attached_stream_session(&self, client_id: ClientId, session_id: Option<SessionId>) {
        self.with_write(
            |state| state.set_attached_stream_session(client_id, session_id),
            (),
        );
    }

    fn set_attach_detach_allowed(&self, client_id: ClientId, allowed: bool) {
        self.with_write(
            |state| state.set_attach_detach_allowed(client_id, allowed),
            (),
        );
    }
}

// ── StatefulPlugin participant for persistence ─────────────────────

/// Stable id for the follow-state snapshot surface.
const CLIENTS_STATEFUL_ID: PluginEventKind =
    PluginEventKind::from_static("bmux.clients/follow-state");

/// Current snapshot schema version for follow-state. Increment when
/// the on-disk shape of [`FollowStateSnapshot`] changes in a way that
/// requires restore-path branching.
const CLIENTS_STATEFUL_VERSION: u32 = 1;

/// Snapshot participant that serializes the plugin's [`FollowState`]
/// via the domain-agnostic [`FollowStateWriter::snapshot`] /
/// [`FollowStateWriter::restore_snapshot`] hooks.
struct ClientsStatefulPlugin {
    writer: Arc<dyn FollowStateWriter>,
}

impl StatefulPlugin for ClientsStatefulPlugin {
    fn id(&self) -> PluginEventKind {
        CLIENTS_STATEFUL_ID
    }

    fn restore_dependencies(&self) -> Vec<PluginEventKind> {
        vec![
            PluginEventKind::from_static("bmux.sessions/session-manager"),
            PluginEventKind::from_static("bmux.contexts/context-state"),
        ]
    }

    fn snapshot(&self) -> StatefulPluginResult<StatefulPluginSnapshot> {
        let snap = self.writer.snapshot();
        let bytes =
            serde_json::to_vec(&snap).map_err(|err| StatefulPluginError::SnapshotFailed {
                plugin: CLIENTS_STATEFUL_ID.as_str().to_string(),
                details: err.to_string(),
            })?;
        Ok(StatefulPluginSnapshot::new(
            CLIENTS_STATEFUL_ID,
            CLIENTS_STATEFUL_VERSION,
            bytes,
        ))
    }

    fn validate_snapshot(&self, snapshot: &StatefulPluginSnapshot) -> StatefulPluginResult<()> {
        decode_clients_snapshot(snapshot).map(|_| ())
    }

    fn restore_snapshot(&self, snapshot: StatefulPluginSnapshot) -> StatefulPluginResult<()> {
        self.writer
            .restore_snapshot(decode_clients_snapshot(&snapshot)?);
        Ok(())
    }
}

fn decode_clients_snapshot(
    snapshot: &StatefulPluginSnapshot,
) -> StatefulPluginResult<FollowStateSnapshot> {
    if snapshot.version != CLIENTS_STATEFUL_VERSION {
        return Err(StatefulPluginError::UnsupportedVersion {
            plugin: CLIENTS_STATEFUL_ID.as_str().to_string(),
            version: snapshot.version,
            expected: vec![CLIENTS_STATEFUL_VERSION],
        });
    }
    serde_json::from_slice(&snapshot.bytes).map_err(|err| StatefulPluginError::RestoreFailed {
        plugin: CLIENTS_STATEFUL_ID.as_str().to_string(),
        details: err.to_string(),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SetCurrentSessionArgs {
    session_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SetFollowingArgs {
    #[serde(default)]
    target_client_id: Option<Uuid>,
    global: bool,
}

#[derive(Default)]
pub struct ClientsPlugin;

impl RustPlugin for ClientsPlugin {
    type Contract = bmux_clients_plugin_api::Contract;

    fn activate(
        &mut self,
        _context: NativeLifecycleContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        // Register the global follow-state handle. Core server code
        // (and other plugins) access this via
        // `global_plugin_state_registry().expect_state::<FollowState>()`.
        // Re-activation is a no-op: only the first `register` call
        // installs the handle; subsequent calls replace it (which is
        // fine because we create a fresh default state in each case
        // and there is exactly one bundled clients plugin per host).
        let state: Arc<RwLock<FollowState>> = Arc::new(RwLock::new(FollowState::default()));
        global_plugin_state_registry().register::<FollowState>(&state);

        // Register the trait-object handle so consumers can reach
        // follow state through the domain-agnostic reader/writer
        // surface without naming the concrete plugin-owned type.
        let adapter = FollowStateAdapter {
            inner: Arc::clone(&state),
        };
        let handle = Arc::new(RwLock::new(FollowStateHandle::new(adapter)));
        global_plugin_state_registry().register::<FollowStateHandle>(&handle);

        // Register this plugin as a persistence participant so the
        // snapshot-orchestration plugin can drive save/restore over
        // follow-state on its schedule.
        let writer_for_snapshot: Arc<dyn FollowStateWriter> = {
            let guard = handle
                .read()
                .expect("freshly-created FollowStateHandle lock is poisoned");
            Arc::clone(&guard.0)
        };
        let stateful = StatefulPluginHandle::new(ClientsStatefulPlugin {
            writer: writer_for_snapshot,
        });
        let registry = global_plugin_state_registry();
        let stateful_registry = bmux_snapshot_runtime::get_or_init_stateful_registry(
            || registry.get::<StatefulPluginRegistry>(),
            |fresh| {
                registry.register::<StatefulPluginRegistry>(fresh);
            },
        );
        stateful_registry
            .write()
            .expect("stateful plugin registry lock poisoned")
            .push(stateful);

        global_event_bus().register_channel::<ClientEvent>(clients_events::EVENT_KIND);
        Ok(bmux_plugin_sdk::EXIT_OK)
    }

    fn run_command(
        &mut self,
        _context: NativeCommandContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        Err(PluginCommandError::unknown_command(""))
    }

    fn invoke_service(&self, context: NativeServiceContext) -> ServiceResponse {
        bmux_plugin_sdk::route_service!(context, {
            "clients-selection-state-v1", "current" => |_req: (), ctx| {
                Ok::<Result<Selection, SelectionError>, ServiceResponse>(selection_operation(ctx.caller_client_id, |state, client| state.selection(client)))
            },
            "clients-selection-commands-v1", "recover" => |req: clients_selection_commands_v1::client::RecoverRequest, ctx| {
                Ok::<Result<Selection, SelectionError>, ServiceResponse>(selection_operation(ctx.caller_client_id, |state, client| state.recover_selection(client, req.expected_revision)))
            },
            "clients-selection-commands-v1", "begin" => |req: clients_selection_commands_v1::client::BeginRequest, ctx| {
                Ok::<Result<Selection, SelectionError>, ServiceResponse>(selection_operation(ctx.caller_client_id, |state, client| state.begin_selection(client, req.expected_revision)))
            },
            "clients-selection-commands-v1", "commit" => |req: clients_selection_commands_v1::client::CommitRequest, ctx| {
                Ok::<Result<Selection, SelectionError>, ServiceResponse>(commit_client_selection(ctx, ctx.caller_client_id, &req))
            },
            "clients-state", "list-clients" => |_req: (), _ctx| {
                list_clients_local()
                    .map_err(|e| ServiceResponse::error("list_failed", e))
            },
            "clients-state", "current-client" => |_req: (), ctx| {
                Ok::<Result<ClientSummary, ClientQueryError>, ServiceResponse>(
                    current_client_local(ctx.caller_client_id)
                )
            },
            "clients-commands", "set-current-session" => |req: SetCurrentSessionArgs, ctx| {
                Ok::<Result<ClientAck, SetCurrentSessionError>, ServiceResponse>(
                    set_current_session_local(ctx, ctx.caller_client_id, req.session_id)
                )
            },
            "clients-commands", "set-following" => |req: SetFollowingArgs, ctx| {
                Ok::<Result<ClientAck, SetFollowingError>, ServiceResponse>(
                    set_following_via_ipc(ctx, ctx.caller_client_id, &req)
                )
            },
        })
    }

    fn register_typed_services(
        &self,
        context: TypedServiceRegistrationContext<'_>,
        registry: &mut TypedServiceRegistry,
    ) {
        let caller = Arc::new(TypedServiceCaller::from_registration_context(&context));

        let state: Arc<dyn ClientsStateService + Send + Sync> =
            Arc::new(ClientsStateHandle::new(Arc::clone(&caller)));
        let _ = clients_state::register_provider(registry, state);

        let selection = Arc::new(SelectionHandle {
            caller: caller.clone(),
        });
        let _ = bmux_clients_plugin_api::clients_selection_state_v1::register_provider(
            registry,
            selection.clone(),
        );
        let _ = clients_selection_commands_v1::register_provider(registry, selection);
        let commands: Arc<dyn ClientsCommandsService + Send + Sync> =
            Arc::new(ClientsCommandsHandle::new(caller));
        let _ = clients_commands::register_provider(registry, commands);
    }
}

struct SelectionHandle {
    caller: Arc<TypedServiceCaller>,
}

impl bmux_clients_plugin_api::clients_selection_state_v1::ClientsSelectionStateV1Service
    for SelectionHandle
{
    fn current<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Selection, SelectionError>> + Send + 'a>> {
        Box::pin(async move {
            selection_operation(
                current_client_id_for_typed_handle(&self.caller),
                |state, client| state.selection(client),
            )
        })
    }
}

impl clients_selection_commands_v1::ClientsSelectionCommandsV1Service for SelectionHandle {
    fn begin<'a>(
        &'a self,
        expected_revision: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Selection, SelectionError>> + Send + 'a>> {
        Box::pin(async move {
            selection_operation(
                current_client_id_for_typed_handle(&self.caller),
                |state, client| state.begin_selection(client, expected_revision),
            )
        })
    }
    fn recover<'a>(
        &'a self,
        expected_revision: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Selection, SelectionError>> + Send + 'a>> {
        Box::pin(async move {
            selection_operation(
                current_client_id_for_typed_handle(&self.caller),
                |state, client| state.recover_selection(client, expected_revision),
            )
        })
    }
    fn commit<'a>(
        &'a self,
        expected_revision: u64,
        context_id: Option<Uuid>,
        session_id: Option<Uuid>,
    ) -> Pin<Box<dyn Future<Output = Result<Selection, SelectionError>> + Send + 'a>> {
        Box::pin(async move {
            commit_client_selection(
                self.caller.as_ref(),
                current_client_id_for_typed_handle(&self.caller),
                &clients_selection_commands_v1::client::CommitRequest {
                    expected_revision,
                    context_id,
                    session_id,
                },
            )
        })
    }
}

fn commit_client_selection(
    caller: &(impl ServiceCaller + Sync),
    client: Option<Uuid>,
    req: &clients_selection_commands_v1::client::CommitRequest,
) -> Result<Selection, SelectionError> {
    let client_id = client.ok_or(SelectionError::NoCurrentClient)?;
    if let Some(session) = req.session_id {
        authorize_selection(caller, client_id, req.context_id, session)?;
        let manager = global_plugin_state_registry()
            .get::<SessionManagerHandle>()
            .ok_or(SelectionError::InvalidTarget)?;
        if !manager
            .read()
            .map_err(|_| SelectionError::InvalidTarget)?
            .0
            .contains(SessionId(session))
        {
            return Err(SelectionError::InvalidTarget);
        }
    }
    if let Some(context_id) = req.context_id {
        // Read the published reader contract directly. Re-entering contexts
        // service dispatch here can deadlock behind a waiting plugin writer.
        let handle = global_plugin_state_registry()
            .get::<bmux_context_state::ContextStateHandle>()
            .ok_or(SelectionError::InvalidTarget)?;
        let handle = handle
            .read()
            .map_err(|_| SelectionError::InvalidTarget)?
            .clone();
        let context = handle
            .0
            .list()
            .into_iter()
            .find(|context| context.id == context_id)
            .ok_or(SelectionError::InvalidTarget)?;
        let context = bmux_contexts_plugin_api::contexts_state::ContextSummary {
            id: context.id,
            name: context.name,
            attributes: context.attributes,
        };
        validate_selection_binding(context_id, req.session_id, &context)?;
    }
    selection_operation(client, |state, client| {
        state.commit_selection(
            client,
            req.expected_revision,
            req.context_id,
            req.session_id,
        )
    })
}

struct SelectionPolicyCaller<'a, C> {
    caller: &'a C,
    missing: std::sync::atomic::AtomicBool,
}
impl<C: ServiceCaller + Sync> ServiceCaller for SelectionPolicyCaller<'_, C> {
    fn call_service_raw(
        &self,
        capability: &str,
        kind: ServiceKind,
        interface: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> bmux_plugin_sdk::Result<Vec<u8>> {
        let result = self
            .caller
            .call_service_raw(capability, kind, interface, operation, payload);
        if matches!(
            &result,
            Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
                operation: "call_service"
            })
        ) {
            self.missing
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        result
    }
    fn execute_kernel_request(
        &self,
        _request: bmux_ipc::Request,
    ) -> bmux_plugin_sdk::Result<bmux_ipc::ResponsePayload> {
        Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
            operation: "selection policy kernel access",
        })
    }
}

fn authorize_selection(
    caller: &(impl ServiceCaller + Sync),
    client: Uuid,
    context: Option<Uuid>,
    session: Uuid,
) -> Result<(), SelectionError> {
    let principal = global_plugin_state_registry()
        .get::<bmux_client_state::ClientPrincipalHandle>()
        .and_then(|handle| handle.read().ok().map(|value| value.clone()))
        .and_then(|handle| handle.0.get(ClientId(client)))
        .unwrap_or_else(Uuid::nil);
    let policy = SelectionPolicyCaller {
        caller,
        missing: std::sync::atomic::AtomicBool::new(false),
    };
    let mut dispatch = dispatch_client(&policy);
    let decision = match bmux_plugin::block_on_typed_dispatch(
        bmux_permissions_plugin_api::session_policy_state::client::check(
            &mut dispatch,
            session,
            context,
            client,
            principal,
            "session.select".to_string(),
            None,
            None,
            None,
        ),
    ) {
        Ok(decision) => decision,
        Err(_) if policy.missing.load(std::sync::atomic::Ordering::Relaxed) => return Ok(()),
        Err(error) => {
            return Err(SelectionError::Failed {
                reason: format!("selection authorization failed: {error}"),
            });
        }
    };
    if !decision.allowed {
        return Err(SelectionError::Failed {
            reason: decision
                .reason
                .unwrap_or_else(|| "selection denied".to_string()),
        });
    }
    Ok(())
}

fn validate_selection_binding(
    context_id: Uuid,
    session_id: Option<Uuid>,
    context: &bmux_contexts_plugin_api::contexts_state::ContextSummary,
) -> Result<(), SelectionError> {
    let binding = context
        .attributes
        .get("bmux.session_id")
        .and_then(|value| Uuid::parse_str(value).ok());
    if context.id != context_id || session_id.is_none() || binding != session_id {
        return Err(SelectionError::InvalidTarget);
    }
    Ok(())
}

fn selection_operation(
    client: Option<Uuid>,
    operation: impl FnOnce(&mut FollowState, ClientId) -> Result<Selection, SelectionError>,
) -> Result<Selection, SelectionError> {
    let client = ClientId(client.ok_or(SelectionError::NoCurrentClient)?);
    let state = global_plugin_state_registry()
        .get::<FollowState>()
        .ok_or_else(|| SelectionError::Failed {
            reason: "clients state unavailable".into(),
        })?;
    let mut guard = state.write().map_err(|_| SelectionError::Failed {
        reason: "clients state poisoned".into(),
    })?;
    operation(&mut guard, client)
}

// ── IPC helpers ──────────────────────────────────────────────────────

#[allow(clippy::too_many_lines)]
fn set_current_session_local(
    caller: &(impl ServiceCaller + Sync),
    caller_client_id: Option<Uuid>,
    session_id: Uuid,
) -> Result<ClientAck, SetCurrentSessionError> {
    let Some(self_id) = caller_client_id else {
        return Err(SetCurrentSessionError::Denied {
            reason: "no current client identity".to_string(),
        });
    };
    let self_client_id = ClientId(self_id);
    let next_session = SessionId(session_id);

    let Some(sessions) = global_plugin_state_registry().get::<SessionManagerHandle>() else {
        return Err(SetCurrentSessionError::Denied {
            reason: "sessions plugin state not registered".to_string(),
        });
    };
    let sessions = sessions
        .read()
        .map_err(|_| SetCurrentSessionError::Denied {
            reason: "sessions plugin state lock poisoned".to_string(),
        })?;
    if !sessions.0.contains(next_session) {
        return Err(SetCurrentSessionError::NotFound);
    }
    drop(sessions);

    let Some(state_handle) = global_plugin_state_registry().get::<FollowState>() else {
        return Err(SetCurrentSessionError::Denied {
            reason: "clients plugin state not registered".to_string(),
        });
    };

    let (previous_session, follower_previous_sessions, reservation) = {
        let mut follow_state =
            state_handle
                .write()
                .map_err(|_| SetCurrentSessionError::Denied {
                    reason: "follow state lock poisoned".to_string(),
                })?;

        if !follow_state.connected_clients.contains(&self_client_id) {
            return Err(SetCurrentSessionError::NotFound);
        }

        let previous_session = follow_state
            .selected_sessions
            .get(&self_client_id)
            .copied()
            .flatten();
        let follower_previous_sessions = follow_state
            .follows
            .iter()
            .filter(|(_, entry)| entry.leader_client_id == self_client_id && entry.global)
            .map(|(follower_id, _)| {
                (
                    *follower_id,
                    follow_state
                        .selected_sessions
                        .get(follower_id)
                        .copied()
                        .flatten(),
                )
            })
            .collect::<Vec<_>>();

        let revision = follow_state
            .selection(self_client_id)
            .map_err(|error| SetCurrentSessionError::Denied {
                reason: format!("{error:?}"),
            })?
            .revision;
        let reservation = follow_state
            .begin_selection(self_client_id, revision)
            .map_err(|error| SetCurrentSessionError::Denied {
                reason: format!("{error:?}"),
            })?;
        drop(follow_state);
        (previous_session, follower_previous_sessions, reservation)
    };

    if previous_session != Some(next_session) {
        reconcile_client_membership_via_typed_dispatch(
            caller,
            self_id,
            previous_session.map(|s| s.0),
            Some(session_id),
        )
        .map_err(|reason| SetCurrentSessionError::Denied { reason })?;
    }

    commit_client_selection(
        caller,
        Some(self_id),
        &clients_selection_commands_v1::client::CommitRequest {
            expected_revision: reservation.revision,
            context_id: None,
            session_id: Some(session_id),
        },
    )
    .map_err(|error| SetCurrentSessionError::Denied {
        reason: format!("selection commit failed; input suspended: {error:?}"),
    })?;
    let follower_updates = state_handle
        .write()
        .map_err(|_| SetCurrentSessionError::Denied {
            reason: "follow state lock poisoned".into(),
        })?
        .sync_followers_from_leader(self_client_id, None, Some(next_session));

    let _ = global_event_bus().emit(
        &clients_events::EVENT_KIND,
        ClientEvent::SessionSelected {
            client_id: self_id,
            session_id,
        },
    );

    for update in follower_updates {
        if let Some(update_session) = update.session_id {
            let previous = follower_previous_sessions
                .iter()
                .find_map(|(client_id, previous)| {
                    (*client_id == update.follower_client_id).then_some(*previous)
                })
                .flatten();
            if previous != Some(update_session) {
                let _ = reconcile_client_membership_via_typed_dispatch(
                    caller,
                    update.follower_client_id.0,
                    previous.map(|s| s.0),
                    Some(update_session.0),
                );
            }

            let _ = global_event_bus().emit(
                &clients_events::EVENT_KIND,
                ClientEvent::FollowTargetChanged {
                    follower_client_id: update.follower_client_id.0,
                    leader_client_id: update.leader_client_id.0,
                    context_id: update.context_id,
                    session_id: update_session.0,
                },
            );
        }
    }

    Ok(ClientAck { client_id: self_id })
}

fn list_clients_local() -> Result<Vec<ClientSummary>, String> {
    let Some(state) = global_plugin_state_registry().get::<FollowState>() else {
        return Err("clients plugin state not registered".to_string());
    };
    let follow_state = state
        .read()
        .map_err(|_| "follow state lock poisoned".to_string())?;
    Ok(follow_state
        .list_clients()
        .iter()
        .map(primitive_summary_to_typed)
        .collect())
}

fn current_client_local(caller_client_id: Option<Uuid>) -> Result<ClientSummary, ClientQueryError> {
    let Some(self_id) = caller_client_id else {
        return Err(ClientQueryError::NoCurrentClient);
    };
    let Some(state) = global_plugin_state_registry().get::<FollowState>() else {
        return Err(ClientQueryError::NoCurrentClient);
    };
    let follow_state = state
        .read()
        .map_err(|_| ClientQueryError::NoCurrentClient)?;
    follow_state
        .list_clients()
        .iter()
        .find(|entry| entry.id == self_id)
        .map(primitive_summary_to_typed)
        .ok_or(ClientQueryError::NotFound)
}

#[allow(clippy::too_many_lines)]
fn set_following_via_ipc(
    caller: &(impl ServiceCaller + Sync),
    caller_client_id: Option<Uuid>,
    req: &SetFollowingArgs,
) -> Result<ClientAck, SetFollowingError> {
    use bmux_session_models::{ClientId, SessionId};

    // Determine self-id for the returned `ClientAck`.
    let Some(self_id) = caller_client_id else {
        return Err(SetFollowingError::Denied {
            reason: "no current client identity".to_string(),
        });
    };
    let self_client_id = ClientId(self_id);

    // Validate inputs.
    if (req.target_client_id, req.global) == (None, true) {
        return Err(SetFollowingError::Denied {
            reason: "global follow requires an explicit target client id".to_string(),
        });
    }

    // Acquire plugin-owned FollowState.
    let Some(state_handle) = global_plugin_state_registry().get::<FollowState>() else {
        return Err(SetFollowingError::Denied {
            reason: "clients plugin state not registered".to_string(),
        });
    };

    // Disable-follow path: target_client_id == None && global == false.
    if req.target_client_id.is_none() {
        let removed = {
            let mut follow_state = state_handle
                .write()
                .map_err(|_| SetFollowingError::Denied {
                    reason: "follow state lock poisoned".to_string(),
                })?;
            follow_state.stop_follow(self_client_id)
        };
        if removed {
            let _ = global_event_bus().emit(
                &clients_events::EVENT_KIND,
                ClientEvent::FollowChanged {
                    client_id: self_id,
                    target_client_id: None,
                    global: false,
                },
            );
            let _ = global_event_bus().emit(
                &clients_events::EVENT_KIND,
                ClientEvent::FollowStopped {
                    follower_client_id: self_id,
                },
            );
        }
        return Ok(ClientAck { client_id: self_id });
    }

    // Enable-follow path.
    let target_client_id = req.target_client_id.expect("validated above");
    let leader_client_id = ClientId(target_client_id);

    let (initial_target_context, initial_target_session) = {
        let mut follow_state = state_handle
            .write()
            .map_err(|_| SetFollowingError::Denied {
                reason: "follow state lock poisoned".to_string(),
            })?;
        match follow_state.start_follow(self_client_id, leader_client_id, req.global) {
            Ok(initial) => initial,
            Err(reason) => {
                return Err(SetFollowingError::Denied {
                    reason: reason.to_string(),
                });
            }
        }
    };

    // For global follow, mirror the leader's selection onto the
    // follower: select the leader's context and reconcile session
    // membership. Typed dispatch into contexts-commands +
    // sessions-commands keeps this plugin ignorant of the other
    // plugins' internals.
    if req.global {
        if let Some(initial_target_context) = initial_target_context {
            let _ = select_context_via_typed_dispatch(caller, initial_target_context);
        }

        // Determine the follower's previous session, for session
        // membership reconciliation.
        let previous_session: Option<SessionId> = {
            let follow_state = state_handle.read().map_err(|_| SetFollowingError::Denied {
                reason: "follow state lock poisoned".to_string(),
            })?;
            follow_state
                .selected_sessions
                .get(&self_client_id)
                .copied()
                .flatten()
        };

        // Update FollowState to point the follower at the leader's
        // session. `set_selected_target` writes selected_contexts and
        // selected_sessions atomically.
        let _ = {
            let mut follow_state = state_handle
                .write()
                .map_err(|_| SetFollowingError::Denied {
                    reason: "follow state lock poisoned".to_string(),
                })?;
            follow_state.set_selected_target(
                self_client_id,
                initial_target_context,
                initial_target_session,
            );
            follow_state.sync_followers_from_leader(
                leader_client_id,
                initial_target_context,
                initial_target_session,
            )
        };

        // Reconcile session-manager client membership via typed dispatch.
        if previous_session != initial_target_session {
            let _ = reconcile_client_membership_via_typed_dispatch(
                caller,
                self_id,
                previous_session.map(|s| s.0),
                initial_target_session.map(|s| s.0),
            );
        }
    }

    // Emit event-bus events for plugin consumers and cross-process
    // attach-UI subscribers via the generic plugin-bus forwarder.
    let _ = global_event_bus().emit(
        &clients_events::EVENT_KIND,
        ClientEvent::FollowChanged {
            client_id: self_id,
            target_client_id: Some(target_client_id),
            global: req.global,
        },
    );
    let _ = global_event_bus().emit(
        &clients_events::EVENT_KIND,
        ClientEvent::FollowStarted {
            follower_client_id: self_id,
            leader_client_id: target_client_id,
            global: req.global,
        },
    );
    if let Some(session_id) = initial_target_session {
        let _ = global_event_bus().emit(
            &clients_events::EVENT_KIND,
            ClientEvent::FollowTargetChanged {
                follower_client_id: self_id,
                leader_client_id: target_client_id,
                context_id: initial_target_context,
                session_id: session_id.0,
            },
        );
    }

    Ok(ClientAck { client_id: self_id })
}

fn select_context_via_typed_dispatch(
    caller: &(impl ServiceCaller + Sync),
    context_id: Uuid,
) -> Result<(), String> {
    let mut client = dispatch_client(caller);
    let result = bmux_plugin::block_on_typed_dispatch(contexts_commands::client::select_context(
        &mut client,
        ContextSelector {
            id: Some(context_id),
            name: None,
        },
    ))
    .map_err(|err| err.to_string())?;
    result.map(|_| ()).map_err(|err| format!("{err:?}"))
}

fn reconcile_client_membership_via_typed_dispatch(
    caller: &(impl ServiceCaller + Sync),
    client_id: Uuid,
    previous: Option<Uuid>,
    next: Option<Uuid>,
) -> Result<(), String> {
    let mut client = dispatch_client(caller);
    let result = bmux_plugin::block_on_typed_dispatch(
        sessions_commands::client::reconcile_client_membership(
            &mut client,
            client_id,
            previous,
            next,
        ),
    )
    .map_err(|err| err.to_string())?;
    result.map(|_| ()).map_err(|err| format!("{err:?}"))
}

// ── Typed handles ────────────────────────────────────────────────────

pub struct ClientsStateHandle {
    caller: Arc<TypedServiceCaller>,
}

impl ClientsStateHandle {
    const fn new(caller: Arc<TypedServiceCaller>) -> Self {
        Self { caller }
    }
}

impl ClientsStateService for ClientsStateHandle {
    fn list_clients<'a>(&'a self) -> Pin<Box<dyn Future<Output = Vec<ClientSummary>> + Send + 'a>> {
        Box::pin(async move {
            self.caller
                .call_service::<(), Vec<ClientSummary>>(
                    bmux_clients_plugin_api::capabilities::CLIENTS_READ.as_str(),
                    ServiceKind::Query,
                    clients_state::INTERFACE_ID.as_str(),
                    "list-clients",
                    &(),
                )
                .unwrap_or_default()
        })
    }

    fn current_client<'a>(
        &'a self,
    ) -> Pin<
        Box<dyn Future<Output = std::result::Result<ClientSummary, ClientQueryError>> + Send + 'a>,
    > {
        Box::pin(async move {
            self.caller
                .call_service::<(), Result<ClientSummary, ClientQueryError>>(
                    bmux_clients_plugin_api::capabilities::CLIENTS_READ.as_str(),
                    ServiceKind::Query,
                    clients_state::INTERFACE_ID.as_str(),
                    "current-client",
                    &(),
                )
                .map_or(Err(ClientQueryError::NoCurrentClient), |result| result)
        })
    }
}

pub struct ClientsCommandsHandle {
    caller: Arc<TypedServiceCaller>,
}

impl ClientsCommandsHandle {
    const fn new(caller: Arc<TypedServiceCaller>) -> Self {
        Self { caller }
    }
}

impl ClientsCommandsService for ClientsCommandsHandle {
    fn set_current_session<'a>(
        &'a self,
        session_id: Uuid,
    ) -> Pin<
        Box<
            dyn Future<Output = std::result::Result<ClientAck, SetCurrentSessionError>> + Send + 'a,
        >,
    > {
        Box::pin(async move {
            let caller_client_id = current_client_id_for_typed_handle(self.caller.as_ref());
            set_current_session_local(self.caller.as_ref(), caller_client_id, session_id)
        })
    }

    fn set_following<'a>(
        &'a self,
        target_client_id: Option<Uuid>,
        global: bool,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<ClientAck, SetFollowingError>> + Send + 'a>>
    {
        Box::pin(async move {
            // Handle callers don't have `caller_client_id` threaded
            // through (TypedServiceCaller doesn't carry it), so fall
            // back to a typed `current-client` lookup to obtain it.
            let caller_client_id = current_client_id_for_typed_handle(self.caller.as_ref());
            set_following_via_ipc(
                self.caller.as_ref(),
                caller_client_id,
                &SetFollowingArgs {
                    target_client_id,
                    global,
                },
            )
        })
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

fn current_client_id_for_typed_handle(caller: &TypedServiceCaller) -> Option<Uuid> {
    match caller.call_service::<(), std::result::Result<ClientSummary, ClientQueryError>>(
        bmux_clients_plugin_api::capabilities::CLIENTS_READ.as_str(),
        ServiceKind::Query,
        clients_state::INTERFACE_ID.as_str(),
        clients_state::OP_CURRENT_CLIENT.as_str(),
        &(),
    ) {
        Ok(Ok(summary)) => Some(summary.id),
        _ => None,
    }
}

const fn primitive_summary_to_typed(summary: &PrimitiveClientSummary) -> ClientSummary {
    ClientSummary {
        id: summary.id,
        selected_session_id: summary.selected_session_id,
        selected_context_id: summary.selected_context_id,
        following_client_id: summary.following_client_id,
        following_global: summary.following_global,
    }
}

bmux_plugin_sdk::export_plugin!(ClientsPlugin, include_str!("../plugin.toml"));
