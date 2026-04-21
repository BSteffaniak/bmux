//! bmux clients plugin — typed owner of per-client identity and view state.
//!
//! Provides typed services that reach the server's client state
//! directly via the IPC kernel-bridge escape hatch
//! (`ServiceCaller::execute_kernel_request`).

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

pub use bmux_clients_plugin_api::FollowState;

use bmux_client_state::{
    FollowEntry, FollowStateHandle, FollowStateReader, FollowStateSnapshot, FollowStateWriter,
    FollowTargetUpdate,
};
use bmux_clients_plugin_api::clients_commands::{
    self, ClientAck, ClientsCommandsService, SetCurrentSessionError, SetFollowingError,
};
use bmux_clients_plugin_api::clients_events::{self, ClientEvent};
use bmux_clients_plugin_api::clients_state::{
    self, ClientQueryError, ClientSummary, ClientsStateService,
};
use bmux_ipc::Event;
use bmux_plugin::{
    ServiceCaller, TypedServiceCaller, global_event_bus, global_plugin_state_registry,
};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{HostScope, TypedServiceRegistrationContext, TypedServiceRegistry};
use bmux_session_models::{ClientId, SessionId};
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
            |state| state.selected_sessions.get(&client_id).copied().flatten(),
            None,
        )
    }

    fn selected_context(&self, client_id: ClientId) -> Option<Uuid> {
        self.with_read(
            |state| state.selected_contexts.get(&client_id).copied().flatten(),
            None,
        )
    }

    fn follow_target(&self, client_id: ClientId) -> Option<FollowEntry> {
        self.with_read(|state| state.follows.get(&client_id).copied(), None)
    }

    fn list_clients(&self) -> Vec<bmux_ipc::ClientSummary> {
        self.with_read(
            FollowState::list_clients,
            Vec::<bmux_ipc::ClientSummary>::new(),
        )
    }

    fn selected_target(&self, client_id: ClientId) -> Option<(Option<Uuid>, Option<SessionId>)> {
        self.with_read(|state| state.selected_target(client_id), None)
    }

    fn is_connected(&self, client_id: ClientId) -> bool {
        self.with_read(|state| state.connected_clients.contains(&client_id), false)
    }
}

impl FollowStateWriter for FollowStateAdapter {
    fn connect_client(&self, client_id: ClientId) {
        self.with_write(|state| state.connect_client(client_id), ());
    }

    fn disconnect_client(&self, client_id: ClientId) -> Vec<Event> {
        self.with_write(|state| state.disconnect_client(client_id), Vec::new())
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
                    state.selected_contexts.insert(*client_id, None);
                    state.selected_sessions.insert(*client_id, None);
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
            },
            (),
        );
    }
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

        global_event_bus().register_channel::<ClientEvent>(clients_events::EVENT_KIND);
        Ok(bmux_plugin_sdk::EXIT_OK)
    }

    fn run_command(
        &mut self,
        _context: NativeCommandContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        Err(PluginCommandError::unknown_command(""))
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        bmux_plugin_sdk::route_service!(context, {
            "clients-state", "list-clients" => |_req: (), _ctx| {
                list_clients_local()
                    .map_err(|e| ServiceResponse::error("list_failed", e))
            },
            "clients-state", "current-client" => |_req: (), ctx| {
                Ok::<Result<ClientSummary, ClientQueryError>, ServiceResponse>(
                    current_client_local(ctx.caller_client_id)
                )
            },
            "clients-commands", "set-current-session" => |_req: SetCurrentSessionArgs, _ctx| {
                // Session selection is currently driven by `Request::Attach`
                // (which selects as a side-effect) rather than an explicit
                // set-current-session RPC. Wiring this into a dedicated
                // typed operation is tracked as a follow-up; today this
                // handler returns `Denied` so callers can't rely on it.
                Ok::<Result<ClientAck, SetCurrentSessionError>, ServiceResponse>(
                    Err(SetCurrentSessionError::Denied {
                        reason: "set-current-session is driven by Request::Attach today; \
                                 explicit typed operation is a follow-up"
                            .to_string(),
                    })
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

        let (Ok(read_cap), Ok(write_cap)) = (
            HostScope::new(bmux_clients_plugin_api::capabilities::CLIENTS_READ.as_str()),
            HostScope::new(bmux_clients_plugin_api::capabilities::CLIENTS_WRITE.as_str()),
        ) else {
            return;
        };

        let state: Arc<dyn ClientsStateService + Send + Sync> =
            Arc::new(ClientsStateHandle::new(Arc::clone(&caller)));
        registry.insert_typed::<dyn ClientsStateService + Send + Sync>(
            read_cap,
            ServiceKind::Query,
            clients_state::INTERFACE_ID,
            state,
        );

        let commands: Arc<dyn ClientsCommandsService + Send + Sync> =
            Arc::new(ClientsCommandsHandle::new(caller));
        registry.insert_typed::<dyn ClientsCommandsService + Send + Sync>(
            write_cap,
            ServiceKind::Command,
            clients_commands::INTERFACE_ID,
            commands,
        );
    }
}

// ── IPC helpers ──────────────────────────────────────────────────────

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
        .map(ipc_summary_to_typed)
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
        .map(ipc_summary_to_typed)
        .ok_or(ClientQueryError::NotFound)
}

#[allow(clippy::too_many_lines)]
fn set_following_via_ipc(
    caller: &impl ServiceCaller,
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

    // Emit event-bus events: generic FollowChanged for plugin
    // consumers, plus the wire-shape FollowStarted / FollowTargetChanged
    // events that the server bridges into `bmux_ipc::Event::*`.
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
    caller: &impl ServiceCaller,
    context_id: Uuid,
) -> Result<(), String> {
    #[derive(serde::Serialize)]
    struct Selector {
        id: Option<Uuid>,
        name: Option<String>,
    }
    #[derive(serde::Serialize)]
    struct Args {
        selector: Selector,
    }
    let _: serde_json::Value = caller
        .call_service(
            "bmux.contexts.write",
            ServiceKind::Command,
            "contexts-commands",
            "select-context",
            &Args {
                selector: Selector {
                    id: Some(context_id),
                    name: None,
                },
            },
        )
        .map_err(|err| err.to_string())?;
    Ok(())
}

fn reconcile_client_membership_via_typed_dispatch(
    caller: &impl ServiceCaller,
    client_id: Uuid,
    previous: Option<Uuid>,
    next: Option<Uuid>,
) -> Result<(), String> {
    #[derive(serde::Serialize)]
    struct Args {
        client_id: Uuid,
        previous: Option<Uuid>,
        next: Option<Uuid>,
    }
    let _: serde_json::Value = caller
        .call_service(
            "bmux.sessions.write",
            ServiceKind::Command,
            "sessions-commands",
            "reconcile-client-membership",
            &Args {
                client_id,
                previous,
                next,
            },
        )
        .map_err(|err| err.to_string())?;
    Ok(())
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
        _session_id: Uuid,
    ) -> Pin<
        Box<
            dyn Future<Output = std::result::Result<ClientAck, SetCurrentSessionError>> + Send + 'a,
        >,
    > {
        Box::pin(async move {
            Err(SetCurrentSessionError::Denied {
                reason: "set-current-session is not wired into the core runtime yet".to_string(),
            })
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
            let caller_client_id = match self.caller.call_service::<(), std::result::Result<
                ClientSummary,
                ClientQueryError,
            >>(
                bmux_clients_plugin_api::capabilities::CLIENTS_READ.as_str(),
                ServiceKind::Query,
                clients_state::INTERFACE_ID.as_str(),
                "current-client",
                &(),
            ) {
                Ok(Ok(summary)) => Some(summary.id),
                _ => None,
            };
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

const fn ipc_summary_to_typed(summary: &bmux_ipc::ClientSummary) -> ClientSummary {
    ClientSummary {
        id: summary.id,
        selected_session_id: summary.selected_session_id,
        selected_context_id: summary.selected_context_id,
        following_client_id: summary.following_client_id,
        following_global: summary.following_global,
    }
}

bmux_plugin_sdk::export_plugin!(ClientsPlugin, include_str!("../plugin.toml"));
