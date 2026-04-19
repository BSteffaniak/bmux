//! Typed helpers for dispatching `clients-commands` and
//! `clients-state` operations from client-side code.

#![allow(dead_code)] // Operations are consumed incrementally as call sites migrate.

use bmux_clients_plugin_api::{
    capabilities::{CLIENTS_READ, CLIENTS_WRITE},
    clients_commands::{self, ClientAck, SetCurrentSessionError, SetFollowingError},
    clients_state::{self, ClientSummary},
};
use bmux_ipc::InvokeServiceKind;
use bmux_plugin_sdk::{CapabilityId, InterfaceId};
use serde::Serialize;
use uuid::Uuid;

use super::typed_service::{InvokeError, invoke_with};

pub const CLIENTS_READ_CAPABILITY: CapabilityId = CLIENTS_READ;
pub const CLIENTS_WRITE_CAPABILITY: CapabilityId = CLIENTS_WRITE;
pub const CLIENTS_COMMANDS_INTERFACE: InterfaceId = clients_commands::INTERFACE_ID;
pub const CLIENTS_STATE_INTERFACE: InterfaceId = clients_state::INTERFACE_ID;

// ── Typed argument structs ──────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct SetCurrentSessionArgs {
    pub session_id: Uuid,
}

#[derive(Debug, Clone, Serialize)]
pub struct SetFollowingArgs {
    pub target_client_id: Option<Uuid>,
    pub global: bool,
}

// ── Convenience wrappers ────────────────────────────────────────────

#[allow(clippy::future_not_send)]
pub async fn invoke_command<F, Fut, Req, Resp>(
    operation: &str,
    args: &Req,
    invoke: F,
) -> Result<Resp, InvokeError>
where
    Req: Serialize + Sync,
    Resp: for<'de> serde::Deserialize<'de>,
    F: FnOnce(Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<u8>, String>>,
{
    invoke_with(operation, args, invoke).await
}

#[allow(clippy::future_not_send)]
pub async fn invoke_query<F, Fut, Req, Resp>(
    operation: &str,
    args: &Req,
    invoke: F,
) -> Result<Resp, InvokeError>
where
    Req: Serialize + Sync,
    Resp: for<'de> serde::Deserialize<'de>,
    F: FnOnce(Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<u8>, String>>,
{
    invoke_with(operation, args, invoke).await
}

// ── Response re-exports ─────────────────────────────────────────────

pub type ClientsList = Vec<ClientSummary>;
pub type ClientSummaryResponse = ClientSummary;
pub type SetCurrentSessionResponse = Result<ClientAck, SetCurrentSessionError>;
pub type SetFollowingResponse = Result<ClientAck, SetFollowingError>;

pub const OP_LIST_CLIENTS: &str = "list-clients";
pub const OP_CURRENT_CLIENT: &str = "current-client";
pub const OP_SET_CURRENT_SESSION: &str = "set-current-session";
pub const OP_SET_FOLLOWING: &str = "set-following";

pub const QUERY_KIND: InvokeServiceKind = InvokeServiceKind::Query;
pub const COMMAND_KIND: InvokeServiceKind = InvokeServiceKind::Command;
