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
pub use context_state::ContextState;

use bmux_clients_plugin_api::clients_state::{
    self as clients_state, ClientQueryError, ClientSummary,
};
use bmux_context_state::{
    ContextStateHandle, ContextStateReader, ContextStateSnapshot, ContextStateWriter,
    RuntimeContext,
};
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
    HostRuntimeApi, ServiceCaller, TypedServiceCaller, global_event_bus,
    global_plugin_state_registry,
};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{
    HostScope, PluginEventKind, ServiceKind as SdkServiceKind, StatefulPlugin, StatefulPluginError,
    StatefulPluginHandle, StatefulPluginResult, StatefulPluginSnapshot,
    TypedServiceRegistrationContext, TypedServiceRegistry, decode_service_message,
    encode_service_message,
};
use bmux_session_models::{ClientId, SessionId};
use bmux_snapshot_runtime::StatefulPluginRegistry;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use tracing::instrument;
use uuid::Uuid;

/// Adapter wrapping the plugin's `Arc<RwLock<ContextState>>` and
/// implementing the domain-agnostic [`ContextStateReader`] +
/// [`ContextStateWriter`] traits from `bmux_context_state`.
///
/// Registered as a [`ContextStateHandle`] in the plugin state registry
/// alongside the concrete `Arc<RwLock<ContextState>>` so consumers can
/// read context-state through the trait surface without naming the
/// concrete plugin-owned type.
struct ContextStateAdapter {
    inner: Arc<RwLock<ContextState>>,
}

impl ContextStateAdapter {
    fn with_read<T>(&self, f: impl FnOnce(&ContextState) -> T, fallback: T) -> T {
        self.inner.read().map_or(fallback, |guard| f(&guard))
    }

    fn with_write<T>(&self, f: impl FnOnce(&mut ContextState) -> T, fallback: T) -> T {
        self.inner
            .write()
            .map_or(fallback, |mut guard| f(&mut guard))
    }
}

impl ContextStateReader for ContextStateAdapter {
    fn list(&self) -> Vec<bmux_ipc::ContextSummary> {
        self.with_read(ContextState::list, Vec::<bmux_ipc::ContextSummary>::new())
    }

    fn current_for_client(&self, client_id: ClientId) -> Option<bmux_ipc::ContextSummary> {
        self.with_read(|state| state.current_for_client(client_id), None)
    }

    fn current_session_for_client(&self, client_id: ClientId) -> Option<SessionId> {
        self.with_read(|state| state.current_session_for_client(client_id), None)
    }

    fn context_for_session(&self, session_id: SessionId) -> Option<Uuid> {
        self.with_read(|state| state.context_for_session(session_id), None)
    }

    fn resolve_id(
        &self,
        selector: &bmux_ipc::ContextSelector,
    ) -> std::result::Result<Uuid, &'static str> {
        self.with_read(
            |state| state.resolve_id(selector),
            Err("context-state lock poisoned"),
        )
    }
}

impl ContextStateWriter for ContextStateAdapter {
    fn create(
        &self,
        client_id: ClientId,
        name: Option<String>,
        attributes: BTreeMap<String, String>,
    ) -> bmux_ipc::ContextSummary {
        let fallback = bmux_ipc::ContextSummary {
            id: Uuid::nil(),
            name: name.clone(),
            attributes: attributes.clone(),
        };
        self.with_write(|state| state.create(client_id, name, attributes), fallback)
    }

    fn select_for_client(
        &self,
        client_id: ClientId,
        selector: &bmux_ipc::ContextSelector,
    ) -> std::result::Result<bmux_ipc::ContextSummary, &'static str> {
        self.with_write(
            |state| state.select_for_client(client_id, selector),
            Err("context-state lock poisoned"),
        )
    }

    fn close(
        &self,
        client_id: ClientId,
        selector: &bmux_ipc::ContextSelector,
        force: bool,
    ) -> std::result::Result<(Uuid, Option<SessionId>), &'static str> {
        self.with_write(
            |state| state.close(client_id, selector, force),
            Err("context-state lock poisoned"),
        )
    }

    fn remove_contexts_for_session(&self, session_id: SessionId) -> Vec<Uuid> {
        self.with_write(
            |state| state.remove_contexts_for_session(session_id),
            Vec::new(),
        )
    }

    fn bind_session(
        &self,
        context_id: Uuid,
        session_id: SessionId,
    ) -> std::result::Result<(), &'static str> {
        self.with_write(
            |state| state.bind_session(context_id, session_id),
            Err("context-state lock poisoned"),
        )
    }

    fn disconnect_client(&self, client_id: ClientId) {
        self.with_write(|state| state.disconnect_client(client_id), ());
    }

    fn remove_context_by_id(
        &self,
        context_id: Uuid,
        preferred_client: Option<ClientId>,
    ) -> Option<(Uuid, Option<SessionId>)> {
        self.with_write(
            |state| state.remove_context_by_id(context_id, preferred_client),
            None,
        )
    }

    fn snapshot(&self) -> ContextStateSnapshot {
        self.with_read(
            |state| {
                let contexts = state
                    .contexts
                    .iter()
                    .map(|(id, rc)| {
                        (
                            *id,
                            RuntimeContext {
                                id: rc.id,
                                name: rc.name.clone(),
                                attributes: rc.attributes.clone(),
                            },
                        )
                    })
                    .collect();
                ContextStateSnapshot {
                    contexts,
                    session_by_context: state.session_by_context.clone(),
                    selected_by_client: state.selected_by_client.clone(),
                    mru_contexts: state.mru_contexts.clone(),
                }
            },
            ContextStateSnapshot::default(),
        )
    }

    fn restore_snapshot(&self, snapshot: ContextStateSnapshot) {
        self.with_write(
            |state| {
                state.contexts = snapshot.contexts;
                state.session_by_context = snapshot.session_by_context;
                state.selected_by_client = snapshot.selected_by_client;
                state.mru_contexts = snapshot.mru_contexts;
            },
            (),
        );
    }
}

// ── StatefulPlugin participant for persistence ─────────────────────

/// Stable id for the context-state snapshot surface.
const CONTEXTS_STATEFUL_ID: PluginEventKind =
    PluginEventKind::from_static("bmux.contexts/context-state");

/// Current snapshot schema version for context-state.
const CONTEXTS_STATEFUL_VERSION: u32 = 1;

/// Snapshot participant that serializes the plugin's [`ContextState`]
/// via the domain-agnostic [`ContextStateWriter::snapshot`] /
/// [`ContextStateWriter::restore_snapshot`] hooks.
struct ContextsStatefulPlugin {
    writer: Arc<dyn ContextStateWriter>,
}

impl StatefulPlugin for ContextsStatefulPlugin {
    fn id(&self) -> PluginEventKind {
        CONTEXTS_STATEFUL_ID
    }

    fn snapshot(&self) -> StatefulPluginResult<StatefulPluginSnapshot> {
        let snap = self.writer.snapshot();
        let bytes =
            serde_json::to_vec(&snap).map_err(|err| StatefulPluginError::SnapshotFailed {
                plugin: CONTEXTS_STATEFUL_ID.as_str().to_string(),
                details: err.to_string(),
            })?;
        Ok(StatefulPluginSnapshot::new(
            CONTEXTS_STATEFUL_ID,
            CONTEXTS_STATEFUL_VERSION,
            bytes,
        ))
    }

    fn restore_snapshot(&self, snapshot: StatefulPluginSnapshot) -> StatefulPluginResult<()> {
        if snapshot.version != CONTEXTS_STATEFUL_VERSION {
            return Err(StatefulPluginError::UnsupportedVersion {
                plugin: CONTEXTS_STATEFUL_ID.as_str().to_string(),
                version: snapshot.version,
                expected: vec![CONTEXTS_STATEFUL_VERSION],
            });
        }
        let decoded: ContextStateSnapshot =
            serde_json::from_slice(&snapshot.bytes).map_err(|err| {
                StatefulPluginError::RestoreFailed {
                    plugin: CONTEXTS_STATEFUL_ID.as_str().to_string(),
                    details: err.to_string(),
                }
            })?;
        self.writer.restore_snapshot(decoded);
        Ok(())
    }
}

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

        // Register the trait-object handle so consumers can reach
        // context state through the domain-agnostic reader/writer
        // surface without naming the concrete plugin-owned type.
        let adapter = ContextStateAdapter {
            inner: Arc::clone(&state),
        };
        let handle = Arc::new(RwLock::new(ContextStateHandle::new(adapter)));
        global_plugin_state_registry().register::<ContextStateHandle>(&handle);

        // Register this plugin as a persistence participant so the
        // snapshot-orchestration plugin can drive save/restore over
        // context-state on its schedule.
        let writer_for_snapshot: Arc<dyn ContextStateWriter> = {
            let guard = handle
                .read()
                .expect("freshly-created ContextStateHandle lock is poisoned");
            Arc::clone(&guard.0)
        };
        let stateful = StatefulPluginHandle::new(ContextsStatefulPlugin {
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
                current_context_local(ctx, ctx.caller_client_id)
                    .map_err(|e| ServiceResponse::error("current_failed", e))
            },
            "contexts-commands", "create-context" => |req: CreateContextArgs, ctx| {
                Ok::<Result<ContextAck, CreateContextError>, ServiceResponse>(
                    create_context_local(ctx, ctx.caller_client_id, req.name, req.attributes)
                )
            },
            "contexts-commands", "select-context" => |req: SelectorArgs, ctx| {
                Ok::<Result<ContextAck, SelectContextError>, ServiceResponse>(
                    select_context_local(ctx, ctx.caller_client_id, &req.selector)
                )
            },
            "contexts-commands", "close-context" => |req: CloseContextArgs, ctx| {
                Ok::<Result<ContextAck, CloseContextError>, ServiceResponse>(
                    close_context_local(ctx, ctx.caller_client_id, &req.selector, req.force)
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
///
/// Returns `Err` rather than panicking when no state has been
/// registered. The typical cause is a handler firing in a process
/// where `ContextsPlugin::activate` has not run (for example, an
/// attach client that loaded the plugin crate in-process but is
/// expected to reach the activated instance via
/// `Request::InvokeService`). The service-location dispatch layer in
/// `bmux_plugin::loader::call_service_raw` normally routes such
/// callers to the server-side provider before this function runs, so
/// a non-routed reach here indicates a bootstrap ordering issue.
fn local_state() -> Result<Arc<RwLock<ContextState>>, String> {
    global_plugin_state_registry()
        .get::<ContextState>()
        .ok_or_else(|| {
            "contexts-plugin: ContextState not registered in this process \
             (activate did not run here; typed dispatch should forward to the \
             process that owns the activated provider)"
                .to_string()
        })
}

/// Resolve the caller's `ClientId` from the `NativeServiceContext`.
/// Falls back to a typed `clients-state::current-client` query when
/// the context did not carry a client id (e.g. legacy IPC paths that
/// haven't been updated yet).
fn resolve_caller_client_id(
    caller: &impl ServiceCaller,
    caller_client_id: Option<::uuid::Uuid>,
) -> Result<ClientId, String> {
    if let Some(id) = caller_client_id {
        return Ok(ClientId(id));
    }
    match caller.call_service::<(), std::result::Result<ClientSummary, ClientQueryError>>(
        bmux_clients_plugin_api::capabilities::CLIENTS_READ.as_str(),
        ServiceKind::Query,
        clients_state::INTERFACE_ID.as_str(),
        "current-client",
        &(),
    ) {
        Ok(Ok(summary)) => Ok(ClientId(summary.id)),
        Ok(Err(err)) => Err(format!("current-client query failed: {err:?}")),
        Err(err) => Err(err.to_string()),
    }
}

// ── Read handlers (state-local) ──────────────────────────────────────

fn list_contexts_local(_caller: &impl ServiceCaller) -> Result<Vec<ContextSummary>, String> {
    let state = local_state()?;
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
    let state = local_state()?;
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

fn current_context_local(
    caller: &impl ServiceCaller,
    caller_client_id: Option<::uuid::Uuid>,
) -> Result<Option<ContextSummary>, String> {
    let client_id = resolve_caller_client_id(caller, caller_client_id)?;
    let state = local_state()?;
    let guard = state
        .read()
        .map_err(|_| "context state lock poisoned".to_string())?;
    Ok(guard
        .current_for_client(client_id)
        .map(ipc_summary_to_typed))
}

// ── Write handlers (state-local + cross-plugin orchestration) ────────

#[instrument(
    level = "debug",
    target = "bmux_contexts_plugin::lifecycle",
    skip_all,
    fields(
        caller_client_id = ?caller_client_id,
        name = %name.as_deref().unwrap_or("<unnamed>"),
        context_id = tracing::field::Empty,
        session_id = tracing::field::Empty,
    ),
)]
fn create_context_local(
    caller: &impl ServiceCaller,
    caller_client_id: Option<::uuid::Uuid>,
    name: Option<String>,
    attributes: BTreeMap<String, String>,
) -> Result<ContextAck, CreateContextError> {
    let client_id = resolve_caller_client_id(caller, caller_client_id)
        .map_err(|reason| CreateContextError::Failed { reason })?;
    // Pair this entry log with the `ContextState::create` debug log
    // emitted from the state layer. The entry log proves a plugin
    // command actually reached `create_context_local`; the state log
    // proves a context mutation actually happened. Their 1:1
    // relationship should hold; if not, the divergence points at the
    // bug.
    tracing::debug!(
        target: "bmux_contexts_plugin::lifecycle",
        client_id = %client_id.0,
        name = %name.as_deref().unwrap_or("<unnamed>"),
        "create_context_local begin",
    );

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

    // Cross-plugin orchestration: atomic create-and-select. The
    // sessions-plugin tracks each client's selected session
    // independently from context state, so we mirror the selection
    // there too. Without this, followers / multi-client attach views
    // can see the new context in listings but cannot retarget to its
    // pane runtime.
    if let Err(reason) = select_session_via_sessions_plugin(caller, session_id) {
        // Session select is best-effort; failing should not unwind
        // the context creation. Log through the host so the error is
        // observable without corrupting a TUI.
        let _ = caller.log_write(&bmux_plugin_sdk::LogWriteRequest {
            level: bmux_plugin_sdk::LogWriteLevel::Warn,
            message: format!(
                "contexts.create_context: failed to select created session (context_id={} session_id={}): {reason}",
                context_summary.id, session_id.0,
            ),
            target: Some("bmux.contexts".to_string()),
        });
    }

    // Record the freshly-minted ids on the instrumentation span so
    // downstream log analysis can attribute events / retargets to the
    // specific create call that produced them.
    tracing::Span::current().record("context_id", tracing::field::display(context_summary.id));
    tracing::Span::current().record("session_id", tracing::field::display(session_id.0));

    // Event ordering: `Created` first (listings / catalog refresh),
    // then `Selected` (so subscribers observing the selection delta
    // retarget deterministically), then
    // `SessionActiveContextChanged` (multi-client focus broadcast
    // carrying enough context for attach runtimes to apply follow
    // policy).
    let _ = global_event_bus().emit(
        &contexts_events::EVENT_KIND,
        ContextEvent::Created {
            context_id: context_summary.id,
            name,
        },
    );
    let _ = global_event_bus().emit(
        &contexts_events::EVENT_KIND,
        ContextEvent::Selected {
            context_id: context_summary.id,
        },
    );
    let _ = global_event_bus().emit(
        &contexts_events::EVENT_KIND,
        ContextEvent::SessionActiveContextChanged {
            session_id: session_id.0,
            context_id: context_summary.id,
            initiator_client_id: Some(client_id.0),
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
    let state = local_state().map_err(|reason| CreateContextError::Failed { reason })?;
    let mut guard = state.write().map_err(|_| CreateContextError::Failed {
        reason: "context state lock poisoned".to_string(),
    })?;
    let context = guard.create(client_id, name, attributes);
    let bind_result = guard.bind_session(context.id, session_id);
    Ok((context, bind_result))
}

#[instrument(
    level = "debug",
    target = "bmux_contexts_plugin::lifecycle",
    skip_all,
    fields(
        caller_client_id = ?caller_client_id,
        context_id = tracing::field::Empty,
        session_id = tracing::field::Empty,
    ),
)]
fn select_context_local(
    caller: &impl ServiceCaller,
    caller_client_id: Option<::uuid::Uuid>,
    selector: &WireSelector,
) -> Result<ContextAck, SelectContextError> {
    let Some(ipc_selector) = selector.to_ipc() else {
        return Err(SelectContextError::Denied {
            reason: "selector must specify either id or name".to_string(),
        });
    };
    let client_id = resolve_caller_client_id(caller, caller_client_id)
        .map_err(|reason| SelectContextError::Denied { reason })?;

    let (context, session_after_select) = mutate_state_select(client_id, &ipc_selector)?;

    tracing::Span::current().record("context_id", tracing::field::display(context.id));
    if let Some(session_id) = session_after_select {
        tracing::Span::current().record("session_id", tracing::field::display(session_id.0));
    }

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
    // Multi-client retarget broadcast: carries the session id so
    // other clients attached to the same session can decide whether
    // to follow the selection based on their local follow policy.
    if let Some(session_id) = session_after_select {
        let _ = global_event_bus().emit(
            &contexts_events::EVENT_KIND,
            ContextEvent::SessionActiveContextChanged {
                session_id: session_id.0,
                context_id: context.id,
                initiator_client_id: Some(client_id.0),
            },
        );
    }
    Ok(ContextAck { id: context.id })
}

#[allow(clippy::significant_drop_tightening)]
fn mutate_state_select(
    client_id: ClientId,
    ipc_selector: &bmux_ipc::ContextSelector,
) -> Result<(bmux_ipc::ContextSummary, Option<SessionId>), SelectContextError> {
    let state = local_state().map_err(|reason| SelectContextError::Denied { reason })?;
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

#[instrument(
    level = "debug",
    target = "bmux_contexts_plugin::lifecycle",
    skip_all,
    fields(
        caller_client_id = ?caller_client_id,
        force,
        removed_id = tracing::field::Empty,
        replacement_context_id = tracing::field::Empty,
        replacement_session_id = tracing::field::Empty,
    ),
)]
fn close_context_local(
    caller: &impl ServiceCaller,
    caller_client_id: Option<::uuid::Uuid>,
    selector: &WireSelector,
    force: bool,
) -> Result<ContextAck, CloseContextError> {
    let Some(ipc_selector) = selector.to_ipc() else {
        return Err(CloseContextError::Failed {
            reason: "selector must specify either id or name".to_string(),
        });
    };
    let client_id = resolve_caller_client_id(caller, caller_client_id)
        .map_err(|reason| CloseContextError::Failed { reason })?;

    let CloseOutcome {
        removed_id,
        bound_session_id,
        replacement,
    } = mutate_state_close(client_id, &ipc_selector, force)?;

    tracing::Span::current().record("removed_id", tracing::field::display(removed_id));
    if let Some((replacement_context_id, replacement_session_id)) = replacement {
        tracing::Span::current().record(
            "replacement_context_id",
            tracing::field::display(replacement_context_id),
        );
        if let Some(session_id) = replacement_session_id {
            tracing::Span::current().record(
                "replacement_session_id",
                tracing::field::display(session_id.0),
            );
        }
    }

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

    // Auto-focus-sibling: if the caller had this context selected and
    // a replacement was chosen by `ContextState::close`, mirror the
    // selection into sessions-plugin and emit the same event pair
    // `select-context` would, so attach runtimes retarget
    // deterministically.
    if let Some((replacement_context_id, replacement_session_id)) = replacement {
        if let Some(session_id) = replacement_session_id
            && let Err(reason) = select_session_via_sessions_plugin(caller, session_id)
        {
            let _ = caller.log_write(&bmux_plugin_sdk::LogWriteRequest {
                level: bmux_plugin_sdk::LogWriteLevel::Warn,
                message: format!(
                    "contexts.close_context: failed to select sibling session (context_id={replacement_context_id} session_id={}): {reason}",
                    session_id.0,
                ),
                target: Some("bmux.contexts".to_string()),
            });
        }
        let _ = global_event_bus().emit(
            &contexts_events::EVENT_KIND,
            ContextEvent::Selected {
                context_id: replacement_context_id,
            },
        );
        if let Some(session_id) = replacement_session_id {
            let _ = global_event_bus().emit(
                &contexts_events::EVENT_KIND,
                ContextEvent::SessionActiveContextChanged {
                    session_id: session_id.0,
                    context_id: replacement_context_id,
                    initiator_client_id: Some(client_id.0),
                },
            );
        }
    }

    Ok(ContextAck { id: removed_id })
}

/// Result of `mutate_state_close`, extending `ContextState::close` with
/// the sibling the underlying state picked for the caller's new active
/// selection (if any). `replacement` is `None` when either no contexts
/// remain or the caller didn't actually have this context selected.
struct CloseOutcome {
    removed_id: ::uuid::Uuid,
    bound_session_id: Option<SessionId>,
    replacement: Option<(::uuid::Uuid, Option<SessionId>)>,
}

#[allow(clippy::significant_drop_tightening)]
fn mutate_state_close(
    client_id: ClientId,
    ipc_selector: &bmux_ipc::ContextSelector,
    force: bool,
) -> Result<CloseOutcome, CloseContextError> {
    let state = local_state().map_err(|reason| CloseContextError::Failed { reason })?;
    let mut guard = state.write().map_err(|_| CloseContextError::Failed {
        reason: "context state lock poisoned".to_string(),
    })?;
    let (removed_id, bound_session_id) = guard
        .close(client_id, ipc_selector, force)
        .map_err(|_reason| CloseContextError::NotFound)?;
    // After `close`, the caller's `selected_by_client` now either
    // points at the replacement (if ContextState picked one) or is
    // absent (no contexts remain). Read it back to drive the event
    // emission above.
    let replacement = guard
        .selected_by_client
        .get(&client_id)
        .copied()
        .map(|context_id| {
            let session_id = guard.session_by_context.get(&context_id).copied();
            (context_id, session_id)
        });
    Ok(CloseOutcome {
        removed_id,
        bound_session_id,
        replacement,
    })
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
        Box::pin(async move {
            current_context_local(self.caller.as_ref(), None)
                .ok()
                .flatten()
        })
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
        Box::pin(async move { create_context_local(self.caller.as_ref(), None, name, attributes) })
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
            select_context_local(self.caller.as_ref(), None, &wire)
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
            close_context_local(self.caller.as_ref(), None, &wire, force)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn remove_contexts_for_session_clears_mapping_and_reselects_client() {
        let client_id = ClientId::new();
        let mut context_state = ContextState::default();

        let first = context_state.create(client_id, Some("first".to_string()), BTreeMap::new());
        let first_session_id = SessionId::new();
        context_state
            .bind_session(first.id, first_session_id)
            .expect("first context should bind to session");

        let second = context_state.create(client_id, Some("second".to_string()), BTreeMap::new());
        let second_session_id = SessionId::new();
        context_state
            .bind_session(second.id, second_session_id)
            .expect("second context should bind to session");

        let _ = context_state
            .select_for_client(client_id, &bmux_ipc::ContextSelector::ById(first.id))
            .expect("selecting first context should succeed");

        let removed = context_state.remove_contexts_for_session(first_session_id);
        assert_eq!(removed, vec![first.id]);
        assert!(
            context_state
                .context_for_session(first_session_id)
                .is_none()
        );
        assert_eq!(
            context_state
                .current_for_client(client_id)
                .map(|context| context.id),
            Some(second.id)
        );
        assert_eq!(
            context_state.current_session_for_client(client_id),
            Some(second_session_id)
        );
    }

    #[tokio::test]
    async fn close_active_context_promotes_most_recent_active_context() {
        let client_id = ClientId::new();
        let mut context_state = ContextState::default();

        let first = context_state.create(client_id, Some("first".to_string()), BTreeMap::new());
        let first_id = first.id;
        context_state
            .bind_session(first_id, SessionId::new())
            .expect("first context should bind to session");

        let second = context_state.create(client_id, Some("second".to_string()), BTreeMap::new());
        let second_id = second.id;
        context_state
            .bind_session(second_id, SessionId::new())
            .expect("second context should bind to session");

        let _ = context_state
            .select_for_client(client_id, &bmux_ipc::ContextSelector::ById(first_id))
            .expect("selecting first context should succeed");

        let (closed_id, _closed_session) = context_state
            .close(client_id, &bmux_ipc::ContextSelector::ById(first_id), true)
            .expect("closing first context should succeed");
        assert_eq!(closed_id, first_id);

        let current = context_state
            .current_for_client(client_id)
            .expect("current context should exist after close");
        assert_eq!(current.id, second_id);
    }

    // ── Event emission tests ─────────────────────────────────────────
    //
    // These tests verify the attach-side retarget contract:
    // `create_context_local` must emit `Created` -> `Selected` ->
    // `SessionActiveContextChanged` in order, and
    // `close_context_local` must emit `Closed` followed by
    // `Selected` + `SessionActiveContextChanged` for the sibling the
    // caller was reseated onto. These event pairs are what the attach
    // runtime subscribes to for deterministic focus switching.

    /// Install a minimal cross-plugin test router that satisfies the
    /// sessions-commands and clients-state calls contexts-plugin
    /// makes from its create / close / select paths.
    fn install_contexts_test_router(
        fixed_client_id: Uuid,
    ) -> bmux_plugin::test_support::TestServiceRouterGuard {
        use bmux_plugin::test_support::{TestServiceRouter, install_test_service_router};
        let router: TestServiceRouter = std::sync::Arc::new(
            move |_caller_plugin,
                  _caller_client,
                  _capability,
                  _kind,
                  interface,
                  operation,
                  _payload| {
                match (interface, operation) {
                    ("sessions-commands", "new-session") => {
                        let ack: std::result::Result<
                            bmux_sessions_plugin_api::sessions_commands::SessionAck,
                            bmux_sessions_plugin_api::sessions_commands::NewSessionError,
                        > = Ok(bmux_sessions_plugin_api::sessions_commands::SessionAck {
                            id: Uuid::new_v4(),
                        });
                        encode_service_message(&ack)
                    }
                    ("sessions-commands", "select-session") => {
                        let ack: std::result::Result<
                            bmux_sessions_plugin_api::sessions_commands::SessionAck,
                            bmux_sessions_plugin_api::sessions_commands::SelectSessionError,
                        > = Ok(bmux_sessions_plugin_api::sessions_commands::SessionAck {
                            id: Uuid::new_v4(),
                        });
                        encode_service_message(&ack)
                    }
                    ("sessions-commands", "kill-session") => {
                        let ack: std::result::Result<
                            bmux_sessions_plugin_api::sessions_commands::SessionAck,
                            bmux_sessions_plugin_api::sessions_commands::KillSessionError,
                        > = Ok(bmux_sessions_plugin_api::sessions_commands::SessionAck {
                            id: Uuid::new_v4(),
                        });
                        encode_service_message(&ack)
                    }
                    ("clients-state", "current-client") => {
                        let summary: std::result::Result<
                            bmux_clients_plugin_api::clients_state::ClientSummary,
                            bmux_clients_plugin_api::clients_state::ClientQueryError,
                        > = Ok(bmux_clients_plugin_api::clients_state::ClientSummary {
                            id: fixed_client_id,
                            selected_session_id: None,
                            selected_context_id: None,
                            following_client_id: None,
                            following_global: false,
                        });
                        encode_service_message(&summary)
                    }
                    ("logging-command/v1", "write") => encode_service_message(&()),
                    _ => Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
                        operation: "contexts_test_router",
                    }),
                }
            },
        );
        install_test_service_router(router)
    }

    /// Minimal `NativeServiceContext` wired to the fake router above.
    fn test_service_context(caller_client_id: Uuid) -> bmux_plugin_sdk::NativeServiceContext {
        bmux_plugin_sdk::NativeServiceContext {
            plugin_id: "bmux.contexts".to_string(),
            request: bmux_plugin_sdk::ServiceRequest {
                caller_plugin_id: "bmux.windows".to_string(),
                service: bmux_plugin_sdk::RegisteredService {
                    capability: HostScope::new("bmux.contexts.write")
                        .expect("capability should parse"),
                    kind: SdkServiceKind::Command,
                    interface_id: "contexts-commands".to_string(),
                    provider: bmux_plugin_sdk::ProviderId::Plugin("bmux.contexts".to_string()),
                },
                operation: "create-context".to_string(),
                payload: Vec::new(),
            },
            required_capabilities: vec![
                "bmux.contexts.read".to_string(),
                "bmux.contexts.write".to_string(),
                "bmux.clients.read".to_string(),
                "bmux.sessions.write".to_string(),
                "bmux.logs.write".to_string(),
            ],
            provided_capabilities: vec!["bmux.contexts.read".to_string()],
            services: Vec::new(),
            available_capabilities: vec![
                "bmux.contexts.read".to_string(),
                "bmux.contexts.write".to_string(),
                "bmux.clients.read".to_string(),
                "bmux.sessions.write".to_string(),
                "bmux.logs.write".to_string(),
            ],
            enabled_plugins: vec!["bmux.contexts".to_string()],
            plugin_search_roots: Vec::new(),
            host: bmux_plugin_sdk::HostMetadata {
                product_name: "bmux".to_string(),
                product_version: env!("CARGO_PKG_VERSION").to_string(),
                plugin_api_version: bmux_plugin_sdk::ApiVersion::new(1, 0),
                plugin_abi_version: bmux_plugin_sdk::ApiVersion::new(1, 0),
            },
            connection: bmux_plugin_sdk::HostConnectionInfo {
                config_dir: "/config".to_string(),
                config_dir_candidates: vec!["/config".to_string()],
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
                state_dir: "/state".to_string(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            caller_client_id: Some(caller_client_id),
            host_kernel_bridge: None,
        }
    }

    /// Subscribe to the contexts-events bus and collect emissions from
    /// the current thread for the duration of the test.
    fn subscribe_events() -> std::sync::mpsc::Receiver<ContextEvent> {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut bus_rx = bmux_plugin::global_event_bus()
            .subscribe::<ContextEvent>(&contexts_events::EVENT_KIND)
            .expect("event channel should be registered");
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("subscriber runtime should build");
            runtime.block_on(async move {
                while let Ok(event) = bus_rx.recv().await {
                    if tx.send((*event).clone()).is_err() {
                        return;
                    }
                }
            });
        });
        rx
    }

    /// Drain events already available on `rx` without blocking past a
    /// short grace window. Used at the end of tests to assert exact
    /// emitted sequences.
    fn drain_events_after_short_wait(
        rx: &std::sync::mpsc::Receiver<ContextEvent>,
    ) -> Vec<ContextEvent> {
        // Allow the async subscriber thread a moment to forward the
        // emissions before we call `try_recv`.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            out.push(event);
        }
        out
    }

    #[test]
    fn create_context_emits_created_selected_and_session_active_change_in_order() {
        // Ensure `ContextState` is registered for this test thread;
        // the plugin's `activate` normally does this during startup.
        let state_handle = std::sync::Arc::new(std::sync::RwLock::new(ContextState::default()));
        let _existing = global_plugin_state_registry().register::<ContextState>(&state_handle);
        // The contexts-events bus channel is also registered during
        // `activate`; outside of activate we may need to register it
        // here. `register_channel` is idempotent in the sense that a
        // second registration replaces the first with a fresh channel,
        // which is fine for tests.
        bmux_plugin::global_event_bus()
            .register_channel::<ContextEvent>(contexts_events::EVENT_KIND);

        let client_id = Uuid::new_v4();
        let _router_guard = install_contexts_test_router(client_id);
        let events_rx = subscribe_events();
        let ctx = test_service_context(client_id);

        let ack = create_context_local(
            &ctx,
            Some(client_id),
            Some("test-context".to_string()),
            BTreeMap::new(),
        )
        .expect("create-context should succeed");

        let emitted = drain_events_after_short_wait(&events_rx);
        assert!(
            emitted.len() >= 3,
            "expected at least 3 events, got {emitted:?}"
        );

        // Find the triple of events for this context id and verify
        // their ordering and content.
        let context_id = ack.id;
        let positions: Vec<(usize, &ContextEvent)> = emitted
            .iter()
            .enumerate()
            .filter(|(_, ev)| match ev {
                ContextEvent::Created { context_id: id, .. }
                | ContextEvent::Selected { context_id: id }
                | ContextEvent::SessionActiveContextChanged { context_id: id, .. } => {
                    *id == context_id
                }
                ContextEvent::Closed { .. } => false,
            })
            .collect();
        assert_eq!(
            positions.len(),
            3,
            "expected Created+Selected+SessionActiveContextChanged for {context_id}, got {emitted:?}"
        );
        assert!(
            matches!(positions[0].1, ContextEvent::Created { .. }),
            "first event should be Created, got {:?}",
            positions[0].1
        );
        assert!(
            matches!(positions[1].1, ContextEvent::Selected { .. }),
            "second event should be Selected, got {:?}",
            positions[1].1
        );
        let ContextEvent::SessionActiveContextChanged {
            initiator_client_id,
            ..
        } = positions[2].1
        else {
            panic!(
                "third event should be SessionActiveContextChanged, got {:?}",
                positions[2].1
            );
        };
        assert_eq!(
            *initiator_client_id,
            Some(client_id),
            "SessionActiveContextChanged must carry the initiating client id",
        );
    }
}
