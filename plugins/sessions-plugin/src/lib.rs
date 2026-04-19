//! bmux sessions plugin — typed owner of session lifecycle.
//!
//! Provides typed services for other plugins and attach-side callers
//! to list, create, kill, and select sessions without going through
//! the legacy `bmux_client::BmuxClient::session_*` methods.
//!
//! # Current phase: typed facade over `HostRuntimeApi::session_*`
//!
//! In this phase the sessions-plugin does not own session state
//! directly. Its typed service handles delegate to the core host
//! runtime via [`bmux_plugin::TypedServiceCaller`] and return the
//! results through the BPDL-generated types. Once callers are all
//! migrated off `BmuxClient::session_*` and the core host-runtime's
//! session methods are deleted (M4 Stage 8), the sessions-plugin
//! will re-home the underlying state.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_plugin::{HostRuntimeApi, TypedServiceCaller};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{
    HostScope, SessionCreateRequest as CoreSessionCreateRequest,
    SessionKillRequest as CoreSessionKillRequest, SessionSelectRequest as CoreSessionSelectRequest,
    SessionSelector as CoreSessionSelector, TypedServiceRegistrationContext, TypedServiceRegistry,
};
use bmux_sessions_plugin_api::sessions_commands::{
    self, KillSessionError, NewSessionError, SelectSessionError, SessionAck,
    SessionSelector as CommandSessionSelector, SessionsCommandsService,
};
use bmux_sessions_plugin_api::sessions_state::{
    self, SessionQueryError, SessionSelector as StateSessionSelector, SessionSummary,
    SessionsStateService,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Top-level plugin type. Holds no state today (the runtime data lives
/// in the core host runtime that this plugin delegates to); the type
/// exists so the SDK's `export_plugin!` macro has something to own.
#[derive(Default)]
pub struct SessionsPlugin;

impl RustPlugin for SessionsPlugin {
    fn run_command(
        &mut self,
        _context: NativeCommandContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        // The sessions plugin doesn't expose CLI commands in this
        // phase; session management is still driven by the core
        // `bmux session *` subcommands until Stage 6 migrates those
        // onto plugin-owned commands.
        Err(PluginCommandError::unknown_command(""))
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        // Both typed dispatch and byte dispatch flow through the same
        // handles, but the byte-dispatch surface for sessions has no
        // callers right now (unlike windows-commands, which is invoked
        // over the wire from the attach runtime). Return
        // `unsupported_service_operation` uniformly so the plugin
        // host's generic fallback kicks in.
        ServiceResponse::error(
            "unsupported_service_operation",
            format!(
                "sessions plugin has no byte-dispatch service handler for '{}:{}'",
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

// ── Typed state (query) handle ───────────────────────────────────────

/// Typed implementation of [`SessionsStateService`].
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
            self.caller.session_list().map_or_else(
                |_| Vec::new(),
                |response| {
                    response
                        .sessions
                        .into_iter()
                        .map(core_summary_to_typed)
                        .collect()
                },
            )
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
            let Some(core_selector) = state_selector_to_core(&selector) else {
                return Err(SessionQueryError::InvalidSelector {
                    reason: "selector must specify either id or name".to_string(),
                });
            };
            // The core host runtime has no selector-targeted `get`
            // operation; list and filter client-side. That's a
            // temporary shim until Stage 8 re-homes the state into
            // this plugin directly.
            let response =
                self.caller
                    .session_list()
                    .map_err(|err| SessionQueryError::InvalidSelector {
                        reason: err.to_string(),
                    })?;
            response
                .sessions
                .into_iter()
                .find(|summary| matches_selector(summary, &core_selector))
                .map(core_summary_to_typed)
                .ok_or(SessionQueryError::NotFound)
        })
    }
}

// ── Typed commands handle ────────────────────────────────────────────

/// Typed implementation of [`SessionsCommandsService`].
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
        Box::pin(async move {
            let response = self
                .caller
                .session_create(&CoreSessionCreateRequest { name })
                .map_err(|err| NewSessionError::Failed {
                    reason: err.to_string(),
                })?;
            Ok(SessionAck { id: response.id })
        })
    }

    fn kill_session<'a>(
        &'a self,
        selector: CommandSessionSelector,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<SessionAck, KillSessionError>> + Send + 'a>>
    {
        Box::pin(async move {
            let Some(core_selector) = command_selector_to_core(&selector) else {
                return Err(KillSessionError::Failed {
                    reason: "selector must specify either id or name".to_string(),
                });
            };
            let response = self
                .caller
                .session_kill(&CoreSessionKillRequest {
                    selector: core_selector,
                    force_local: false,
                })
                .map_err(|err| KillSessionError::Failed {
                    reason: err.to_string(),
                })?;
            Ok(SessionAck { id: response.id })
        })
    }

    fn select_session<'a>(
        &'a self,
        selector: CommandSessionSelector,
    ) -> Pin<
        Box<dyn Future<Output = std::result::Result<SessionAck, SelectSessionError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let Some(core_selector) = command_selector_to_core(&selector) else {
                return Err(SelectSessionError::Denied {
                    reason: "selector must specify either id or name".to_string(),
                });
            };
            let response = self
                .caller
                .session_select(&CoreSessionSelectRequest {
                    selector: core_selector,
                })
                .map_err(|err| SelectSessionError::Denied {
                    reason: err.to_string(),
                })?;
            Ok(SessionAck {
                id: response.session_id,
            })
        })
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

fn state_selector_to_core(selector: &StateSessionSelector) -> Option<CoreSessionSelector> {
    if let Some(id) = selector.id {
        return Some(CoreSessionSelector::ById(id));
    }
    selector
        .name
        .as_ref()
        .map(|name| CoreSessionSelector::ByName(name.clone()))
}

fn command_selector_to_core(selector: &CommandSessionSelector) -> Option<CoreSessionSelector> {
    if let Some(id) = selector.id {
        return Some(CoreSessionSelector::ById(id));
    }
    selector
        .name
        .as_ref()
        .map(|name| CoreSessionSelector::ByName(name.clone()))
}

fn matches_selector(
    summary: &bmux_plugin_sdk::SessionSummary,
    selector: &CoreSessionSelector,
) -> bool {
    match selector {
        CoreSessionSelector::ById(id) => summary.id == *id,
        CoreSessionSelector::ByName(name) => summary.name.as_deref() == Some(name.as_str()),
    }
}

fn core_summary_to_typed(summary: bmux_plugin_sdk::SessionSummary) -> SessionSummary {
    SessionSummary {
        id: summary.id,
        name: summary.name,
        // `client_count` on the core type is `usize`; the BPDL typed
        // record uses `u32`. Saturating cast is fine — realistic
        // client counts never come close to `u32::MAX`.
        client_count: u32::try_from(summary.client_count).unwrap_or(u32::MAX),
    }
}

bmux_plugin_sdk::export_plugin!(SessionsPlugin, include_str!("../plugin.toml"));
