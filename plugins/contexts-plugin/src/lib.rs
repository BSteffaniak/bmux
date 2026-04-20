//! bmux contexts plugin — authoritative owner of context lifecycle and state.
//!
//! Provides typed services for other plugins and attach-side callers
//! to list, create, select, and close contexts. State lives in this
//! plugin's address space; server never names `ContextState`.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![allow(clippy::result_large_err)]

pub mod context_state;

pub use context_state::{CONTEXT_SESSION_ID_ATTRIBUTE, ContextState, RuntimeContext};

use bmux_contexts_plugin_api::contexts_commands::{
    self, CloseContextError, ContextAck, ContextSelector as CommandContextSelector,
    ContextsCommandsService, CreateContextError, SelectContextError,
};
use bmux_contexts_plugin_api::contexts_events::{self, ContextEvent};
use bmux_contexts_plugin_api::contexts_state::{
    self, ContextQueryError, ContextSelector as StateContextSelector, ContextSummary,
    ContextsStateService,
};
use bmux_plugin::{
    ServiceCaller, TypedServiceCaller, global_event_bus, global_plugin_state_registry,
};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{
    HostScope, ServiceKind as SdkServiceKind, TypedServiceRegistrationContext,
    TypedServiceRegistry, decode_service_message, encode_service_message,
};
use bmux_session_models::{ClientId, SessionId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

// ── Argument records (wire) ─────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CreateContextArgs {
    #[serde(default)]
    name: Option<String>,
    attributes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SelectorArgs {
    selector: WireSelector,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CloseContextArgs {
    selector: WireSelector,
    force: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireSelector {
    #[serde(default)]
    id: Option<::uuid::Uuid>,
    #[serde(default)]
    name: Option<String>,
}

impl WireSelector {
    fn to_ipc(&self) -> Option<bmux_ipc::ContextSelector> {
        if let Some(id) = self.id {
            return Some(bmux_ipc::ContextSelector::ById(id));
        }
        self.name
            .as_ref()
            .map(|name| bmux_ipc::ContextSelector::ByName(name.clone()))
    }
}

// ── Plugin entrypoint ────────────────────────────────────────────────

#[derive(Default)]
pub struct ContextsPlugin;

impl RustPlugin for ContextsPlugin {
    fn activate(
        &mut self,
        _context: NativeLifecycleContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        let state: Arc<RwLock<ContextState>> = Arc::new(RwLock::new(ContextState::default()));
        global_plugin_state_registry().register::<ContextState>(&state);
        // Register the typed event channel for `contexts-events`.
        // Consumers subscribe via
        // `global_event_bus().subscribe::<ContextEvent>(&contexts_events::EVENT_KIND)`.
        global_event_bus().register_channel::<ContextEvent>(contexts_events::EVENT_KIND);
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
            "contexts-state", "list-contexts" => |_req: (), ctx| {
                list_contexts_local(ctx)
                    .map_err(|e| ServiceResponse::error("list_failed", e))
            },
            "contexts-state", "get-context" => |req: SelectorArgs, ctx| {
                get_context_local(ctx, &req.selector)
                    .map_err(|e| ServiceResponse::error("get_failed", e))
            },
            "contexts-state", "current-context" => |_req: (), ctx| {
                current_context_local(ctx)
                    .map_err(|e| ServiceResponse::error("current_failed", e))
            },
            "contexts-commands", "create-context" => |req: CreateContextArgs, ctx| {
                Ok::<Result<ContextAck, CreateContextError>, ServiceResponse>(
                    create_context_local(ctx, req.name, req.attributes)
                )
            },
            "contexts-commands", "select-context" => |req: SelectorArgs, ctx| {
                Ok::<Result<ContextAck, SelectContextError>, ServiceResponse>(
                    select_context_local(ctx, &req.selector)
                )
            },
            "contexts-commands", "close-context" => |req: CloseContextArgs, ctx| {
                Ok::<Result<ContextAck, CloseContextError>, ServiceResponse>(
                    close_context_local(ctx, &req.selector, req.force)
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
            HostScope::new(bmux_contexts_plugin_api::capabilities::CONTEXTS_READ.as_str()),
            HostScope::new(bmux_contexts_plugin_api::capabilities::CONTEXTS_WRITE.as_str()),
        ) else {
            return;
        };

        let state: Arc<dyn ContextsStateService + Send + Sync> =
            Arc::new(ContextsStateHandle::new(Arc::clone(&caller)));
        registry.insert_typed::<dyn ContextsStateService + Send + Sync>(
            read_cap,
            ServiceKind::Query,
            contexts_state::INTERFACE_ID,
            state,
        );

        let commands: Arc<dyn ContextsCommandsService + Send + Sync> =
            Arc::new(ContextsCommandsHandle::new(caller));
        registry.insert_typed::<dyn ContextsCommandsService + Send + Sync>(
            write_cap,
            ServiceKind::Command,
            contexts_commands::INTERFACE_ID,
            commands,
        );
    }
}

// ── Plugin-local state helpers ───────────────────────────────────────

/// Retrieve the plugin-owned `ContextState` handle.
fn local_state() -> Arc<RwLock<ContextState>> {
    global_plugin_state_registry().get::<ContextState>().expect(
        "contexts-plugin: ContextState must be registered by activate before any handler runs",
    )
}

/// Resolve the caller's `ClientId` from the `NativeServiceContext`.
/// Falls back to `Request::WhoAmI` when the context did not carry a
/// client id (e.g. legacy IPC paths that haven't been updated yet).
fn resolve_caller_client_id(caller: &impl ServiceCaller) -> Result<ClientId, String> {
    if let Some(id) = caller_client_id_from_thread_local() {
        return Ok(ClientId(id));
    }
    match caller.execute_kernel_request(bmux_ipc::Request::WhoAmI) {
        Ok(bmux_ipc::ResponsePayload::ClientIdentity { id }) => Ok(ClientId(id)),
        Ok(_) => Err("unexpected response payload for whoami".to_string()),
        Err(err) => Err(err.to_string()),
    }
}

/// Look up the caller's client id from the `NativeServiceContext` that
/// is currently in scope, if the handler was dispatched via the
/// `route_service!` macro. The macro doesn't currently expose the
/// context's `caller_client_id` field to handler closures, so this
/// helper returns `None` and the fallback uses `WhoAmI`.
const fn caller_client_id_from_thread_local() -> Option<::uuid::Uuid> {
    // Future: read from a thread-local populated by the dispatch
    // machinery so handlers don't need the `WhoAmI` round-trip. For
    // now, always fall back.
    None
}

// ── Read handlers (state-local) ──────────────────────────────────────

fn list_contexts_local(_caller: &impl ServiceCaller) -> Result<Vec<ContextSummary>, String> {
    let state = local_state();
    let guard = state
        .read()
        .map_err(|_| "context state lock poisoned".to_string())?;
    Ok(guard.list().into_iter().map(ipc_summary_to_typed).collect())
}

#[allow(clippy::significant_drop_tightening)]
fn get_context_local(
    _caller: &impl ServiceCaller,
    selector: &WireSelector,
) -> Result<Result<ContextSummary, ContextQueryError>, String> {
    let Some(ipc_selector) = selector.to_ipc() else {
        return Ok(Err(ContextQueryError::InvalidSelector {
            reason: "selector must specify either id or name".to_string(),
        }));
    };
    let state = local_state();
    let guard = state
        .read()
        .map_err(|_| "context state lock poisoned".to_string())?;
    let resolved = guard.resolve_id(&ipc_selector);
    Ok(resolved.map_or(Err(ContextQueryError::NotFound), |id| {
        guard
            .contexts
            .get(&id)
            .map(ContextState::to_summary)
            .map(ipc_summary_to_typed)
            .ok_or(ContextQueryError::NotFound)
    }))
}

fn current_context_local(caller: &impl ServiceCaller) -> Result<Option<ContextSummary>, String> {
    let client_id = resolve_caller_client_id(caller)?;
    let state = local_state();
    let guard = state
        .read()
        .map_err(|_| "context state lock poisoned".to_string())?;
    Ok(guard
        .current_for_client(client_id)
        .map(ipc_summary_to_typed))
}

// ── Write handlers (state-local + cross-plugin orchestration) ────────

fn create_context_local(
    caller: &impl ServiceCaller,
    name: Option<String>,
    attributes: BTreeMap<String, String>,
) -> Result<ContextAck, CreateContextError> {
    let client_id =
        resolve_caller_client_id(caller).map_err(|reason| CreateContextError::Failed { reason })?;

    // Cross-plugin orchestration: ask the sessions plugin to allocate
    // a session runtime for this context. We go via typed dispatch so
    // contexts-plugin doesn't know (or care) which plugin implements
    // `sessions-commands`.
    let session_id = create_session_via_sessions_plugin(caller, name.clone())?;

    // Local state mutation: construct the context, bind it to the
    // freshly-created session, and emit the lifecycle event.
    let (context_summary, bind_result) =
        mutate_state_create(client_id, name.clone(), attributes, session_id)?;

    if let Err(reason) = bind_result {
        return Err(CreateContextError::Failed {
            reason: reason.to_string(),
        });
    }

    let _ = global_event_bus().emit(
        &contexts_events::EVENT_KIND,
        ContextEvent::Created {
            context_id: context_summary.id,
            name,
        },
    );
    Ok(ContextAck {
        id: context_summary.id,
    })
}

#[allow(clippy::significant_drop_tightening)]
fn mutate_state_create(
    client_id: ClientId,
    name: Option<String>,
    attributes: BTreeMap<String, String>,
    session_id: SessionId,
) -> Result<
    (
        bmux_ipc::ContextSummary,
        core::result::Result<(), &'static str>,
    ),
    CreateContextError,
> {
    let state = local_state();
    let mut guard = state.write().map_err(|_| CreateContextError::Failed {
        reason: "context state lock poisoned".to_string(),
    })?;
    let context = guard.create(client_id, name, attributes);
    let bind_result = guard.bind_session(context.id, session_id);
    Ok((context, bind_result))
}

fn select_context_local(
    caller: &impl ServiceCaller,
    selector: &WireSelector,
) -> Result<ContextAck, SelectContextError> {
    let Some(ipc_selector) = selector.to_ipc() else {
        return Err(SelectContextError::Denied {
            reason: "selector must specify either id or name".to_string(),
        });
    };
    let client_id =
        resolve_caller_client_id(caller).map_err(|reason| SelectContextError::Denied { reason })?;

    let (context, session_after_select) = mutate_state_select(client_id, &ipc_selector)?;

    // If the newly-selected context is bound to a session, ask the
    // sessions plugin to make it the caller's selected session so the
    // attach view retargets properly.
    if let Some(session_id) = session_after_select {
        let _ = select_session_via_sessions_plugin(caller, session_id);
    }

    let _ = global_event_bus().emit(
        &contexts_events::EVENT_KIND,
        ContextEvent::Selected {
            context_id: context.id,
        },
    );
    Ok(ContextAck { id: context.id })
}

#[allow(clippy::significant_drop_tightening)]
fn mutate_state_select(
    client_id: ClientId,
    ipc_selector: &bmux_ipc::ContextSelector,
) -> Result<(bmux_ipc::ContextSummary, Option<SessionId>), SelectContextError> {
    let state = local_state();
    let mut guard = state.write().map_err(|_| SelectContextError::Denied {
        reason: "context state lock poisoned".to_string(),
    })?;
    let context = guard
        .select_for_client(client_id, ipc_selector)
        .map_err(|reason| SelectContextError::Denied {
            reason: reason.to_string(),
        })?;
    let session_id = guard.current_session_for_client(client_id);
    Ok((context, session_id))
}

fn close_context_local(
    caller: &impl ServiceCaller,
    selector: &WireSelector,
    force: bool,
) -> Result<ContextAck, CloseContextError> {
    let Some(ipc_selector) = selector.to_ipc() else {
        return Err(CloseContextError::Failed {
            reason: "selector must specify either id or name".to_string(),
        });
    };
    let client_id =
        resolve_caller_client_id(caller).map_err(|reason| CloseContextError::Failed { reason })?;

    let (removed_id, bound_session_id) = mutate_state_close(client_id, &ipc_selector, force)?;

    // Cross-plugin orchestration: if the closed context had a bound
    // session runtime, kill it via the sessions plugin.
    if let Some(session_id) = bound_session_id {
        let _ = kill_session_via_sessions_plugin(caller, session_id);
    }

    let _ = global_event_bus().emit(
        &contexts_events::EVENT_KIND,
        ContextEvent::Closed {
            context_id: removed_id,
        },
    );
    Ok(ContextAck { id: removed_id })
}

#[allow(clippy::significant_drop_tightening)]
fn mutate_state_close(
    client_id: ClientId,
    ipc_selector: &bmux_ipc::ContextSelector,
    force: bool,
) -> Result<(::uuid::Uuid, Option<SessionId>), CloseContextError> {
    let state = local_state();
    let mut guard = state.write().map_err(|_| CloseContextError::Failed {
        reason: "context state lock poisoned".to_string(),
    })?;
    guard
        .close(client_id, ipc_selector, force)
        .map_err(|_reason| CloseContextError::NotFound)
}

// ── Cross-plugin helpers: sessions-commands typed dispatch ───────────

fn create_session_via_sessions_plugin(
    caller: &impl ServiceCaller,
    name: Option<String>,
) -> Result<SessionId, CreateContextError> {
    #[derive(Serialize)]
    struct NewSessionArgs {
        name: Option<String>,
    }
    use bmux_sessions_plugin_api::sessions_commands::{self, NewSessionError, SessionAck};

    let payload = encode_service_message(&NewSessionArgs { name }).map_err(|err| {
        CreateContextError::Failed {
            reason: format!("encode new-session payload: {err}"),
        }
    })?;
    let resp_bytes = caller
        .call_service_raw(
            bmux_sessions_plugin_api::capabilities::SESSIONS_WRITE.as_str(),
            SdkServiceKind::Command,
            sessions_commands::INTERFACE_ID.as_str(),
            "new-session",
            payload,
        )
        .map_err(|err| CreateContextError::Failed {
            reason: format!("sessions-commands:new-session failed: {err}"),
        })?;
    let result: Result<SessionAck, NewSessionError> =
        decode_service_message(&resp_bytes).map_err(|err| CreateContextError::Failed {
            reason: format!("decode new-session response: {err}"),
        })?;
    match result {
        Ok(ack) => Ok(SessionId(ack.id)),
        Err(NewSessionError::NameAlreadyExists { name }) => {
            Err(CreateContextError::NameAlreadyExists { name })
        }
        Err(NewSessionError::InvalidName { reason }) => {
            Err(CreateContextError::InvalidName { reason })
        }
        Err(NewSessionError::Failed { reason }) => Err(CreateContextError::Failed { reason }),
    }
}

fn kill_session_via_sessions_plugin(
    caller: &impl ServiceCaller,
    session_id: SessionId,
) -> Result<(), String> {
    use bmux_sessions_plugin_api::sessions_commands::{
        self, KillSessionError, SessionAck, SessionSelector,
    };

    #[derive(Serialize)]
    struct KillSessionArgs {
        selector: SessionSelector,
        force_local: bool,
    }

    let payload = encode_service_message(&KillSessionArgs {
        selector: SessionSelector {
            id: Some(session_id.0),
            name: None,
        },
        force_local: false,
    })
    .map_err(|err| format!("encode kill-session payload: {err}"))?;
    let resp_bytes = caller
        .call_service_raw(
            bmux_sessions_plugin_api::capabilities::SESSIONS_WRITE.as_str(),
            SdkServiceKind::Command,
            sessions_commands::INTERFACE_ID.as_str(),
            "kill-session",
            payload,
        )
        .map_err(|err| format!("sessions-commands:kill-session failed: {err}"))?;
    let result: Result<SessionAck, KillSessionError> = decode_service_message(&resp_bytes)
        .map_err(|err| format!("decode kill-session response: {err}"))?;
    match result {
        Ok(_) | Err(KillSessionError::NotFound) => Ok(()),
        Err(KillSessionError::Failed { reason }) => Err(reason),
    }
}

fn select_session_via_sessions_plugin(
    caller: &impl ServiceCaller,
    session_id: SessionId,
) -> Result<(), String> {
    use bmux_sessions_plugin_api::sessions_commands::{
        self, SelectSessionError, SessionAck, SessionSelector,
    };

    #[derive(Serialize)]
    struct SelectSessionArgs {
        selector: SessionSelector,
    }

    let payload = encode_service_message(&SelectSessionArgs {
        selector: SessionSelector {
            id: Some(session_id.0),
            name: None,
        },
    })
    .map_err(|err| format!("encode select-session payload: {err}"))?;
    let resp_bytes = caller
        .call_service_raw(
            bmux_sessions_plugin_api::capabilities::SESSIONS_WRITE.as_str(),
            SdkServiceKind::Command,
            sessions_commands::INTERFACE_ID.as_str(),
            "select-session",
            payload,
        )
        .map_err(|err| format!("sessions-commands:select-session failed: {err}"))?;
    let result: Result<SessionAck, SelectSessionError> = decode_service_message(&resp_bytes)
        .map_err(|err| format!("decode select-session response: {err}"))?;
    match result {
        Ok(_) | Err(SelectSessionError::NotFound) => Ok(()),
        Err(SelectSessionError::Denied { reason }) => Err(reason),
    }
}

// ── Typed state handle (consumed by other plugins) ───────────────────

pub struct ContextsStateHandle {
    caller: Arc<TypedServiceCaller>,
}

impl ContextsStateHandle {
    const fn new(caller: Arc<TypedServiceCaller>) -> Self {
        Self { caller }
    }
}

impl ContextsStateService for ContextsStateHandle {
    fn list_contexts<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Vec<ContextSummary>> + Send + 'a>> {
        Box::pin(async move { list_contexts_local(self.caller.as_ref()).unwrap_or_default() })
    }

    fn get_context<'a>(
        &'a self,
        selector: StateContextSelector,
    ) -> Pin<
        Box<
            dyn Future<Output = std::result::Result<ContextSummary, ContextQueryError>> + Send + 'a,
        >,
    > {
        Box::pin(async move {
            let wire = WireSelector {
                id: selector.id,
                name: selector.name,
            };
            match get_context_local(self.caller.as_ref(), &wire) {
                Ok(result) => result,
                Err(reason) => Err(ContextQueryError::InvalidSelector { reason }),
            }
        })
    }

    fn current_context<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Option<ContextSummary>> + Send + 'a>> {
        Box::pin(async move { current_context_local(self.caller.as_ref()).ok().flatten() })
    }
}

// ── Typed commands handle ────────────────────────────────────────────

pub struct ContextsCommandsHandle {
    caller: Arc<TypedServiceCaller>,
}

impl ContextsCommandsHandle {
    const fn new(caller: Arc<TypedServiceCaller>) -> Self {
        Self { caller }
    }
}

impl ContextsCommandsService for ContextsCommandsHandle {
    fn create_context<'a>(
        &'a self,
        name: Option<String>,
        attributes: BTreeMap<String, String>,
    ) -> Pin<
        Box<dyn Future<Output = std::result::Result<ContextAck, CreateContextError>> + Send + 'a>,
    > {
        Box::pin(async move { create_context_local(self.caller.as_ref(), name, attributes) })
    }

    fn select_context<'a>(
        &'a self,
        selector: CommandContextSelector,
    ) -> Pin<
        Box<dyn Future<Output = std::result::Result<ContextAck, SelectContextError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let wire = WireSelector {
                id: selector.id,
                name: selector.name,
            };
            select_context_local(self.caller.as_ref(), &wire)
        })
    }

    fn close_context<'a>(
        &'a self,
        selector: CommandContextSelector,
        force: bool,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<ContextAck, CloseContextError>> + Send + 'a>>
    {
        Box::pin(async move {
            let wire = WireSelector {
                id: selector.id,
                name: selector.name,
            };
            close_context_local(self.caller.as_ref(), &wire, force)
        })
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

fn ipc_summary_to_typed(summary: bmux_ipc::ContextSummary) -> ContextSummary {
    ContextSummary {
        id: summary.id,
        name: summary.name,
        attributes: summary.attributes,
    }
}

bmux_plugin_sdk::export_plugin!(ContextsPlugin, include_str!("../plugin.toml"));
