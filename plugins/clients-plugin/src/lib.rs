//! bmux clients plugin — typed owner of per-client identity and view state.
//!
//! Provides typed services that reach the server's client state
//! directly via the IPC kernel-bridge escape hatch
//! (`ServiceCaller::execute_kernel_request`).

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_clients_plugin_api::clients_commands::{
    self, ClientAck, ClientsCommandsService, SetCurrentSessionError, SetFollowingError,
};
use bmux_clients_plugin_api::clients_state::{
    self, ClientQueryError, ClientSummary, ClientsStateService,
};
use bmux_plugin::{ServiceCaller, TypedServiceCaller};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{HostScope, TypedServiceRegistrationContext, TypedServiceRegistry};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use uuid::Uuid;

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
    fn run_command(
        &mut self,
        _context: NativeCommandContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        Err(PluginCommandError::unknown_command(""))
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        bmux_plugin_sdk::route_service!(context, {
            "clients-state", "list-clients" => |_req: (), ctx| {
                list_clients_via_ipc(ctx)
                    .map_err(|e| ServiceResponse::error("list_failed", e))
            },
            "clients-state", "current-client" => |_req: (), ctx| {
                Ok::<Result<ClientSummary, ClientQueryError>, ServiceResponse>(
                    current_client_via_ipc(ctx)
                )
            },
            "clients-commands", "set-current-session" => |_req: SetCurrentSessionArgs, _ctx| {
                Ok::<Result<ClientAck, SetCurrentSessionError>, ServiceResponse>(
                    Err(SetCurrentSessionError::Denied {
                        reason: "set-current-session is not wired into the core runtime yet"
                            .to_string(),
                    })
                )
            },
            "clients-commands", "set-following" => |_req: SetFollowingArgs, _ctx| {
                Ok::<Result<ClientAck, SetFollowingError>, ServiceResponse>(
                    Err(SetFollowingError::Denied {
                        reason: "set-following is not wired into the core runtime yet"
                            .to_string(),
                    })
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

fn list_clients_via_ipc(caller: &impl ServiceCaller) -> Result<Vec<ClientSummary>, String> {
    match caller.execute_kernel_request(bmux_ipc::Request::ListClients) {
        Ok(bmux_ipc::ResponsePayload::ClientList { clients }) => {
            Ok(clients.iter().map(ipc_summary_to_typed).collect())
        }
        Ok(_) => Err("unexpected response payload for list-clients".to_string()),
        Err(err) => Err(err.to_string()),
    }
}

fn current_client_via_ipc(caller: &impl ServiceCaller) -> Result<ClientSummary, ClientQueryError> {
    let Ok(bmux_ipc::ResponsePayload::ClientIdentity { id: self_id }) =
        caller.execute_kernel_request(bmux_ipc::Request::WhoAmI)
    else {
        return Err(ClientQueryError::NoCurrentClient);
    };
    let Ok(bmux_ipc::ResponsePayload::ClientList { clients }) =
        caller.execute_kernel_request(bmux_ipc::Request::ListClients)
    else {
        return Err(ClientQueryError::NoCurrentClient);
    };
    clients
        .iter()
        .find(|entry| entry.id == self_id)
        .map(ipc_summary_to_typed)
        .ok_or(ClientQueryError::NotFound)
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
        Box::pin(async move { list_clients_via_ipc(self.caller.as_ref()).unwrap_or_default() })
    }

    fn current_client<'a>(
        &'a self,
    ) -> Pin<
        Box<dyn Future<Output = std::result::Result<ClientSummary, ClientQueryError>> + Send + 'a>,
    > {
        Box::pin(async move { current_client_via_ipc(self.caller.as_ref()) })
    }
}

pub struct ClientsCommandsHandle {
    _caller: Arc<TypedServiceCaller>,
}

impl ClientsCommandsHandle {
    const fn new(caller: Arc<TypedServiceCaller>) -> Self {
        Self { _caller: caller }
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
        _target_client_id: Option<Uuid>,
        _global: bool,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<ClientAck, SetFollowingError>> + Send + 'a>>
    {
        Box::pin(async move {
            Err(SetFollowingError::Denied {
                reason: "set-following is not wired into the core runtime yet".to_string(),
            })
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
