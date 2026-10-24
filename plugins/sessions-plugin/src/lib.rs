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
    PluginEventKind, StatefulPlugin, StatefulPluginError, StatefulPluginHandle,
    StatefulPluginResult, StatefulPluginSnapshot, TypedServiceRegistrationContext,
    TypedServiceRegistry,
};
use bmux_session_models::{
    ClientId, Session, SessionId, SessionInfo, SessionSelector as PrimitiveSessionSelector,
};
use bmux_session_state::{
    SessionManagerHandle, SessionManagerReader, SessionManagerSnapshot, SessionManagerWriter,
};
use bmux_sessions_plugin_api::sessions_commands::{
    self, KillSessionError, NewSessionError, ReconcileError, RenameSessionError,
    SelectSessionError, SessionAck, SessionsCommandsService,
};
use bmux_sessions_plugin_api::sessions_events::{self, SessionEvent};
use bmux_sessions_plugin_api::sessions_state::{
    self, SessionQueryError, SessionSelector as StateSessionSelector, SessionSummary,
    SessionsStateService,
};
use bmux_snapshot_runtime::StatefulPluginRegistry;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

const COMMAND_OUTCOME_SELECTED_CONTEXT_ID_KEY: &str = "bmux.contexts.selected_context_id";

const fn dispatch_client<C: ServiceCaller + Sync + ?Sized>(
    caller: &C,
) -> bmux_plugin::ServiceCallerDispatchClient<'_, C> {
    bmux_plugin::ServiceCallerDispatchClient::new(caller)
}

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

    fn rename_session(&self, session_id: SessionId, name: String) -> anyhow::Result<()> {
        self.inner
            .write()
            .map_err(|_| anyhow::anyhow!("session-manager lock poisoned"))?
            .rename_session(session_id, name)
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

    fn validate_snapshot(&self, snapshot: &StatefulPluginSnapshot) -> StatefulPluginResult<()> {
        decode_sessions_snapshot(snapshot).map(|_| ())
    }

    fn restore_snapshot(&self, snapshot: StatefulPluginSnapshot) -> StatefulPluginResult<()> {
        self.writer
            .restore_snapshot(decode_sessions_snapshot(&snapshot)?);
        Ok(())
    }
}

fn decode_sessions_snapshot(
    snapshot: &StatefulPluginSnapshot,
) -> StatefulPluginResult<SessionManagerSnapshot> {
    if snapshot.version != SESSIONS_STATEFUL_VERSION {
        return Err(StatefulPluginError::UnsupportedVersion {
            plugin: SESSIONS_STATEFUL_ID.as_str().to_string(),
            version: snapshot.version,
            expected: vec![SESSIONS_STATEFUL_VERSION],
        });
    }
    serde_json::from_slice(&snapshot.bytes).map_err(|err| StatefulPluginError::RestoreFailed {
        plugin: SESSIONS_STATEFUL_ID.as_str().to_string(),
        details: err.to_string(),
    })
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

/// Wire-format argument for `rename-session`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RenameSessionArgs {
    selector: WireSelector,
    name: String,
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
    fn to_selector(&self) -> Option<PrimitiveSessionSelector> {
        if let Some(id) = self.id {
            return Some(PrimitiveSessionSelector::ById(id));
        }
        self.name
            .as_ref()
            .map(|name| PrimitiveSessionSelector::ByName(name.clone()))
    }
}

#[derive(Default)]
pub struct SessionsPlugin;

impl RustPlugin for SessionsPlugin {
    type Contract = bmux_sessions_plugin_api::Contract;

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
        context: NativeCommandContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        match context.command.as_str() {
            "new-session" => {
                let name = option_value(&context.arguments, "name");
                match new_session_with_context(&context, name) {
                    Ok(ack) => {
                        record_selected_context_outcome(ack.id);
                        if matches!(
                            context.invocation_source,
                            bmux_plugin_sdk::NativeCommandInvocationSource::Cli
                        ) {
                            println!("created session: {}", ack.id);
                        }
                        Ok(bmux_plugin_sdk::EXIT_OK)
                    }
                    Err(error) => Err(PluginCommandError::failed(format!(
                        "new-session failed: {error:?}"
                    ))),
                }
            }
            other => Err(PluginCommandError::unknown_command(other)),
        }
    }

    fn invoke_service(&self, context: NativeServiceContext) -> ServiceResponse {
        bmux_plugin_sdk::route_service!(context, {
            "sessions-state", "list-sessions" => |_req: (), _ctx| {
                list_sessions_local()
                    .map_err(|e| ServiceResponse::error("list_failed", e))
            },
            "sessions-state", "get-session" => |req: SelectorArgs, _ctx| {
                get_session_local(&req.selector)
                    .map_err(|e| ServiceResponse::error("get_failed", e))
            },
            "sessions-program-v1", "new-program-session" => |req: bmux_sessions_plugin_api::sessions_program_v1::client::NewProgramSessionRequest, ctx| {
                Ok::<_, ServiceResponse>(new_program_session(ctx, req.name, req.options))
            },
            "sessions-spawn-v1", "new-session" => |req: bmux_sessions_plugin_api::sessions_spawn_v1::client::NewSessionRequest, ctx| {
                Ok::<_, ServiceResponse>(new_session_with_options(ctx, req.name, req.options))
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
            "sessions-commands", "rename-session" => |req: RenameSessionArgs, _ctx| {
                Ok::<Result<SessionAck, RenameSessionError>, ServiceResponse>(
                    rename_session_local(&req.selector, &req.name)
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

        let state: Arc<dyn SessionsStateService + Send + Sync> =
            Arc::new(SessionsStateHandle::new(Arc::clone(&caller)));
        let _ = sessions_state::register_provider(registry, state);

        let commands: Arc<dyn SessionsCommandsService + Send + Sync> =
            Arc::new(SessionsCommandsHandle::new(Arc::clone(&caller)));
        let _ = sessions_commands::register_provider(registry, commands);
        let spawn: Arc<
            dyn bmux_sessions_plugin_api::sessions_spawn_v1::SessionsSpawnV1Service + Send + Sync,
        > = Arc::new(SessionsCommandsHandle::new(caller));
        let _ = bmux_sessions_plugin_api::sessions_spawn_v1::register_provider(registry, spawn);
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
    let Some(session_selector) = selector.to_selector() else {
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
        .find(|info| matches_session_info(info, &session_selector))
        .map(session_info_to_typed)
        .ok_or(SessionQueryError::NotFound))
}

fn matches_session_info(
    info: &bmux_session_models::SessionInfo,
    selector: &PrimitiveSessionSelector,
) -> bool {
    match selector {
        PrimitiveSessionSelector::ById(id) => info.id.0 == *id,
        PrimitiveSessionSelector::ByName(name) => info.name.as_deref() == Some(name.as_str()),
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

fn new_session_with_context(
    caller: &(impl ServiceCaller + Sync),
    name: Option<String>,
) -> Result<SessionAck, NewSessionError> {
    // CLI callers do not own context state. Let the activated contexts provider
    // allocate, bind and select in the caller's target runtime; do not preallocate
    // a session here, which would orphan it if context creation cannot run.
    let mut client = dispatch_client(caller);
    let context = bmux_plugin::block_on_typed_dispatch(
        bmux_contexts_plugin_api::contexts_commands::client::create_context(
            &mut client,
            name,
            BTreeMap::from([("workspace".to_string(), "default".to_string())]),
        ),
    )
    .map_err(|error| NewSessionError::Failed {
        reason: format!("contexts-commands/create-context failed: {error}"),
    })?
    .map_err(|error| NewSessionError::Failed {
        reason: format!("context creation failed: {error:?}"),
    })?;
    Ok(SessionAck { id: context.id })
}

fn record_selected_context_outcome(context_id: ::uuid::Uuid) {
    bmux_plugin_sdk::record_command_outcome_metadata(
        COMMAND_OUTCOME_SELECTED_CONTEXT_ID_KEY,
        serde_json::json!(context_id),
    );
}

fn new_program_session(
    caller: &(impl ServiceCaller + Sync),
    name: Option<String>,
    options: bmux_sessions_plugin_api::sessions_program_v1::ProgramOptions,
) -> Result<
    bmux_sessions_plugin_api::sessions_program_v1::ExecutionRef,
    bmux_sessions_plugin_api::sessions_program_v1::ProgramError,
> {
    use bmux_pane_runtime_plugin_api::pane_runtime_execution_v4 as execution;
    use bmux_sessions_plugin_api::sessions_program_v1::{ExecutionRef, ProgramError};
    let mut client = dispatch_client(caller);
    let result = bmux_plugin::block_on_typed_dispatch(execution::client::new_program_session(
        &mut client,
        name.clone(),
        execution::TerminalProgramOptions {
            program: options.program,
            args: options.args,
            cwd: options.cwd,
            env: options.env,
            cols: options.cols,
            rows: options.rows,
        },
    ))
    .map_err(|error| ProgramError::LaunchFailed {
        reason: error.to_string(),
    })?
    .map_err(|error| match error {
        execution::TerminalExecutionError::Unavailable => ProgramError::Unavailable,
        execution::TerminalExecutionError::Denied { reason } => ProgramError::Denied { reason },
        execution::TerminalExecutionError::InvalidOptions { reason } => {
            ProgramError::InvalidOptions { reason }
        }
        other => ProgramError::LaunchFailed {
            reason: format!("{other:?}"),
        },
    })?;
    let _ = global_event_bus().emit(
        &sessions_events::EVENT_KIND,
        SessionEvent::Created {
            session_id: result.session_id,
            name,
        },
    );
    Ok(ExecutionRef {
        session_id: result.session_id,
        pane_id: result.pane_id,
        execution_token: result.execution_token,
    })
}

fn new_session_with_options(
    caller: &(impl ServiceCaller + Sync),
    name: Option<String>,
    options: bmux_sessions_plugin_api::sessions_spawn_v1::InitialShellOptions,
) -> Result<SessionAck, NewSessionError> {
    let mut client = dispatch_client(caller);
    let result = bmux_plugin::block_on_typed_dispatch(
        bmux_pane_runtime_plugin_api::pane_runtime_spawn_commands_v1::client::new_session(
            &mut client,
            name.clone(),
            bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::InitialShellOptions {
                cwd: options.cwd,
            },
        ),
    )
    .map_err(|error| NewSessionError::Failed {
        reason: error.to_string(),
    })?
    .map_err(|error| NewSessionError::Failed {
        reason: format!("{error:?}"),
    })?;
    let session_id = result.session_id;
    let _ = global_event_bus().emit(
        &sessions_events::EVENT_KIND,
        SessionEvent::Created { session_id, name },
    );
    Ok(SessionAck { id: session_id })
}

fn new_session_via_ipc(
    caller: &(impl ServiceCaller + Sync),
    name: Option<String>,
) -> Result<SessionAck, NewSessionError> {
    let mut client = dispatch_client(caller);
    let result = bmux_plugin::block_on_typed_dispatch(
        bmux_pane_runtime_plugin_api::pane_runtime_commands::client::new_session_with_runtime(
            &mut client,
            name.clone(),
        ),
    );
    match result {
        Ok(Ok(ack)) => {
            let session_id = ack.session_id;
            let _ = global_event_bus().emit(
                &sessions_events::EVENT_KIND,
                SessionEvent::Created { session_id, name },
            );
            Ok(SessionAck { id: session_id })
        }
        Ok(Err(err)) => Err(NewSessionError::Failed {
            reason: format!("{err:?}"),
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
    #[derive(serde::Serialize)]
    struct Args {
        session_id: ::uuid::Uuid,
        force_local: bool,
    }
    // Name-based selection resolves to an id locally via the
    // plugin-owned `SessionManager` so the typed pane-runtime call
    // can receive a concrete uuid.
    let session_id = match selector {
        WireSelector { id: Some(id), .. } => *id,
        WireSelector {
            id: None,
            name: Some(name),
        } => {
            let Some(state) = bmux_plugin::global_plugin_state_registry().get::<SessionManager>()
            else {
                return Err(KillSessionError::Failed {
                    reason: "sessions plugin state not registered".to_string(),
                });
            };
            let manager = state.read().map_err(|_| KillSessionError::Failed {
                reason: "session manager lock poisoned".to_string(),
            })?;
            match manager
                .list_sessions()
                .into_iter()
                .find(|info| info.name.as_deref() == Some(name.as_str()))
            {
                Some(info) => info.id.0,
                None => return Err(KillSessionError::NotFound),
            }
        }
        _ => {
            return Err(KillSessionError::Failed {
                reason: "selector must specify either id or name".to_string(),
            });
        }
    };

    let result = caller.call_service::<Args, Result<
        bmux_pane_runtime_plugin_api::pane_runtime_commands::SessionAck,
        bmux_pane_runtime_plugin_api::pane_runtime_commands::SessionRuntimeCommandError,
    >>(
        bmux_pane_runtime_plugin_api::capabilities::PANE_RUNTIME_WRITE.as_str(),
        ServiceKind::Command,
        bmux_pane_runtime_plugin_api::pane_runtime_commands::INTERFACE_ID.as_str(),
        "kill-session-runtime",
        &Args {
            session_id,
            force_local,
        },
    );
    match result {
        Ok(Ok(ack)) => {
            let id = ack.session_id;
            let _ = global_event_bus().emit(
                &sessions_events::EVENT_KIND,
                SessionEvent::Removed { session_id: id },
            );
            Ok(SessionAck { id })
        }
        Ok(Err(
            bmux_pane_runtime_plugin_api::pane_runtime_commands::SessionRuntimeCommandError::SessionNotFound,
        )) => Err(KillSessionError::NotFound),
        Ok(Err(err)) => Err(KillSessionError::Failed {
            reason: format!("{err:?}"),
        }),
        Err(err) => Err(KillSessionError::Failed {
            reason: err.to_string(),
        }),
    }
}

fn pane_runtime_session_selector(
    selector: PrimitiveSessionSelector,
) -> bmux_pane_runtime_plugin_api::attach_runtime_commands::SessionSelector {
    match selector {
        PrimitiveSessionSelector::ById(id) => {
            bmux_pane_runtime_plugin_api::attach_runtime_commands::SessionSelector {
                id: Some(id),
                name: None,
            }
        }
        PrimitiveSessionSelector::ByName(name) => {
            bmux_pane_runtime_plugin_api::attach_runtime_commands::SessionSelector {
                id: None,
                name: Some(name),
            }
        }
    }
}

fn validate_rename_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("name must not be empty".to_string());
    }
    Ok(trimmed.to_string())
}

fn rename_session_local(
    selector: &WireSelector,
    name: &str,
) -> Result<SessionAck, RenameSessionError> {
    let name =
        validate_rename_name(name).map_err(|reason| RenameSessionError::InvalidName { reason })?;
    let Some(session_selector) = selector.to_selector() else {
        return Err(RenameSessionError::Failed {
            reason: "selector must specify either id or name".to_string(),
        });
    };
    let Some(state) = global_plugin_state_registry().get::<SessionManager>() else {
        return Err(RenameSessionError::Failed {
            reason: "sessions plugin state not registered".to_string(),
        });
    };
    let mut manager = state.write().map_err(|_| RenameSessionError::Failed {
        reason: "session manager lock poisoned".to_string(),
    })?;
    let Some(session_id) = manager
        .list_sessions()
        .into_iter()
        .find(|info| matches_session_info(info, &session_selector))
        .map(|info| info.id)
    else {
        return Err(RenameSessionError::NotFound);
    };
    manager
        .rename_session(session_id, name.clone())
        .map_err(|error| RenameSessionError::Failed {
            reason: error.to_string(),
        })?;
    drop(manager);

    let _ = global_event_bus().emit(
        &sessions_events::EVENT_KIND,
        SessionEvent::Renamed {
            session_id: session_id.0,
            name,
        },
    );
    Ok(SessionAck { id: session_id.0 })
}

fn map_attach_session_error(
    error: bmux_pane_runtime_plugin_api::attach_runtime_commands::AttachCommandError,
) -> SelectSessionError {
    match error {
        bmux_pane_runtime_plugin_api::attach_runtime_commands::AttachCommandError::SessionNotFound => {
            SelectSessionError::NotFound
        }
        error => SelectSessionError::Denied {
            reason: format!("{error:?}"),
        },
    }
}

fn select_session_via_ipc(
    caller: &(impl ServiceCaller + Sync),
    selector: &WireSelector,
) -> Result<SessionAck, SelectSessionError> {
    let Some(session_selector) = selector.to_selector() else {
        return Err(SelectSessionError::Denied {
            reason: "selector must specify either id or name".to_string(),
        });
    };
    let mut client = bmux_plugin::ServiceCallerDispatchClient::new(caller);
    let result = bmux_plugin::block_on_typed_dispatch(
        bmux_pane_runtime_plugin_api::attach_runtime_commands::client::attach_session(
            &mut client,
            pane_runtime_session_selector(session_selector),
            true,
        ),
    );
    match result {
        Ok(Ok(grant)) => {
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
        Ok(Err(err)) => Err(map_attach_session_error(err)),
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

impl bmux_sessions_plugin_api::sessions_spawn_v1::SessionsSpawnV1Service
    for SessionsCommandsHandle
{
    fn new_session<'a>(
        &'a self,
        name: Option<String>,
        options: bmux_sessions_plugin_api::sessions_spawn_v1::InitialShellOptions,
    ) -> Pin<Box<dyn Future<Output = Result<SessionAck, NewSessionError>> + Send + 'a>> {
        Box::pin(async move { new_session_with_options(self.caller.as_ref(), name, options) })
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
        selector: StateSessionSelector,
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
        selector: StateSessionSelector,
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

    fn rename_session<'a>(
        &'a self,
        selector: StateSessionSelector,
        name: String,
    ) -> Pin<
        Box<dyn Future<Output = std::result::Result<SessionAck, RenameSessionError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let wire = WireSelector {
                id: selector.id,
                name: selector.name,
            };
            rename_session_local(&wire, &name)
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

/// Extract the value of a `--<long_name> <value>` option from a
/// flattened argument vector. Mirrors the helper used by other
/// plugins for keybinding-dispatched commands.
fn option_value(arguments: &[String], long_name: &str) -> Option<String> {
    let long_flag = format!("--{long_name}");
    arguments
        .windows(2)
        .find_map(|chunk| (chunk[0] == long_flag).then(|| chunk[1].clone()))
}

bmux_plugin_sdk::export_plugin!(SessionsPlugin, include_str!("../plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    struct ContextCreationCaller {
        expected_name: Option<String>,
        result: Result<
            bmux_contexts_plugin_api::contexts_commands::ContextAck,
            bmux_contexts_plugin_api::contexts_commands::CreateContextError,
        >,
        transport_failure: bool,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl ServiceCaller for ContextCreationCaller {
        fn call_service_raw(
            &self,
            capability: &str,
            kind: bmux_plugin_sdk::ServiceKind,
            interface: &str,
            operation: &str,
            payload: Vec<u8>,
        ) -> bmux_plugin_sdk::Result<Vec<u8>> {
            let payload = bmux_plugin_sdk::service_contract::decode_for(interface, <bmux_contexts_plugin_api::Contract as bmux_plugin_sdk::PluginContract>::service_contract_for(interface).expect("generated mock contract"), &payload)?.to_vec();
            // This mock preserves the ServiceCaller error ABI by value.
            #[allow(clippy::result_large_err)]
            let mock_response = || -> bmux_plugin_sdk::Result<Vec<u8>> {
                use bmux_contexts_plugin_api::contexts_commands;
                // Any preallocation/current-client lookup fails these assertions. The
                // generated dispatcher must reach just the target's contexts provider.
                assert_eq!(
                    capability,
                    bmux_contexts_plugin_api::capabilities::CONTEXTS_WRITE.as_str()
                );
                assert_eq!(kind, bmux_plugin_sdk::ServiceKind::Command);
                assert_eq!(interface, contexts_commands::INTERFACE_ID.as_str());
                assert_eq!(operation, contexts_commands::OP_CREATE_CONTEXT.as_str());
                let request: contexts_commands::client::CreateContextRequest =
                    bmux_plugin_sdk::decode_service_message(&payload)?;
                assert_eq!(request.name, self.expected_name);
                assert_eq!(
                    request.attributes,
                    BTreeMap::from([("workspace".to_string(), "default".to_string())])
                );
                self.calls
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if self.transport_failure {
                    return Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                        details: "target contexts unavailable".into(),
                    });
                }
                bmux_plugin_sdk::encode_service_message(&self.result)
            };
            let response = mock_response()?;
            Ok(bmux_plugin_sdk::service_contract::encode_response(
                &response,
            ))
        }

        fn execute_kernel_request(
            &self,
            _request: bmux_ipc::Request,
        ) -> bmux_plugin_sdk::Result<bmux_ipc::ResponsePayload> {
            panic!("CLI context creation must not allocate through a kernel fallback")
        }
    }

    #[test]
    fn cli_new_session_dispatches_context_creation_without_local_state_or_preallocation() {
        use bmux_contexts_plugin_api::contexts_commands::ContextAck;
        let context_id = ::uuid::Uuid::from_u128(123);
        let session_id = ::uuid::Uuid::from_u128(456);
        for name in [None, Some("remote-work".to_string())] {
            let caller = ContextCreationCaller {
                expected_name: name.clone(),
                result: Ok(ContextAck {
                    id: context_id,
                    session_id: Some(session_id),
                }),
                transport_failure: false,
                calls: std::sync::atomic::AtomicUsize::new(0),
            };
            // No activate call or registry setup: the CLI is a remote caller only.
            let ack = new_session_with_context(&caller, name).unwrap();
            assert_eq!(ack.id, context_id);
            assert_eq!(caller.calls.load(std::sync::atomic::Ordering::Relaxed), 1);
            // The unchanged CLI outcome path must publish the returned context,
            // not the different underlying session id.
            bmux_plugin_sdk::begin_command_outcome_capture();
            record_selected_context_outcome(ack.id);
            let outcome = bmux_plugin_sdk::finish_command_outcome_capture();
            let expected = context_id.to_string();
            assert_eq!(
                outcome
                    .metadata
                    .get(COMMAND_OUTCOME_SELECTED_CONTEXT_ID_KEY)
                    .and_then(serde_json::Value::as_str),
                Some(expected.as_str())
            );
        }
    }

    #[test]
    fn cli_context_creation_failure_has_no_preallocated_session_or_fallback() {
        use bmux_contexts_plugin_api::contexts_commands::CreateContextError;
        for transport_failure in [false, true] {
            let caller = ContextCreationCaller {
                expected_name: Some("unavailable-target".to_string()),
                result: Err(CreateContextError::Failed {
                    reason: "creation denied".into(),
                }),
                transport_failure,
                calls: std::sync::atomic::AtomicUsize::new(0),
            };
            let error =
                new_session_with_context(&caller, caller.expected_name.clone()).unwrap_err();
            let expected = if transport_failure {
                "target contexts unavailable"
            } else {
                "creation denied"
            };
            assert!(
                matches!(error, NewSessionError::Failed { reason } if reason.contains(expected))
            );
            assert_eq!(caller.calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        }
    }

    #[test]
    fn missing_attach_session_maps_to_select_not_found() {
        let error = map_attach_session_error(
            bmux_pane_runtime_plugin_api::attach_runtime_commands::AttachCommandError::SessionNotFound,
        );

        assert!(matches!(error, SelectSessionError::NotFound));
    }

    #[test]
    fn attach_session_denial_remains_select_denial() {
        let error = map_attach_session_error(
            bmux_pane_runtime_plugin_api::attach_runtime_commands::AttachCommandError::Denied {
                reason: "blocked".to_string(),
            },
        );

        assert!(matches!(
            error,
            SelectSessionError::Denied { reason } if reason.contains("blocked")
        ));
    }

    #[test]
    fn fresh_session_forwards_generated_options_without_legacy_fallback() {
        struct UnavailableSpawn;
        impl ServiceCaller for UnavailableSpawn {
            fn call_service_raw(
                &self,
                capability: &str,
                _kind: bmux_plugin_sdk::ServiceKind,
                interface: &str,
                operation: &str,
                payload: Vec<u8>,
            ) -> bmux_plugin_sdk::Result<Vec<u8>> {
                let payload = bmux_plugin_sdk::service_contract::decode_for(interface, <bmux_pane_runtime_plugin_api::Contract as bmux_plugin_sdk::PluginContract>::service_contract_for(interface).expect("generated mock contract"), &payload)?.to_vec();
                // This mock preserves the ServiceCaller error ABI by value.
                #[allow(clippy::result_large_err)]
                let mock_response = || -> bmux_plugin_sdk::Result<Vec<u8>> {
                    assert_eq!(capability, "bmux.pane_runtime.spawn.v1");
                    assert_eq!(interface, "pane-runtime-spawn-commands-v1");
                    assert_eq!(operation, "new-session");
                    let request: bmux_pane_runtime_plugin_api::pane_runtime_spawn_commands_v1::client::NewSessionRequest =
                    bmux_plugin_sdk::decode_service_message(&payload)?;
                    assert_eq!(request.name.as_deref(), Some("fresh"));
                    assert_eq!(
                        request.options.cwd.as_deref(),
                        Some("/execution-host/project")
                    );
                    Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                        details: "spawn capability unavailable".into(),
                    })
                };
                let response = mock_response()?;
                Ok(bmux_plugin_sdk::service_contract::encode_response(
                    &response,
                ))
            }

            fn execute_kernel_request(
                &self,
                _request: bmux_ipc::Request,
            ) -> bmux_plugin_sdk::Result<bmux_ipc::ResponsePayload> {
                panic!("fresh creation must not bypass generated service transport")
            }
        }
        let error = new_session_with_options(
            &UnavailableSpawn,
            Some("fresh".into()),
            bmux_sessions_plugin_api::sessions_spawn_v1::InitialShellOptions {
                cwd: Some("/execution-host/project".into()),
            },
        )
        .unwrap_err();
        assert!(
            matches!(error, NewSessionError::Failed { reason } if reason.contains("spawn capability unavailable"))
        );
    }

    #[test]
    fn selected_context_outcome_records_new_context_id() {
        let context_id = ::uuid::Uuid::from_u128(42);
        bmux_plugin_sdk::begin_command_outcome_capture();
        record_selected_context_outcome(context_id);

        let outcome = bmux_plugin_sdk::finish_command_outcome_capture();
        let expected = context_id.to_string();
        assert_eq!(
            outcome
                .metadata
                .get(COMMAND_OUTCOME_SELECTED_CONTEXT_ID_KEY)
                .and_then(serde_json::Value::as_str),
            Some(expected.as_str())
        );
    }

    #[test]
    fn pane_runtime_session_selector_converts_id_to_bpdl_record_shape() {
        let id = uuid::Uuid::new_v4();
        let selector = pane_runtime_session_selector(PrimitiveSessionSelector::ById(id));

        assert_eq!(selector.id, Some(id));
        assert_eq!(selector.name, None);
    }

    #[test]
    fn pane_runtime_session_selector_converts_name_to_bpdl_record_shape() {
        let selector =
            pane_runtime_session_selector(PrimitiveSessionSelector::ByName("target".to_string()));

        assert_eq!(selector.id, None);
        assert_eq!(selector.name.as_deref(), Some("target"));
    }

    #[test]
    fn attach_session_request_uses_generated_bpdl_struct_shape() {
        let id = uuid::Uuid::new_v4();
        let request =
            bmux_pane_runtime_plugin_api::attach_runtime_commands::client::AttachSessionRequest {
                selector: pane_runtime_session_selector(PrimitiveSessionSelector::ById(id)),
                can_write: true,
            };

        let encoded = bmux_plugin_sdk::encode_service_message(&request)
            .expect("generated attach-session request should encode");
        let decoded: bmux_pane_runtime_plugin_api::attach_runtime_commands::client::AttachSessionRequest =
            bmux_plugin_sdk::decode_service_message(&encoded)
                .expect("generated attach-session request should decode");

        assert_eq!(decoded.selector.id, Some(id));
        assert_eq!(decoded.selector.name, None);
        assert!(decoded.can_write);
    }
}
