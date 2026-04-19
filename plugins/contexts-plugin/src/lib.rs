//! bmux contexts plugin — typed owner of context lifecycle.
//!
//! Provides typed services for other plugins and attach-side callers
//! to list, create, select, and close contexts without going through
//! the legacy `bmux_client::BmuxClient::*_context` methods.
//!
//! # Current phase: typed facade over `HostRuntimeApi::context_*`
//!
//! In this phase the contexts-plugin does not own context state
//! directly. Its typed service handles delegate to the core host
//! runtime via [`bmux_plugin::TypedServiceCaller`] and return the
//! results through the BPDL-generated types. Once callers are all
//! migrated off `BmuxClient::*_context` and the core host-runtime's
//! context methods are deleted (M4 Stage 8), the contexts-plugin
//! will re-home the underlying state.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_contexts_plugin_api::contexts_commands::{
    self, CloseContextError, ContextAck, ContextSelector as CommandContextSelector,
    ContextsCommandsService, CreateContextError, SelectContextError,
};
use bmux_contexts_plugin_api::contexts_state::{
    self, ContextQueryError, ContextSelector as StateContextSelector, ContextSummary,
    ContextsStateService,
};
use bmux_plugin::{HostRuntimeApi, TypedServiceCaller};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{
    ContextCloseRequest as CoreContextCloseRequest,
    ContextCreateRequest as CoreContextCreateRequest,
    ContextSelectRequest as CoreContextSelectRequest, ContextSelector as CoreContextSelector,
    HostScope, TypedServiceRegistrationContext, TypedServiceRegistry,
};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Top-level plugin type. Holds no state today (the runtime data lives
/// in the core host runtime that this plugin delegates to); the type
/// exists so the SDK's `export_plugin!` macro has something to own.
#[derive(Default)]
pub struct ContextsPlugin;

impl RustPlugin for ContextsPlugin {
    fn run_command(
        &mut self,
        _context: NativeCommandContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        // The contexts plugin doesn't expose CLI commands in this
        // phase; context management is still driven by the core
        // `bmux context *` subcommands until Stage 6 migrates those
        // onto plugin-owned commands.
        Err(PluginCommandError::unknown_command(""))
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        // Byte-dispatch surface is not used today; typed dispatch
        // covers the entire contract. Fall through so the host's
        // generic "unsupported operation" response is uniform.
        ServiceResponse::error(
            "unsupported_service_operation",
            format!(
                "contexts plugin has no byte-dispatch service handler for '{}:{}'",
                context.request.service.interface_id.as_str(),
                context.request.operation.as_str(),
            ),
        )
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

// ── Typed state (query) handle ───────────────────────────────────────

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
        Box::pin(async move {
            self.caller.context_list().map_or_else(
                |_| Vec::new(),
                |response| {
                    response
                        .contexts
                        .into_iter()
                        .map(core_summary_to_typed)
                        .collect()
                },
            )
        })
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
            let Some(core_selector) = state_selector_to_core(&selector) else {
                return Err(ContextQueryError::InvalidSelector {
                    reason: "selector must specify either id or name".to_string(),
                });
            };
            // Core has no selector-targeted `get` operation; list and
            // filter client-side. Shim until Stage 8 re-homes state.
            let response =
                self.caller
                    .context_list()
                    .map_err(|err| ContextQueryError::InvalidSelector {
                        reason: err.to_string(),
                    })?;
            response
                .contexts
                .into_iter()
                .find(|summary| matches_selector(summary, &core_selector))
                .map(core_summary_to_typed)
                .ok_or(ContextQueryError::NotFound)
        })
    }

    fn current_context<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Option<ContextSummary>> + Send + 'a>> {
        Box::pin(async move {
            self.caller
                .context_current()
                .ok()
                .and_then(|response| response.context)
                .map(core_summary_to_typed)
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
        Box::pin(async move {
            let response = self
                .caller
                .context_create(&CoreContextCreateRequest { name, attributes })
                .map_err(|err| CreateContextError::Failed {
                    reason: err.to_string(),
                })?;
            Ok(ContextAck {
                id: response.context.id,
            })
        })
    }

    fn select_context<'a>(
        &'a self,
        selector: CommandContextSelector,
    ) -> Pin<
        Box<dyn Future<Output = std::result::Result<ContextAck, SelectContextError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let Some(core_selector) = command_selector_to_core(&selector) else {
                return Err(SelectContextError::Denied {
                    reason: "selector must specify either id or name".to_string(),
                });
            };
            let response = self
                .caller
                .context_select(&CoreContextSelectRequest {
                    selector: core_selector,
                })
                .map_err(|err| SelectContextError::Denied {
                    reason: err.to_string(),
                })?;
            Ok(ContextAck {
                id: response.context.id,
            })
        })
    }

    fn close_context<'a>(
        &'a self,
        selector: CommandContextSelector,
        force: bool,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<ContextAck, CloseContextError>> + Send + 'a>>
    {
        Box::pin(async move {
            let Some(core_selector) = command_selector_to_core(&selector) else {
                return Err(CloseContextError::Failed {
                    reason: "selector must specify either id or name".to_string(),
                });
            };
            let response = self
                .caller
                .context_close(&CoreContextCloseRequest {
                    selector: core_selector,
                    force,
                })
                .map_err(|err| CloseContextError::Failed {
                    reason: err.to_string(),
                })?;
            Ok(ContextAck { id: response.id })
        })
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

fn state_selector_to_core(selector: &StateContextSelector) -> Option<CoreContextSelector> {
    if let Some(id) = selector.id {
        return Some(CoreContextSelector::ById(id));
    }
    selector
        .name
        .as_ref()
        .map(|name| CoreContextSelector::ByName(name.clone()))
}

fn command_selector_to_core(selector: &CommandContextSelector) -> Option<CoreContextSelector> {
    if let Some(id) = selector.id {
        return Some(CoreContextSelector::ById(id));
    }
    selector
        .name
        .as_ref()
        .map(|name| CoreContextSelector::ByName(name.clone()))
}

fn matches_selector(
    summary: &bmux_plugin_sdk::ContextSummary,
    selector: &CoreContextSelector,
) -> bool {
    match selector {
        CoreContextSelector::ById(id) => summary.id == *id,
        CoreContextSelector::ByName(name) => summary.name.as_deref() == Some(name.as_str()),
    }
}

fn core_summary_to_typed(summary: bmux_plugin_sdk::ContextSummary) -> ContextSummary {
    ContextSummary {
        id: summary.id,
        name: summary.name,
        attributes: summary.attributes,
    }
}

bmux_plugin_sdk::export_plugin!(ContextsPlugin, include_str!("../plugin.toml"));
