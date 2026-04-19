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

use bmux_plugin::{ServiceCaller, TypedServiceCaller};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{HostScope, TypedServiceRegistrationContext, TypedServiceRegistry};
use bmux_sessions_plugin_api::sessions_commands::{
    self, KillSessionError, NewSessionError, SelectSessionError, SessionAck,
    SessionSelector as CommandSessionSelector, SessionsCommandsService,
};
use bmux_sessions_plugin_api::sessions_state::{
    self, SessionQueryError, SessionSelector as StateSessionSelector, SessionSummary,
    SessionsStateService,
};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

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
    fn run_command(
        &mut self,
        _context: NativeCommandContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        Err(PluginCommandError::unknown_command(""))
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        bmux_plugin_sdk::route_service!(context, {
            "sessions-state", "list-sessions" => |_req: (), ctx| {
                list_sessions_via_ipc(ctx)
                    .map_err(|e| ServiceResponse::error("list_failed", e))
            },
            "sessions-state", "get-session" => |req: SelectorArgs, ctx| {
                get_session_via_ipc(ctx, &req.selector)
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

// ── IPC helpers ──────────────────────────────────────────────────────

fn list_sessions_via_ipc(caller: &impl ServiceCaller) -> Result<Vec<SessionSummary>, String> {
    let response = caller
        .execute_kernel_request(bmux_ipc::Request::ListSessions)
        .map_err(|err| err.to_string())?;
    match response {
        bmux_ipc::ResponsePayload::SessionList { sessions } => {
            Ok(sessions.into_iter().map(ipc_summary_to_typed).collect())
        }
        _ => Err("unexpected response payload for list-sessions".to_string()),
    }
}

fn get_session_via_ipc(
    caller: &impl ServiceCaller,
    selector: &WireSelector,
) -> Result<Result<SessionSummary, SessionQueryError>, String> {
    let Some(ipc_selector) = selector.to_ipc() else {
        return Ok(Err(SessionQueryError::InvalidSelector {
            reason: "selector must specify either id or name".to_string(),
        }));
    };
    let response = caller
        .execute_kernel_request(bmux_ipc::Request::ListSessions)
        .map_err(|err| err.to_string())?;
    match response {
        bmux_ipc::ResponsePayload::SessionList { sessions } => Ok(sessions
            .into_iter()
            .find(|summary| matches_selector(summary, &ipc_selector))
            .map(ipc_summary_to_typed)
            .ok_or(SessionQueryError::NotFound)),
        _ => Err("unexpected response payload for list-sessions".to_string()),
    }
}

fn new_session_via_ipc(
    caller: &impl ServiceCaller,
    name: Option<String>,
) -> Result<SessionAck, NewSessionError> {
    match caller.execute_kernel_request(bmux_ipc::Request::NewSession { name }) {
        Ok(bmux_ipc::ResponsePayload::SessionCreated { id, .. }) => Ok(SessionAck { id }),
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
        Ok(bmux_ipc::ResponsePayload::SessionKilled { id }) => Ok(SessionAck { id }),
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
        Ok(bmux_ipc::ResponsePayload::Attached { grant }) => Ok(SessionAck {
            id: grant.session_id,
        }),
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
        Box::pin(async move { list_sessions_via_ipc(self.caller.as_ref()).unwrap_or_default() })
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
            let wire = WireSelector {
                id: selector.id,
                name: selector.name,
            };
            match get_session_via_ipc(self.caller.as_ref(), &wire) {
                Ok(result) => result,
                Err(reason) => Err(SessionQueryError::InvalidSelector { reason }),
            }
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
}

// ── Helpers ─────────────────────────────────────────────────────────

fn matches_selector(
    summary: &bmux_ipc::SessionSummary,
    selector: &bmux_ipc::SessionSelector,
) -> bool {
    match selector {
        bmux_ipc::SessionSelector::ById(id) => summary.id == *id,
        bmux_ipc::SessionSelector::ByName(name) => summary.name.as_deref() == Some(name.as_str()),
    }
}

fn ipc_summary_to_typed(summary: bmux_ipc::SessionSummary) -> SessionSummary {
    SessionSummary {
        id: summary.id,
        name: summary.name,
        client_count: u32::try_from(summary.client_count).unwrap_or(u32::MAX),
    }
}

bmux_plugin_sdk::export_plugin!(SessionsPlugin, include_str!("../plugin.toml"));
