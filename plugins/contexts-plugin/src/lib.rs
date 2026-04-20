//! bmux contexts plugin — typed owner of context lifecycle.
//!
//! Provides typed services for other plugins and attach-side callers
//! to list, create, select, and close contexts.
//!
//! The plugin reaches the server's context state directly via the IPC
//! kernel-bridge escape hatch (`ServiceCaller::execute_kernel_request`).

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

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
use bmux_plugin_sdk::{HostScope, TypedServiceRegistrationContext, TypedServiceRegistry};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

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
                list_contexts_via_ipc(ctx)
                    .map_err(|e| ServiceResponse::error("list_failed", e))
            },
            "contexts-state", "get-context" => |req: SelectorArgs, ctx| {
                get_context_via_ipc(ctx, &req.selector)
                    .map_err(|e| ServiceResponse::error("get_failed", e))
            },
            "contexts-state", "current-context" => |_req: (), ctx| {
                current_context_via_ipc(ctx)
                    .map_err(|e| ServiceResponse::error("current_failed", e))
            },
            "contexts-commands", "create-context" => |req: CreateContextArgs, ctx| {
                Ok::<Result<ContextAck, CreateContextError>, ServiceResponse>(
                    create_context_via_ipc(ctx, req.name, req.attributes)
                )
            },
            "contexts-commands", "select-context" => |req: SelectorArgs, ctx| {
                Ok::<Result<ContextAck, SelectContextError>, ServiceResponse>(
                    select_context_via_ipc(ctx, &req.selector)
                )
            },
            "contexts-commands", "close-context" => |req: CloseContextArgs, ctx| {
                Ok::<Result<ContextAck, CloseContextError>, ServiceResponse>(
                    close_context_via_ipc(ctx, &req.selector, req.force)
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

// ── IPC helpers ──────────────────────────────────────────────────────

fn list_contexts_via_ipc(caller: &impl ServiceCaller) -> Result<Vec<ContextSummary>, String> {
    match caller.execute_kernel_request(bmux_ipc::Request::ListContexts) {
        Ok(bmux_ipc::ResponsePayload::ContextList { contexts }) => {
            Ok(contexts.into_iter().map(ipc_summary_to_typed).collect())
        }
        Ok(_) => Err("unexpected response payload for list-contexts".to_string()),
        Err(err) => Err(err.to_string()),
    }
}

fn get_context_via_ipc(
    caller: &impl ServiceCaller,
    selector: &WireSelector,
) -> Result<Result<ContextSummary, ContextQueryError>, String> {
    let Some(ipc_selector) = selector.to_ipc() else {
        return Ok(Err(ContextQueryError::InvalidSelector {
            reason: "selector must specify either id or name".to_string(),
        }));
    };
    match caller.execute_kernel_request(bmux_ipc::Request::ListContexts) {
        Ok(bmux_ipc::ResponsePayload::ContextList { contexts }) => Ok(contexts
            .into_iter()
            .find(|summary| matches_selector(summary, &ipc_selector))
            .map(ipc_summary_to_typed)
            .ok_or(ContextQueryError::NotFound)),
        Ok(_) => Err("unexpected response payload for list-contexts".to_string()),
        Err(err) => Err(err.to_string()),
    }
}

fn current_context_via_ipc(caller: &impl ServiceCaller) -> Result<Option<ContextSummary>, String> {
    match caller.execute_kernel_request(bmux_ipc::Request::CurrentContext) {
        Ok(bmux_ipc::ResponsePayload::CurrentContext { context }) => {
            Ok(context.map(ipc_summary_to_typed))
        }
        Ok(_) => Err("unexpected response payload for current-context".to_string()),
        Err(err) => Err(err.to_string()),
    }
}

fn create_context_via_ipc(
    caller: &impl ServiceCaller,
    name: Option<String>,
    attributes: BTreeMap<String, String>,
) -> Result<ContextAck, CreateContextError> {
    let name_for_event = name.clone();
    match caller.execute_kernel_request(bmux_ipc::Request::CreateContext { name, attributes }) {
        Ok(bmux_ipc::ResponsePayload::ContextCreated { context }) => {
            // Emit typed event so in-process subscribers can react.
            // Errors are swallowed (fire-and-forget semantics).
            let _ = global_event_bus().emit(
                &contexts_events::EVENT_KIND,
                ContextEvent::Created {
                    context_id: context.id,
                    name: name_for_event,
                },
            );
            Ok(ContextAck { id: context.id })
        }
        Ok(_) => Err(CreateContextError::Failed {
            reason: "unexpected response payload for create-context".to_string(),
        }),
        Err(err) => Err(CreateContextError::Failed {
            reason: err.to_string(),
        }),
    }
}

fn select_context_via_ipc(
    caller: &impl ServiceCaller,
    selector: &WireSelector,
) -> Result<ContextAck, SelectContextError> {
    let Some(ipc_selector) = selector.to_ipc() else {
        return Err(SelectContextError::Denied {
            reason: "selector must specify either id or name".to_string(),
        });
    };
    match caller.execute_kernel_request(bmux_ipc::Request::SelectContext {
        selector: ipc_selector,
    }) {
        Ok(bmux_ipc::ResponsePayload::ContextSelected { context }) => {
            let _ = global_event_bus().emit(
                &contexts_events::EVENT_KIND,
                ContextEvent::Selected {
                    context_id: context.id,
                },
            );
            Ok(ContextAck { id: context.id })
        }
        Ok(_) => Err(SelectContextError::Denied {
            reason: "unexpected response payload for select-context".to_string(),
        }),
        Err(err) => Err(SelectContextError::Denied {
            reason: err.to_string(),
        }),
    }
}

fn close_context_via_ipc(
    caller: &impl ServiceCaller,
    selector: &WireSelector,
    force: bool,
) -> Result<ContextAck, CloseContextError> {
    let Some(ipc_selector) = selector.to_ipc() else {
        return Err(CloseContextError::Failed {
            reason: "selector must specify either id or name".to_string(),
        });
    };
    match caller.execute_kernel_request(bmux_ipc::Request::CloseContext {
        selector: ipc_selector,
        force,
    }) {
        Ok(bmux_ipc::ResponsePayload::ContextClosed { id }) => {
            let _ = global_event_bus().emit(
                &contexts_events::EVENT_KIND,
                ContextEvent::Closed { context_id: id },
            );
            Ok(ContextAck { id })
        }
        Ok(_) => Err(CloseContextError::Failed {
            reason: "unexpected response payload for close-context".to_string(),
        }),
        Err(err) => Err(CloseContextError::Failed {
            reason: err.to_string(),
        }),
    }
}

// ── Typed state handle ───────────────────────────────────────────────

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
        Box::pin(async move { list_contexts_via_ipc(self.caller.as_ref()).unwrap_or_default() })
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
            match get_context_via_ipc(self.caller.as_ref(), &wire) {
                Ok(result) => result,
                Err(reason) => Err(ContextQueryError::InvalidSelector { reason }),
            }
        })
    }

    fn current_context<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Option<ContextSummary>> + Send + 'a>> {
        Box::pin(async move { current_context_via_ipc(self.caller.as_ref()).ok().flatten() })
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
        Box::pin(async move { create_context_via_ipc(self.caller.as_ref(), name, attributes) })
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
            select_context_via_ipc(self.caller.as_ref(), &wire)
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
            close_context_via_ipc(self.caller.as_ref(), &wire, force)
        })
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

fn matches_selector(
    summary: &bmux_ipc::ContextSummary,
    selector: &bmux_ipc::ContextSelector,
) -> bool {
    match selector {
        bmux_ipc::ContextSelector::ById(id) => summary.id == *id,
        bmux_ipc::ContextSelector::ByName(name) => summary.name.as_deref() == Some(name.as_str()),
    }
}

fn ipc_summary_to_typed(summary: bmux_ipc::ContextSummary) -> ContextSummary {
    ContextSummary {
        id: summary.id,
        name: summary.name,
        attributes: summary.attributes,
    }
}

bmux_plugin_sdk::export_plugin!(ContextsPlugin, include_str!("../plugin.toml"));
