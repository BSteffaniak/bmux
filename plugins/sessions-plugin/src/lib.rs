//! bmux sessions plugin — typed owner of session lifecycle.
//!
//! Provides typed services for other plugins and attach-side callers
//! to list, create, kill, and select sessions.
//!
//! The plugin's typed and byte-dispatch surfaces both reach the
//! server's session state directly via the IPC kernel-bridge escape
//! hatch (`ServiceCaller::execute_kernel_request`). This avoids a
//! cycle that would otherwise happen if the plugin's handlers tried
//! to reach their own state through the typed service layer.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

// `SessionManager` is owned and constructed by this plugin. Other
// plugins and the server reach it through the domain-agnostic
// `bmux_session_state::SessionManagerHandle` trait object registered
// into the plugin state registry during `activate`.
pub mod session_manager;
pub use session_manager::SessionManager;

use bmux_plugin::{
    ServiceCaller, TypedServiceCaller, global_event_bus, global_plugin_state_registry,
};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{
    HostScope, PluginEventKind, StatefulPlugin, StatefulPluginError, StatefulPluginHandle,
    StatefulPluginResult, StatefulPluginSnapshot, TypedServiceRegistrationContext,
    TypedServiceRegistry,
};
use bmux_session_models::{ClientId, Session, SessionId, SessionInfo};
use bmux_session_state::{
    SessionManagerHandle, SessionManagerReader, SessionManagerSnapshot, SessionManagerWriter,
};
use bmux_sessions_plugin_api::sessions_commands::{
    self, KillSessionError, NewSessionError, ReconcileError, SelectSessionError, SessionAck,
    SessionSelector as CommandSessionSelector, SessionsCommandsService,
};
use bmux_sessions_plugin_api::sessions_events::{self, SessionEvent};
use bmux_sessions_plugin_api::sessions_state::{
    self, SessionQueryError, SessionSelector as StateSessionSelector, SessionSummary,
    SessionsStateService,
};
use bmux_snapshot_runtime::StatefulPluginRegistry;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

/// Adapter wrapping the plugin's `Arc<RwLock<SessionManager>>` and
/// implementing the domain-agnostic [`SessionManagerReader`] +
/// [`SessionManagerWriter`] traits from `bmux_session_state`.
///
/// Registered as a [`SessionManagerHandle`] in the plugin state
/// registry alongside the concrete `Arc<RwLock<SessionManager>>` so
/// consumers can reach session-manager state through the trait
/// surface without naming the concrete plugin-owned type.
struct SessionManagerAdapter {
    inner: Arc<RwLock<SessionManager>>,
}

impl SessionManagerAdapter {
    fn with_read<T>(&self, f: impl FnOnce(&SessionManager) -> T, fallback: T) -> T {
        self.inner.read().map_or(fallback, |guard| f(&guard))
    }

    fn with_write<T>(&self, f: impl FnOnce(&mut SessionManager) -> T, fallback: T) -> T {
        self.inner
            .write()
            .map_or(fallback, |mut guard| f(&mut guard))
    }
}

impl SessionManagerReader for SessionManagerAdapter {
    fn list_sessions(&self) -> Vec<SessionInfo> {
        self.with_read(SessionManager::list_sessions, Vec::<SessionInfo>::new())
    }

    fn get_session(&self, session_id: SessionId) -> Option<Session> {
        self.with_read(|mgr| mgr.get_session(&session_id).cloned(), None)
    }

    fn contains(&self, session_id: SessionId) -> bool {
        self.with_read(|mgr| mgr.get_session(&session_id).is_some(), false)
    }
}

impl SessionManagerWriter for SessionManagerAdapter {
    fn create_session(&self, name: Option<String>) -> anyhow::Result<SessionId> {
        self.inner
            .write()
            .map_err(|_| anyhow::anyhow!("session-manager lock poisoned"))?
            .create_session(name)
    }

    fn insert_session(&self, session: Session) -> anyhow::Result<()> {
        self.inner
            .write()
            .map_err(|_| anyhow::anyhow!("session-manager lock poisoned"))?
            .insert_session(session)
    }

    fn remove_session(&self, session_id: SessionId) -> anyhow::Result<()> {
        self.inner
            .write()
            .map_err(|_| anyhow::anyhow!("session-manager lock poisoned"))?
            .remove_session(&session_id)
    }

    fn add_client(&self, session_id: SessionId, client_id: ClientId) {
        self.with_write(
            |mgr| {
                if let Some(session) = mgr.get_session_mut(&session_id) {
                    session.add_client(client_id);
                }
            },
            (),
        );
    }

    fn remove_client(&self, session_id: SessionId, client_id: &ClientId) {
        self.with_write(
            |mgr| {
                if let Some(session) = mgr.get_session_mut(&session_id) {
                    session.remove_client(client_id);
                }
            },
            (),
        );
    }

    fn snapshot(&self) -> SessionManagerSnapshot {
        self.with_read(
            |mgr| {
                let sessions = mgr
                    .list_sessions()
                    .into_iter()
                    .filter_map(|info| mgr.get_session(&info.id).cloned())
                    .collect();
                SessionManagerSnapshot(sessions)
            },
            SessionManagerSnapshot::default(),
        )
    }

    fn restore_snapshot(&self, snapshot: SessionManagerSnapshot) {
        self.with_write(
            |mgr| {
                *mgr = SessionManager::new();
                for session in snapshot.0 {
                    let _ = mgr.insert_session(session);
                }
            },
            (),
        );
    }
}

// ── StatefulPlugin participant for persistence ─────────────────────

/// Stable id for the session-manager snapshot surface.
const SESSIONS_STATEFUL_ID: PluginEventKind =
    PluginEventKind::from_static("bmux.sessions/session-manager");

/// Current snapshot schema version for session-manager state.
const SESSIONS_STATEFUL_VERSION: u32 = 1;

/// Snapshot participant that serializes the plugin's [`SessionManager`]
/// via the domain-agnostic [`SessionManagerWriter::snapshot`] /
/// [`SessionManagerWriter::restore_snapshot`] hooks.
struct SessionsStatefulPlugin {
    writer: Arc<dyn SessionManagerWriter>,
}

impl StatefulPlugin for SessionsStatefulPlugin {
    fn id(&self) -> PluginEventKind {
        SESSIONS_STATEFUL_ID
    }

    fn snapshot(&self) -> StatefulPluginResult<StatefulPluginSnapshot> {
        let snap = self.writer.snapshot();
        let bytes =
            serde_json::to_vec(&snap).map_err(|err| StatefulPluginError::SnapshotFailed {
                plugin: SESSIONS_STATEFUL_ID.as_str().to_string(),
                details: err.to_string(),
            })?;
        Ok(StatefulPluginSnapshot::new(
            SESSIONS_STATEFUL_ID,
            SESSIONS_STATEFUL_VERSION,
            bytes,
        ))
    }

    fn restore_snapshot(&self, snapshot: StatefulPluginSnapshot) -> StatefulPluginResult<()> {
        if snapshot.version != SESSIONS_STATEFUL_VERSION {
            return Err(StatefulPluginError::UnsupportedVersion {
                plugin: SESSIONS_STATEFUL_ID.as_str().to_string(),
                version: snapshot.version,
                expected: vec![SESSIONS_STATEFUL_VERSION],
            });
        }
        let decoded: SessionManagerSnapshot =
            serde_json::from_slice(&snapshot.bytes).map_err(|err| {
                StatefulPluginError::RestoreFailed {
                    plugin: SESSIONS_STATEFUL_ID.as_str().to_string(),
                    details: err.to_string(),
                }
            })?;
        self.writer.restore_snapshot(decoded);
        Ok(())
    }
}

/// Wire-format argument for the typed `new-session` byte-dispatch call.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct NewSessionArgs {
    name: Option<String>,
}

/// Wire-format argument for the `kill-session` byte-dispatch call.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct KillSessionArgs {
    selector: WireSelector,
    force_local: bool,
}

/// Wire-format argument for selector-only calls.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SelectorArgs {
    selector: WireSelector,
}

/// Wire-format argument for `reconcile-client-membership`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReconcileArgs {
    client_id: ::uuid::Uuid,
    #[serde(default)]
    previous: Option<::uuid::Uuid>,
    #[serde(default)]
    next: Option<::uuid::Uuid>,
}

/// Wire-format selector.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireSelector {
    #[serde(default)]
    id: Option<::uuid::Uuid>,
    #[serde(default)]
    name: Option<String>,
}

impl WireSelector {
    fn to_ipc(&self) -> Option<bmux_ipc::SessionSelector> {
        if let Some(id) = self.id {
            return Some(bmux_ipc::SessionSelector::ById(id));
        }
        self.name
            .as_ref()
            .map(|name| bmux_ipc::SessionSelector::ByName(name.clone()))
    }
}

#[derive(Default)]
pub struct SessionsPlugin;

impl RustPlugin for SessionsPlugin {
    fn activate(
        &mut self,
        _context: NativeLifecycleContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        let state: Arc<RwLock<SessionManager>> = Arc::new(RwLock::new(SessionManager::new()));
        global_plugin_state_registry().register::<SessionManager>(&state);

        // Register the trait-object handle so consumers can reach
        // session-manager state through the domain-agnostic reader/writer
        // surface without naming the concrete plugin-owned type.
        let adapter = SessionManagerAdapter {
            inner: Arc::clone(&state),
        };
        let handle = Arc::new(RwLock::new(SessionManagerHandle::new(adapter)));
        global_plugin_state_registry().register::<SessionManagerHandle>(&handle);

        // Register this plugin as a persistence participant so the
        // snapshot-orchestration plugin can drive save/restore over
        // session-manager state on its schedule.
        let writer_for_snapshot: Arc<dyn SessionManagerWriter> = {
            let guard = handle
                .read()
                .expect("freshly-created SessionManagerHandle lock is poisoned");
            Arc::clone(&guard.0)
        };
        let stateful = StatefulPluginHandle::new(SessionsStatefulPlugin {
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

        global_event_bus().register_channel::<SessionEvent>(sessions_events::EVENT_KIND);
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
            "sessions-state", "list-sessions" => |_req: (), _ctx| {
                list_sessions_local()
                    .map_err(|e| ServiceResponse::error("list_failed", e))
            },
            "sessions-state", "get-session" => |req: SelectorArgs, _ctx| {
                get_session_local(&req.selector)
                    .map_err(|e| ServiceResponse::error("get_failed", e))
            },
            "sessions-commands", "new-session" => |req: NewSessionArgs, ctx| {
                Ok::<Result<SessionAck, NewSessionError>, ServiceResponse>(
                    new_session_via_ipc(ctx, req.name)
                )
            },
            "sessions-commands", "kill-session" => |req: KillSessionArgs, ctx| {
                Ok::<Result<SessionAck, KillSessionError>, ServiceResponse>(
                    kill_session_via_ipc(ctx, &req.selector, req.force_local)
                )
            },
            "sessions-commands", "select-session" => |req: SelectorArgs, ctx| {
                Ok::<Result<SessionAck, SelectSessionError>, ServiceResponse>(
                    select_session_via_ipc(ctx, &req.selector)
                )
            },
            "sessions-commands", "reconcile-client-membership" => |req: ReconcileArgs, _ctx| {
                Ok::<Result<SessionAck, ReconcileError>, ServiceResponse>(
                    reconcile_client_membership_local(&req)
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
            HostScope::new(bmux_sessions_plugin_api::capabilities::SESSIONS_READ.as_str()),
            HostScope::new(bmux_sessions_plugin_api::capabilities::SESSIONS_WRITE.as_str()),
        ) else {
            return;
        };

        let state: Arc<dyn SessionsStateService + Send + Sync> =
            Arc::new(SessionsStateHandle::new(Arc::clone(&caller)));
        registry.insert_typed::<dyn SessionsStateService + Send + Sync>(
            read_cap,
            ServiceKind::Query,
            sessions_state::INTERFACE_ID,
            state,
        );

        let commands: Arc<dyn SessionsCommandsService + Send + Sync> =
            Arc::new(SessionsCommandsHandle::new(caller));
        registry.insert_typed::<dyn SessionsCommandsService + Send + Sync>(
            write_cap,
            ServiceKind::Command,
            sessions_commands::INTERFACE_ID,
            commands,
        );
    }
}

// ── State-local helpers ──────────────────────────────────────────────

fn list_sessions_local() -> Result<Vec<SessionSummary>, String> {
    let Some(state) = global_plugin_state_registry().get::<SessionManager>() else {
        return Err("sessions plugin state not registered".to_string());
    };
    let manager = state
        .read()
        .map_err(|_| "session manager lock poisoned".to_string())?;
    Ok(manager
        .list_sessions()
        .into_iter()
        .map(session_info_to_typed)
        .collect())
}

fn get_session_local(
    selector: &WireSelector,
) -> Result<Result<SessionSummary, SessionQueryError>, String> {
    let Some(ipc_selector) = selector.to_ipc() else {
        return Ok(Err(SessionQueryError::InvalidSelector {
            reason: "selector must specify either id or name".to_string(),
        }));
    };
    let Some(state) = global_plugin_state_registry().get::<SessionManager>() else {
        return Err("sessions plugin state not registered".to_string());
    };
    let manager = state
        .read()
        .map_err(|_| "session manager lock poisoned".to_string())?;
    Ok(manager
        .list_sessions()
        .into_iter()
        .find(|info| matches_session_info(info, &ipc_selector))
        .map(session_info_to_typed)
        .ok_or(SessionQueryError::NotFound))
}

fn matches_session_info(
    info: &bmux_session_models::SessionInfo,
    selector: &bmux_ipc::SessionSelector,
) -> bool {
    match selector {
        bmux_ipc::SessionSelector::ById(id) => info.id.0 == *id,
        bmux_ipc::SessionSelector::ByName(name) => info.name.as_deref() == Some(name.as_str()),
    }
}

fn session_info_to_typed(info: bmux_session_models::SessionInfo) -> SessionSummary {
    SessionSummary {
        id: info.id.0,
        name: info.name,
        client_count: u32::try_from(info.client_count).unwrap_or(u32::MAX),
    }
}

fn reconcile_client_membership_local(req: &ReconcileArgs) -> Result<SessionAck, ReconcileError> {
    use bmux_session_models::{ClientId, SessionId};

    let Some(state) = global_plugin_state_registry().get::<SessionManager>() else {
        return Err(ReconcileError::Failed {
            reason: "sessions plugin state not registered".to_string(),
        });
    };
    let mut manager = state.write().map_err(|_| ReconcileError::Failed {
        reason: "session manager lock poisoned".to_string(),
    })?;

    let client_id = ClientId(req.client_id);

    if let Some(previous_uuid) = req.previous
        && let Some(session) = manager.get_session_mut(&SessionId(previous_uuid))
    {
        session.remove_client(&client_id);
    }

    if let Some(next_uuid) = req.next
        && let Some(session) = manager.get_session_mut(&SessionId(next_uuid))
    {
        session.add_client(client_id);
    }
    drop(manager);

    Ok(SessionAck {
        id: req.next.unwrap_or_else(::uuid::Uuid::nil),
    })
}

// ── IPC helpers ──────────────────────────────────────────────────────

fn new_session_via_ipc(
    caller: &impl ServiceCaller,
    name: Option<String>,
) -> Result<SessionAck, NewSessionError> {
    match caller.execute_kernel_request(bmux_ipc::Request::NewSession { name: name.clone() }) {
        Ok(bmux_ipc::ResponsePayload::SessionCreated {
            id,
            name: created_name,
        }) => {
            let _ = global_event_bus().emit(
                &sessions_events::EVENT_KIND,
                SessionEvent::Created {
                    session_id: id,
                    name: created_name.or(name),
                },
            );
            Ok(SessionAck { id })
        }
        Ok(_) => Err(NewSessionError::Failed {
            reason: "unexpected response payload for new-session".to_string(),
        }),
        Err(err) => Err(NewSessionError::Failed {
            reason: err.to_string(),
        }),
    }
}

fn kill_session_via_ipc(
    caller: &impl ServiceCaller,
    selector: &WireSelector,
    force_local: bool,
) -> Result<SessionAck, KillSessionError> {
    let Some(ipc_selector) = selector.to_ipc() else {
        return Err(KillSessionError::Failed {
            reason: "selector must specify either id or name".to_string(),
        });
    };
    match caller.execute_kernel_request(bmux_ipc::Request::KillSession {
        selector: ipc_selector,
        force_local,
    }) {
        Ok(bmux_ipc::ResponsePayload::SessionKilled { id }) => {
            let _ = global_event_bus().emit(
                &sessions_events::EVENT_KIND,
                SessionEvent::Removed { session_id: id },
            );
            Ok(SessionAck { id })
        }
        Ok(_) => Err(KillSessionError::Failed {
            reason: "unexpected response payload for kill-session".to_string(),
        }),
        Err(err) => Err(KillSessionError::Failed {
            reason: err.to_string(),
        }),
    }
}

fn select_session_via_ipc(
    caller: &impl ServiceCaller,
    selector: &WireSelector,
) -> Result<SessionAck, SelectSessionError> {
    let Some(ipc_selector) = selector.to_ipc() else {
        return Err(SelectSessionError::Denied {
            reason: "selector must specify either id or name".to_string(),
        });
    };
    match caller.execute_kernel_request(bmux_ipc::Request::Attach {
        selector: ipc_selector,
    }) {
        Ok(bmux_ipc::ResponsePayload::Attached { grant }) => {
            let _ = global_event_bus().emit(
                &sessions_events::EVENT_KIND,
                SessionEvent::Selected {
                    session_id: grant.session_id,
                },
            );
            Ok(SessionAck {
                id: grant.session_id,
            })
        }
        Ok(_) => Err(SelectSessionError::Denied {
            reason: "unexpected response payload for select-session".to_string(),
        }),
        Err(err) => Err(SelectSessionError::Denied {
            reason: err.to_string(),
        }),
    }
}

// ── Typed state (query) handle ───────────────────────────────────────

pub struct SessionsStateHandle {
    caller: Arc<TypedServiceCaller>,
}

impl SessionsStateHandle {
    const fn new(caller: Arc<TypedServiceCaller>) -> Self {
        Self { caller }
    }
}

impl SessionsStateService for SessionsStateHandle {
    fn list_sessions<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Vec<SessionSummary>> + Send + 'a>> {
        Box::pin(async move {
            self.caller
                .call_service::<(), Vec<SessionSummary>>(
                    bmux_sessions_plugin_api::capabilities::SESSIONS_READ.as_str(),
                    ServiceKind::Query,
                    sessions_state::INTERFACE_ID.as_str(),
                    "list-sessions",
                    &(),
                )
                .unwrap_or_default()
        })
    }

    fn get_session<'a>(
        &'a self,
        selector: StateSessionSelector,
    ) -> Pin<
        Box<
            dyn Future<Output = std::result::Result<SessionSummary, SessionQueryError>> + Send + 'a,
        >,
    > {
        Box::pin(async move {
            #[derive(serde::Serialize)]
            struct Args {
                selector: StateSessionSelector,
            }
            self.caller
                .call_service::<Args, Result<SessionSummary, SessionQueryError>>(
                    bmux_sessions_plugin_api::capabilities::SESSIONS_READ.as_str(),
                    ServiceKind::Query,
                    sessions_state::INTERFACE_ID.as_str(),
                    "get-session",
                    &Args { selector },
                )
                .unwrap_or_else(|err| {
                    Err(SessionQueryError::InvalidSelector {
                        reason: err.to_string(),
                    })
                })
        })
    }
}

// ── Typed commands handle ────────────────────────────────────────────

pub struct SessionsCommandsHandle {
    caller: Arc<TypedServiceCaller>,
}

impl SessionsCommandsHandle {
    const fn new(caller: Arc<TypedServiceCaller>) -> Self {
        Self { caller }
    }
}

impl SessionsCommandsService for SessionsCommandsHandle {
    fn new_session<'a>(
        &'a self,
        name: Option<String>,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<SessionAck, NewSessionError>> + Send + 'a>>
    {
        Box::pin(async move { new_session_via_ipc(self.caller.as_ref(), name) })
    }

    fn kill_session<'a>(
        &'a self,
        selector: CommandSessionSelector,
        force_local: bool,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<SessionAck, KillSessionError>> + Send + 'a>>
    {
        Box::pin(async move {
            let wire = WireSelector {
                id: selector.id,
                name: selector.name,
            };
            kill_session_via_ipc(self.caller.as_ref(), &wire, force_local)
        })
    }

    fn select_session<'a>(
        &'a self,
        selector: CommandSessionSelector,
    ) -> Pin<
        Box<dyn Future<Output = std::result::Result<SessionAck, SelectSessionError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let wire = WireSelector {
                id: selector.id,
                name: selector.name,
            };
            select_session_via_ipc(self.caller.as_ref(), &wire)
        })
    }

    fn reconcile_client_membership<'a>(
        &'a self,
        client_id: ::uuid::Uuid,
        previous: Option<::uuid::Uuid>,
        next: Option<::uuid::Uuid>,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<SessionAck, ReconcileError>> + Send + 'a>>
    {
        Box::pin(async move {
            reconcile_client_membership_local(&ReconcileArgs {
                client_id,
                previous,
                next,
            })
        })
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

bmux_plugin_sdk::export_plugin!(SessionsPlugin, include_str!("../plugin.toml"));
