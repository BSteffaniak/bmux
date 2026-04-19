//! Typed helpers for dispatching `contexts-commands` and
//! `contexts-state` operations from client-side code.

#![allow(dead_code)] // Operations are consumed incrementally as call sites migrate.

use bmux_contexts_plugin_api::{
    capabilities::{CONTEXTS_READ, CONTEXTS_WRITE},
    contexts_commands::{
        self, CloseContextError, ContextAck, ContextSelector as CommandContextSelector,
        CreateContextError, SelectContextError,
    },
    contexts_state::{self, ContextSelector as StateContextSelector, ContextSummary},
};
use bmux_ipc::InvokeServiceKind;
use bmux_plugin_sdk::{CapabilityId, InterfaceId};
use serde::Serialize;
use std::collections::BTreeMap;
use uuid::Uuid;

use super::typed_service::{InvokeError, invoke_with};

pub const CONTEXTS_READ_CAPABILITY: CapabilityId = CONTEXTS_READ;
pub const CONTEXTS_WRITE_CAPABILITY: CapabilityId = CONTEXTS_WRITE;
pub const CONTEXTS_COMMANDS_INTERFACE: InterfaceId = contexts_commands::INTERFACE_ID;
pub const CONTEXTS_STATE_INTERFACE: InterfaceId = contexts_state::INTERFACE_ID;

// ── Typed argument structs ──────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct CreateContextArgs {
    pub name: Option<String>,
    pub attributes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SelectContextArgs {
    pub selector: CommandContextSelector,
}

#[derive(Debug, Clone, Serialize)]
pub struct CloseContextArgs {
    pub selector: CommandContextSelector,
    pub force: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct GetContextArgs {
    pub selector: StateContextSelector,
}

// ── Selector convenience constructors ───────────────────────────────

#[must_use]
pub const fn command_selector_by_id(id: Uuid) -> CommandContextSelector {
    CommandContextSelector {
        id: Some(id),
        name: None,
    }
}

#[must_use]
pub const fn command_selector_by_name(name: String) -> CommandContextSelector {
    CommandContextSelector {
        id: None,
        name: Some(name),
    }
}

#[must_use]
pub fn from_ipc_selector(selector: bmux_ipc::ContextSelector) -> CommandContextSelector {
    match selector {
        bmux_ipc::ContextSelector::ById(id) => command_selector_by_id(id),
        bmux_ipc::ContextSelector::ByName(name) => command_selector_by_name(name),
    }
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

pub type CreateContextResponse = Result<ContextAck, CreateContextError>;
pub type SelectContextResponse = Result<ContextAck, SelectContextError>;
pub type CloseContextResponse = Result<ContextAck, CloseContextError>;
pub type ContextsList = Vec<ContextSummary>;
pub type CurrentContextResponse = Option<ContextSummary>;
pub type ContextSummaryResponse = ContextSummary;

pub const OP_LIST_CONTEXTS: &str = "list-contexts";
pub const OP_GET_CONTEXT: &str = "get-context";
pub const OP_CURRENT_CONTEXT: &str = "current-context";
pub const OP_CREATE_CONTEXT: &str = "create-context";
pub const OP_SELECT_CONTEXT: &str = "select-context";
pub const OP_CLOSE_CONTEXT: &str = "close-context";

pub const QUERY_KIND: InvokeServiceKind = InvokeServiceKind::Query;
pub const COMMAND_KIND: InvokeServiceKind = InvokeServiceKind::Command;
