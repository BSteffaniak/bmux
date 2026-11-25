//! bmux clients plugin — typed owner of per-client identity and view state.
//!
//! Provides typed services that reach the server's client state
//! directly via the IPC kernel-bridge escape hatch
//! (`ServiceCaller::execute_kernel_request`).

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

/// Plugin declaration generated from the BPDL world and `plugin.toml`.
pub mod declaration {
    bmux_plugin_schema_macros::plugin_manifest! {
        package: {
            schema: ::bmux_clients_plugin_api::export_schema,
            crate_path: ::bmux_clients_plugin_api,
        },
        settings: "plugin.toml",
    }
}

mod cli;
pub mod follow_state;
mod identity;
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
use bmux_clients_plugin_api::clients_presence_commands_v1::{self, PresenceError};
use bmux_clients_plugin_api::clients_presence_state_v1::{self, Presence};
use bmux_clients_plugin_api::clients_selection_commands_v1;
use bmux_clients_plugin_api::clients_selection_state_v1::{Selection, SelectionError};
use bmux_clients_plugin_api::clients_state::{
    self, ClientQueryError, ClientSummary, ClientsStateService,
};
use bmux_contexts_plugin_api::contexts_commands;
use bmux_contexts_plugin_api::contexts_state::ContextSelector;
use bmux_plugin::{EventBus, PluginStateRegistry, ServiceCaller, TypedServiceCaller};
use bmux_plugin_sdk::author::{Declarations, Plugin};
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
    events: ClientEvents,
}

/// This activation's client event publisher. Without a host-supplied bus
/// (for example a dynamic artifact) events are dropped, never routed to a
/// process-wide bus.
#[derive(Clone)]
struct ClientEvents {
    bus: Option<Arc<EventBus>>,
    /// This activation's view of the host state registry; an empty private
    /// registry until activation supplies one, never a process-wide one.
    states: Arc<PluginStateRegistry>,
}

impl Default for ClientEvents {
    fn default() -> Self {
        Self::new(None, Arc::new(PluginStateRegistry::new()))
    }
}

impl ClientEvents {
    const fn new(bus: Option<Arc<EventBus>>, states: Arc<PluginStateRegistry>) -> Self {
        Self { bus, states }
    }

    fn emit(&self, event: ClientEvent) {
        if let Some(bus) = &self.bus {
            let _ = bus.emit(&clients_events::EVENT_KIND, event);
        }
    }
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
            self.events.emit(ClientEvent::FollowTargetGone {
                follower_client_id: update.follower_client_id.0,
                former_leader_client_id: update.former_leader_client_id.0,
            });
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
                    state.set_selected_target(client_id, None, None);
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
                let clients: Vec<_> = state.connected_clients.iter().copied().collect();
                for client in clients {
                    state.set_selected_target(client, None, None);
                }
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
                let restored_clients = snapshot
                    .connected_clients
                    .iter()
                    .copied()
                    .collect::<Vec<_>>();
                // Fence reservations before replacing their underlying target.
                for client in &restored_clients {
                    let revision = state.selection_revisions.entry(*client).or_default();
                    *revision = revision.saturating_add(1);
                }
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
pub struct ClientsPlugin {
    events: std::sync::RwLock<ClientEvents>,
}

fn slot_mut<T>(
    slot: &std::sync::RwLock<T>,
) -> std::result::Result<std::sync::RwLockWriteGuard<'_, T>, bmux_plugin_sdk::PluginCommandError> {
    slot.write()
        .map_err(|_| bmux_plugin_sdk::PluginCommandError::failed("plugin state lock poisoned"))
}

impl ClientsPlugin {
    /// An instance bound to an explicit host state registry, as activation
    /// would bind it.
    #[cfg(test)]
    const fn with_states(states: Arc<PluginStateRegistry>) -> Self {
        Self {
            events: std::sync::RwLock::new(ClientEvents::new(None, states)),
        }
    }

    /// `set-following` for an attachment's command (key binding or palette,
    /// run by the server's instance) against this instance's own follow
    /// state, or `None` for a standalone CLI invocation, which dispatches.
    fn set_following_here(
        &self,
        ctx: &NativeCommandContext,
        target: Option<Uuid>,
        global: bool,
    ) -> Option<Result<ClientAck, SetFollowingError>> {
        if ctx.invocation_source.is_cli() {
            return None;
        }
        let events = self.events();
        events.states.get::<FollowState>()?;
        Some(set_following_via_ipc(
            &events,
            ctx,
            ctx.caller_client_id,
            &SetFollowingArgs {
                target_client_id: target,
                global,
            },
        ))
    }

    fn events(&self) -> ClientEvents {
        self.events
            .read()
            .map(|events| events.clone())
            .unwrap_or_default()
    }
}

impl Plugin for ClientsPlugin {
    fn declare(d: &mut Declarations<Self>) {
        d.blocking_activate(Self::activate)
            .blocking_command("list-clients", |_, ctx| cli::list(ctx))
            .blocking_command("follow", |plugin, ctx| {
                cli::follow(ctx, &|target, global| {
                    plugin.set_following_here(ctx, target, global)
                })
            })
            .blocking_command("unfollow", |plugin, ctx| {
                cli::unfollow(ctx, &|target, global| {
                    plugin.set_following_here(ctx, target, global)
                })
            })
            .blocking_services(Self::invoke_service, None)
            .typed_services(|plugin, context, registry| {
                plugin.register_typed_services(&context, registry);
                Ok(())
            });
    }
}

impl ClientsPlugin {
    fn activate(
        &self,
        context: &NativeLifecycleContext,
        tasks: Option<bmux_plugin_sdk::HostAsyncHandle>,
    ) -> std::result::Result<(), PluginCommandError> {
        *slot_mut(&self.events)? = ClientEvents::new(
            bmux_plugin::activation_event_bus(context),
            bmux_plugin::activation_state_registry(context),
        );
        let registry = Arc::clone(&self.events().states);
        // Register the follow-state handle on this activation's view of the
        // host registry; core server code and other plugins read it there.
        // Re-activation is a no-op: only the first `register` call
        // installs the handle; subsequent calls replace it (which is
        // fine because we create a fresh default state in each case
        // and there is exactly one bundled clients plugin per host).
        let state: Arc<RwLock<FollowState>> = Arc::new(RwLock::new(FollowState::default()));
        registry.register::<FollowState>(&state);

        // Register the trait-object handle so consumers can reach
        // follow state through the domain-agnostic reader/writer
        // surface without naming the concrete plugin-owned type.
        let adapter = FollowStateAdapter {
            inner: Arc::clone(&state),
            events: self.events(),
        };
        let handle = Arc::new(RwLock::new(FollowStateHandle::new(adapter)));
        registry.register::<FollowStateHandle>(&handle);

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
        let stateful_registry = bmux_snapshot_runtime::get_or_init_stateful_registry(
            || registry.get::<StatefulPluginRegistry>(),
            |fresh| {
                registry.register_communal::<StatefulPluginRegistry>(fresh);
            },
        );
        stateful_registry
            .write()
            .expect("stateful plugin registry lock poisoned")
            .push(stateful);

        if let Some(bus) = &self.events().bus {
            bus.register_channel::<ClientEvent>(clients_events::EVENT_KIND);
            if let Some(tasks) = tasks {
                spawn_follow_observer(
                    &tasks,
                    Arc::clone(bus),
                    self.events(),
                    Arc::new(TypedServiceCaller::from_lifecycle_context(context)),
                )?;
            }
        }
        Ok(())
    }

    fn invoke_service(&self, context: &NativeServiceContext) -> ServiceResponse {
        bmux_plugin_sdk::route_service!(context, {
            "clients-selection-state-v1", "current" => |_req: (), ctx| {
                Ok::<Result<Selection, SelectionError>, ServiceResponse>(selection_operation(&self.events(), ctx.caller_client_id, |state, client| state.selection(client)))
            },
            "clients-selection-commands-v1", "recover" => |req: clients_selection_commands_v1::client::RecoverRequest, ctx| {
                Ok::<Result<Selection, SelectionError>, ServiceResponse>(selection_operation(&self.events(), ctx.caller_client_id, |state, client| state.recover_selection(client, req.expected_revision)))
            },
            "clients-selection-commands-v1", "begin" => |req: clients_selection_commands_v1::client::BeginRequest, ctx| {
                Ok::<Result<Selection, SelectionError>, ServiceResponse>(selection_operation(&self.events(), ctx.caller_client_id, |state, client| state.begin_selection(client, req.expected_revision)))
            },
            "clients-selection-commands-v1", "commit" => |req: clients_selection_commands_v1::client::CommitRequest, ctx| {
                Ok::<Result<Selection, SelectionError>, ServiceResponse>(commit_client_selection(&self.events(), ctx, ctx.caller_client_id, &req))
            },
            "clients-identity/v1", "stream-admission-backend" => |_req: (), ctx| {
                Ok::<_, ServiceResponse>(identity::stream_admission_backend(ctx))
            },
            "clients-identity/v1", "current-stream-provenance" => |_req: (), ctx| {
                Ok::<_, ServiceResponse>(identity::current_stream_provenance(ctx))
            },
            "clients-identity/v1", "current-principal" => |_req: (), ctx| {
                let identity = identity::CallerIdentity::new(
                    Arc::clone(&self.events().states),
                    ctx.caller_client_id,
                );
                Ok::<_, ServiceResponse>(bmux_plugin::block_on_typed_dispatch(
                    bmux_clients_plugin_api::clients_identity::ClientsIdentityService::current_principal(&identity),
                ))
            },
            "clients-presence-state-v1", "list-presence" => |_req: (), _ctx| {
                Ok::<Vec<Presence>, ServiceResponse>(list_presence(&self.events()))
            },
            "clients-presence-commands-v1", "set-terminal-focus" => |req: clients_presence_commands_v1::client::SetTerminalFocusRequest, ctx| {
                Ok::<Result<(), PresenceError>, ServiceResponse>(set_terminal_focus(&self.events(), ctx.caller_client_id, req.focused))
            },
            "clients-state", "list-clients" => |_req: (), _ctx| {
                list_clients_local(&self.events())
                    .map_err(|e| ServiceResponse::error("list_failed", e))
            },
            "clients-state", "current-client" => |_req: (), ctx| {
                Ok::<Result<ClientSummary, ClientQueryError>, ServiceResponse>(
                    current_client_local(&self.events(), ctx.caller_client_id)
                )
            },
            "clients-commands", "set-current-session" => |req: SetCurrentSessionArgs, ctx| {
                Ok::<Result<ClientAck, SetCurrentSessionError>, ServiceResponse>(
                    set_current_session_local(&self.events(), ctx, ctx.caller_client_id, req.session_id)
                )
            },
            "clients-commands", "set-following" => |req: SetFollowingArgs, ctx| {
                Ok::<Result<ClientAck, SetFollowingError>, ServiceResponse>(
                    set_following_via_ipc(&self.events(), ctx, ctx.caller_client_id, &req)
                )
            },
        })
    }

    fn register_typed_services(
        &self,
        context: &TypedServiceRegistrationContext<'_>,
        registry: &mut TypedServiceRegistry,
    ) {
        let caller = Arc::new(TypedServiceCaller::from_registration_context(context));

        let state: Arc<dyn ClientsStateService + Send + Sync> =
            Arc::new(ClientsStateHandle::new(Arc::clone(&caller)));
        let _ = clients_state::register_provider(registry, state);

        let selection = Arc::new(SelectionHandle {
            caller: caller.clone(),
            events: self.events(),
        });
        let _ = bmux_clients_plugin_api::clients_selection_state_v1::register_provider(
            registry,
            selection.clone(),
        );
        let _ = clients_selection_commands_v1::register_provider(registry, selection.clone());
        let _ = clients_presence_state_v1::register_provider(registry, selection.clone());
        let _ = clients_presence_commands_v1::register_provider(registry, selection);
        let commands: Arc<dyn ClientsCommandsService + Send + Sync> =
            Arc::new(ClientsCommandsHandle::new(caller, self.events()));
        let _ = clients_commands::register_provider(registry, commands);
    }
}

struct SelectionHandle {
    caller: Arc<TypedServiceCaller>,
    events: ClientEvents,
}

impl bmux_clients_plugin_api::clients_selection_state_v1::ClientsSelectionStateV1Service
    for SelectionHandle
{
    fn current<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Selection, SelectionError>> + Send + 'a>> {
        Box::pin(async move {
            selection_operation(
                &self.events,
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
                &self.events,
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
                &self.events,
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
                &self.events,
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

impl clients_presence_state_v1::ClientsPresenceStateV1Service for SelectionHandle {
    fn list_presence<'a>(&'a self) -> Pin<Box<dyn Future<Output = Vec<Presence>> + Send + 'a>> {
        Box::pin(async move { list_presence(&self.events) })
    }
}

impl clients_presence_commands_v1::ClientsPresenceCommandsV1Service for SelectionHandle {
    fn set_terminal_focus<'a>(
        &'a self,
        focused: bool,
    ) -> Pin<Box<dyn Future<Output = Result<(), PresenceError>> + Send + 'a>> {
        Box::pin(async move {
            set_terminal_focus(
                &self.events,
                current_client_id_for_typed_handle(&self.caller),
                focused,
            )
        })
    }
}

/// What each connected client shows, and whether its terminal has focus.
fn list_presence(events: &ClientEvents) -> Vec<Presence> {
    let Some(state) = events.states.get::<FollowState>() else {
        return Vec::new();
    };
    let Ok(state) = state.read() else {
        return Vec::new();
    };
    state
        .connected_clients
        .iter()
        .map(|client| Presence {
            client_id: client.0,
            selected_context_id: state.selected_contexts.get(client).copied().flatten(),
            terminal_focused: !state.unfocused_terminals.contains(client),
        })
        .collect()
}

/// Record whether the calling client's terminal has focus.
fn set_terminal_focus(
    events: &ClientEvents,
    client: Option<Uuid>,
    focused: bool,
) -> Result<(), PresenceError> {
    let client = ClientId(client.ok_or(PresenceError::NoCurrentClient)?);
    let state = events
        .states
        .get::<FollowState>()
        .ok_or_else(|| PresenceError::Failed {
            reason: "clients state unavailable".into(),
        })?;
    let mut state = state.write().map_err(|_| PresenceError::Failed {
        reason: "clients state poisoned".into(),
    })?;
    if focused {
        state.unfocused_terminals.remove(&client);
    } else if state.connected_clients.contains(&client) {
        state.unfocused_terminals.insert(client);
    }
    drop(state);
    Ok(())
}

fn commit_client_selection(
    events: &ClientEvents,
    caller: &(impl ServiceCaller + Sync),
    client: Option<Uuid>,
    req: &clients_selection_commands_v1::client::CommitRequest,
) -> Result<Selection, SelectionError> {
    let client_id = client.ok_or(SelectionError::NoCurrentClient)?;
    if let Some(session) = req.session_id {
        authorize_selection(events, caller, client_id, req.context_id, session)?;
        let manager = events
            .states
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
        let handle = events
            .states
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
    selection_operation(events, client, |state, client| {
        state.commit_selection(
            client,
            req.expected_revision,
            req.context_id,
            req.session_id,
        )
    })
}

fn authorize_selection(
    events: &ClientEvents,
    caller: &(impl ServiceCaller + Sync),
    client: Uuid,
    context: Option<Uuid>,
    session: Uuid,
) -> Result<(), SelectionError> {
    use bmux_plugin_sdk::TypedServiceEndpoint;
    use bmux_plugin_sdk::typed_dispatch::{SelectedServiceAvailability, ServiceAvailabilityKey};

    let descriptor =
        bmux_permissions_plugin_api::session_policy_state::client::CheckEndpoint::service_descriptor();
    let availability = bmux_plugin::block_on_typed_dispatch(caller.selected_service_availability(
        ServiceAvailabilityKey {
            capability: descriptor.capability.to_string(),
            kind: descriptor.kind,
            interface_id: descriptor.interface_id.to_string(),
        },
    ))
    .map_err(|error| SelectionError::Failed {
        reason: format!("selection policy availability failed: {error}"),
    })?;
    if availability == SelectedServiceAvailability::Absent {
        // Absence is not locality: only independently authenticated local input
        // retains the optional permissions plugin's single-user fallback.
        return match caller.current_stream_origin() {
            Ok(bmux_plugin::StreamOrigin::Local) => Ok(()),
            Ok(_) => Err(SelectionError::Failed {
                reason: "selection policy absent for non-local caller".into(),
            }),
            Err(error) => Err(SelectionError::Failed {
                reason: format!("selection caller origin unavailable: {error}"),
            }),
        };
    }
    let principal = events
        .states
        .get::<bmux_client_state::ClientPrincipalHandle>()
        .and_then(|handle| handle.read().ok().map(|value| value.clone()))
        .and_then(|handle| handle.0.get(ClientId(client)))
        .unwrap_or_else(Uuid::nil);
    let mut dispatch = dispatch_client(caller);
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
    events: &ClientEvents,
    client: Option<Uuid>,
    operation: impl FnOnce(&mut FollowState, ClientId) -> Result<Selection, SelectionError>,
) -> Result<Selection, SelectionError> {
    let client = ClientId(client.ok_or(SelectionError::NoCurrentClient)?);
    let state = events
        .states
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
    events: &ClientEvents,
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

    let Some(sessions) = events.states.get::<SessionManagerHandle>() else {
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

    let Some(state_handle) = events.states.get::<FollowState>() else {
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
        events,
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

    events.emit(ClientEvent::SessionSelected {
        client_id: self_id,
        session_id,
    });

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

            events.emit(ClientEvent::FollowTargetChanged {
                follower_client_id: update.follower_client_id.0,
                leader_client_id: update.leader_client_id.0,
                context_id: update.context_id,
                session_id: update_session.0,
            });
        }
    }

    Ok(ClientAck { client_id: self_id })
}

fn list_clients_local(events: &ClientEvents) -> Result<Vec<ClientSummary>, String> {
    let Some(state) = events.states.get::<FollowState>() else {
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

fn current_client_local(
    events: &ClientEvents,
    caller_client_id: Option<Uuid>,
) -> Result<ClientSummary, ClientQueryError> {
    let Some(self_id) = caller_client_id else {
        return Err(ClientQueryError::NoCurrentClient);
    };
    let Some(state) = events.states.get::<FollowState>() else {
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
    events: &ClientEvents,
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
    let Some(state_handle) = events.states.get::<FollowState>() else {
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
            events.emit(ClientEvent::FollowChanged {
                client_id: self_id,
                target_client_id: None,
                global: false,
            });
            events.emit(ClientEvent::FollowStopped {
                follower_client_id: self_id,
            });
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
    // The leader's tab is its contexts selection, which tab switches update;
    // the follow state's record of it is written only on session changes and
    // can be stale. Prefer the selection authority when it has one.
    let (initial_target_context, initial_target_session) = if req.global {
        leader_selection(events, leader_client_id)
            .unwrap_or((initial_target_context, initial_target_session))
    } else {
        (initial_target_context, initial_target_session)
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
    events.emit(ClientEvent::FollowChanged {
        client_id: self_id,
        target_client_id: Some(target_client_id),
        global: req.global,
    });
    events.emit(ClientEvent::FollowStarted {
        follower_client_id: self_id,
        leader_client_id: target_client_id,
        global: req.global,
    });
    if let Some(session_id) = initial_target_session {
        events.emit(ClientEvent::FollowTargetChanged {
            follower_client_id: self_id,
            leader_client_id: target_client_id,
            context_id: initial_target_context,
            session_id: session_id.0,
        });
    }

    Ok(ClientAck { client_id: self_id })
}

/// The leader's current tab and its session, from the contexts selection
/// authority's published reader, when it has a selection.
/// Keep global followers on their leader's tab. Contexts announces every
/// change of a client's current tab as `session-active-context-changed`
/// naming that client as initiator (its own tab switch, or a plugin
/// selecting for it). When the client leads global followers, each of them
/// is selected onto the same tab for itself, so its attach retargets exactly
/// as for its own selection. Follow state alone changes only through
/// `set-current-session`, which tab switches never call.
fn spawn_follow_observer(
    tasks: &bmux_plugin_sdk::HostAsyncHandle,
    bus: Arc<EventBus>,
    events: ClientEvents,
    caller: Arc<TypedServiceCaller>,
) -> std::result::Result<(), PluginCommandError> {
    use bmux_contexts_plugin_api::contexts_events::{self, ContextEvent};
    use tokio::sync::broadcast::error::RecvError;
    let worker = tasks.clone();
    tasks
        .spawn_background(
            "clients-follow-observer",
            move |mut cancellation| async move {
                let kind = contexts_events::EVENT_KIND;
                let mut receiver = tokio::select! {
                    () = cancellation.cancelled() => return Ok(()),
                    ready = bus.subscribe_when_registered::<ContextEvent>(&kind) => {
                        ready.map_err(|error| error.to_string())?
                    }
                };
                loop {
                    let event = tokio::select! {
                        () = cancellation.cancelled() => return Ok(()),
                        event = receiver.recv() => event,
                    };
                    let event = match event {
                        Ok(event) => event,
                        Err(RecvError::Lagged(_)) => continue,
                        Err(RecvError::Closed) => return Ok(()),
                    };
                    let ContextEvent::SessionActiveContextChanged {
                        session_id,
                        context_id,
                        initiator_client_id: Some(leader),
                    } = *event
                    else {
                        continue;
                    };
                    let events = events.clone();
                    let caller = Arc::clone(&caller);
                    worker
                        .spawn_blocking(move || {
                            follow_leader(
                                &events,
                                caller.as_ref(),
                                ClientId(leader),
                                context_id,
                                SessionId(session_id),
                            );
                        })
                        .await
                        .map_err(|error| error.to_string())?;
                }
            },
        )
        .map_err(PluginCommandError::failed)
}

/// Move every global follower of `leader` to the leader's new tab.
fn follow_leader(
    events: &ClientEvents,
    caller: &(impl ServiceCaller + Sync),
    leader: ClientId,
    context_id: Uuid,
    session_id: SessionId,
) {
    let Some(state) = events.states.get::<FollowState>() else {
        return;
    };
    let (previous_sessions, updates) = {
        let Ok(mut state) = state.write() else {
            return;
        };
        let previous_sessions: Vec<(ClientId, Option<SessionId>)> = state
            .follows
            .iter()
            .filter(|(_, entry)| entry.leader_client_id == leader && entry.global)
            .map(|(follower, _)| {
                (
                    *follower,
                    state.selected_sessions.get(follower).copied().flatten(),
                )
            })
            .collect();
        if previous_sessions.is_empty() {
            return;
        }
        let updates = state.sync_followers_from_leader(leader, Some(context_id), Some(session_id));
        (previous_sessions, updates)
    };
    for update in updates {
        let follower = update.follower_client_id;
        if let Err(error) = select_for_client_via_typed_dispatch(caller, follower.0, context_id) {
            let _ = bmux_plugin::HostRuntimeApi::log_write(
                caller,
                &bmux_plugin_sdk::LogWriteRequest {
                    level: bmux_plugin_sdk::LogWriteLevel::Warn,
                    message: format!(
                        "follower {} could not follow its leader to tab {context_id}: {error}",
                        follower.0
                    ),
                    target: Some("bmux.clients.follow".to_string()),
                },
            );
            continue;
        }
        let previous = previous_sessions
            .iter()
            .find_map(|(client, previous)| (*client == follower).then_some(*previous))
            .flatten();
        if previous != Some(session_id) {
            let _ = reconcile_client_membership_via_typed_dispatch(
                caller,
                follower.0,
                previous.map(|session| session.0),
                Some(session_id.0),
            );
        }
        events.emit(ClientEvent::FollowTargetChanged {
            follower_client_id: follower.0,
            leader_client_id: leader.0,
            context_id: Some(context_id),
            session_id: session_id.0,
        });
    }
}

fn select_for_client_via_typed_dispatch(
    caller: &(impl ServiceCaller + Sync),
    client_id: Uuid,
    context_id: Uuid,
) -> Result<(), String> {
    let mut client = dispatch_client(caller);
    bmux_plugin::block_on_typed_dispatch(
        bmux_contexts_plugin_api::contexts_client_selection_v1::client::select_for_client(
            &mut client,
            client_id,
            context_id,
        ),
    )
    .map_err(|error| error.to_string())?
    .map(|_| ())
    .map_err(|error| format!("{error:?}"))
}

fn leader_selection(
    events: &ClientEvents,
    leader: ClientId,
) -> Option<(Option<Uuid>, Option<SessionId>)> {
    let handle = events
        .states
        .get::<bmux_context_state::ContextStateHandle>()?
        .read()
        .ok()?
        .clone();
    let context = handle.0.current_for_client(leader)?;
    Some((
        Some(context.id),
        handle.0.current_session_for_client(leader),
    ))
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
                .unwrap_or(Err(ClientQueryError::NoCurrentClient))
        })
    }
}

pub struct ClientsCommandsHandle {
    caller: Arc<TypedServiceCaller>,
    events: ClientEvents,
}

impl ClientsCommandsHandle {
    const fn new(caller: Arc<TypedServiceCaller>, events: ClientEvents) -> Self {
        Self { caller, events }
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
            set_current_session_local(
                &self.events,
                self.caller.as_ref(),
                caller_client_id,
                session_id,
            )
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
                &self.events,
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

bmux_plugin_sdk::plugin!(ClientsPlugin);

/// Request policy for this implementation's trusted application composition.
///
/// This function does not issue authority. The application must retain it against
/// the verified selected artifact and exact loaded instance, then run it inside
/// the admitted invocation before executing the request. Manifests and dynamic
/// plugin exports cannot select this policy or approve their own replacement.
/// Caller provenance is supplied by the host, never by the encoded request.
///
/// # Errors
/// Rejects malformed IPC and operations outside this implementation's needs.
// The composition callback contract uses the SDK's rich error type by value.
#[allow(clippy::result_large_err)]
pub fn authorize_kernel_request(encoded: &[u8]) -> bmux_plugin_sdk::Result<()> {
    let request: bmux_ipc::Request = bmux_ipc::decode(encoded).map_err(|error| {
        bmux_plugin_sdk::PluginError::ServiceProtocol {
            details: error.to_string(),
        }
    })?;
    if matches!(
        request,
        bmux_ipc::Request::CurrentStreamProvenance | bmux_ipc::Request::StreamAdmissionBackend
    ) {
        Ok(())
    } else {
        Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
            operation: "clients_kernel_request_policy",
        })
    }
}

#[cfg(test)]
mod kernel_request_policy_tests {
    #[test]
    fn composition_policy_is_narrow_and_rejects_invalid_requests() {
        let provenance = bmux_ipc::encode(&bmux_ipc::Request::CurrentStreamProvenance).unwrap();
        assert!(super::authorize_kernel_request(&provenance).is_ok());
        let backend = bmux_ipc::encode(&bmux_ipc::Request::StreamAdmissionBackend).unwrap();
        assert!(super::authorize_kernel_request(&backend).is_ok());
        let unrelated = bmux_ipc::encode(&bmux_ipc::Request::Ping).unwrap();
        assert!(super::authorize_kernel_request(&unrelated).is_err());
        assert!(super::authorize_kernel_request(b"invalid IPC").is_err());
    }
}

#[cfg(test)]
mod definition_contract_tests {
    #[test]
    fn explicit_contract_versions_preserve_service_wire_routes() {
        let definition = super::plugin_definition();
        let manifest =
            bmux_plugin::PluginManifest::from_toml_str(crate::declaration::PLUGIN_MANIFEST)
                .unwrap();
        definition.validate().unwrap();
        assert_eq!(definition.metadata.plugin_id, manifest.id);
        assert_eq!(definition.metadata.version, manifest.version);
        assert_eq!(definition.metadata.roles, manifest.component_roles);
        let declared: std::collections::BTreeSet<_> = manifest
            .contracts
            .iter()
            .map(|contract| (&contract.interface_id, contract.major, contract.minor))
            .collect();
        let embedded: std::collections::BTreeSet<_> = definition
            .metadata
            .contracts
            .iter()
            .map(|contract| (&contract.interface_id, contract.major, contract.minor))
            .collect();
        assert_eq!(embedded, declared);
        let services = definition.declarations().unwrap().services;
        for service in services {
            let contract = definition
                .metadata
                .contracts
                .iter()
                .find(|contract| contract.interface_id == service.interface_id)
                .expect("every retained route requires explicit compatibility metadata");
            assert_eq!((contract.major, contract.minor), (1, 0));
        }
    }

    #[test]
    fn embedded_declaration_carries_generated_reentrant_query_without_duplicate_route() {
        let definition = super::plugin_definition();
        let manifest = bmux_plugin::PluginManifest::from_toml_str(
            definition
                .metadata
                .manifest_toml
                .as_deref()
                .expect("definition embeds its manifest"),
        )
        .unwrap();
        let declaration = manifest.to_declaration().unwrap();
        let identity: Vec<_> = declaration
            .services
            .iter()
            .filter(|service| service.interface_id == "clients-identity/v1")
            .collect();
        // The explicit manifest route and the generated declaration merge
        // into one endpoint that keeps the BPDL reentrant set.
        assert_eq!(identity.len(), 1);
        assert!(identity[0].is_reentrant_query("current-stream-provenance"));
        assert!(!identity[0].is_reentrant_query("current-principal"));
    }
}

#[cfg(test)]
mod presence_tests {
    use super::*;

    fn events_with(clients: &[Uuid]) -> ClientEvents {
        let states = Arc::new(PluginStateRegistry::new());
        let mut follow = FollowState::default();
        for client in clients {
            follow.connect_client(ClientId(*client));
            follow
                .selected_contexts
                .insert(ClientId(*client), Some(*client));
        }
        let state = Arc::new(RwLock::new(follow));
        states.register::<FollowState>(&state);
        ClientEvents::new(None, states)
    }

    fn focused(events: &ClientEvents) -> Vec<(Uuid, bool)> {
        list_presence(events)
            .into_iter()
            .map(|presence| (presence.client_id, presence.terminal_focused))
            .collect()
    }

    /// A client's terminal counts as focused until it reports otherwise;
    /// each client reports only its own, and disconnecting forgets it.
    #[test]
    fn terminal_focus_is_reported_per_client() {
        let (a, b) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let events = events_with(&[a, b]);
        assert_eq!(focused(&events), [(a, true), (b, true)]);
        assert_eq!(
            list_presence(&events)[0].selected_context_id,
            Some(a),
            "with what it shows"
        );
        set_terminal_focus(&events, Some(a), false).unwrap();
        assert_eq!(focused(&events), [(a, false), (b, true)]);
        set_terminal_focus(&events, Some(a), true).unwrap();
        assert_eq!(focused(&events), [(a, true), (b, true)]);
        set_terminal_focus(&events, Some(b), false).unwrap();
        let state = events.states.get::<FollowState>().unwrap();
        state.write().unwrap().disconnect_client(ClientId(b));
        assert_eq!(focused(&events), [(a, true)]);
        assert!(state.read().unwrap().unfocused_terminals.is_empty());
        assert_eq!(
            set_terminal_focus(&events, None, false),
            Err(PresenceError::NoCurrentClient)
        );
    }
}
