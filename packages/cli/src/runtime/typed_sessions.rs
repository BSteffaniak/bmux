//! Typed helpers for dispatching `sessions-commands` and
//! `sessions-state` operations from client-side code.
//!
//! The attach client process and CLI subcommand handlers do not have a
//! local plugin host, so they cannot call the BPDL-generated typed
//! trait methods directly. These helpers route through the server's
//! generic `Request::InvokeService` envelope — the same path the
//! plugin host exposes for cross-plugin typed dispatch — so callers
//! write typed args and receive typed responses without hand-encoding
//! IPC requests.

#![allow(dead_code)] // Operations are consumed incrementally as call sites migrate.

use bmux_ipc::InvokeServiceKind;
use bmux_plugin_sdk::{CapabilityId, InterfaceId};
use bmux_sessions_plugin_api::{
    capabilities::{SESSIONS_READ, SESSIONS_WRITE},
    sessions_commands::{
        self, KillSessionError, NewSessionError, SelectSessionError, SessionAck,
        SessionSelector as CommandSessionSelector,
    },
    sessions_state::{self, SessionSelector as StateSessionSelector, SessionSummary},
};
use serde::Serialize;
use uuid::Uuid;

use super::typed_service::{InvokeError, invoke_with};

/// Capability guarding the sessions plugin's mutating command surface.
pub const SESSIONS_WRITE_CAPABILITY: CapabilityId = SESSIONS_WRITE;

/// Capability guarding the sessions plugin's query surface.
pub const SESSIONS_READ_CAPABILITY: CapabilityId = SESSIONS_READ;

/// Interface id for the sessions plugin's mutating command surface.
pub const SESSIONS_COMMANDS_INTERFACE: InterfaceId = sessions_commands::INTERFACE_ID;

/// Interface id for the sessions plugin's query surface.
pub const SESSIONS_STATE_INTERFACE: InterfaceId = sessions_state::INTERFACE_ID;

// ── Typed argument structs ──────────────────────────────────────────

/// Wire-format argument for the typed `new-session` command. Mirrors
/// the BPDL parameter list.
#[derive(Debug, Clone, Serialize)]
pub struct NewSessionArgs {
    pub name: Option<String>,
}

/// Wire-format argument for the typed `kill-session` command.
#[derive(Debug, Clone, Serialize)]
pub struct KillSessionArgs {
    pub selector: CommandSessionSelector,
    pub force_local: bool,
}

/// Wire-format argument for the typed `select-session` command.
#[derive(Debug, Clone, Serialize)]
pub struct SelectSessionArgs {
    pub selector: CommandSessionSelector,
}

/// Wire-format argument for the typed `get-session` query.
#[derive(Debug, Clone, Serialize)]
pub struct GetSessionArgs {
    pub selector: StateSessionSelector,
}

// ── Selector convenience constructors ───────────────────────────────

/// Construct a command-interface selector addressing a session by id.
#[must_use]
pub const fn command_selector_by_id(id: Uuid) -> CommandSessionSelector {
    CommandSessionSelector {
        id: Some(id),
        name: None,
    }
}

/// Construct a command-interface selector addressing a session by name.
#[must_use]
pub const fn command_selector_by_name(name: String) -> CommandSessionSelector {
    CommandSessionSelector {
        id: None,
        name: Some(name),
    }
}

/// Translate an IPC `SessionSelector` to the typed command-interface
/// selector used on the typed wire path.
#[must_use]
pub fn from_ipc_selector(selector: bmux_ipc::SessionSelector) -> CommandSessionSelector {
    match selector {
        bmux_ipc::SessionSelector::ById(id) => command_selector_by_id(id),
        bmux_ipc::SessionSelector::ByName(name) => command_selector_by_name(name),
    }
}

// ── Convenience wrappers over invoke_service_raw ────────────────────

/// Invoke a `sessions-commands` typed command on an arbitrary client
/// type (either `BmuxClient` or `StreamingBmuxClient`) via the generic
/// service-dispatch envelope.
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

/// Invoke a `sessions-state` typed query on an arbitrary client type.
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

// ── Typed response re-exports ───────────────────────────────────────

/// Response returned by the typed `new-session` / `kill-session` /
/// `select-session` commands.
pub type SessionAckResponse = Result<SessionAck, NewSessionError>;

/// Response returned by the typed `kill-session` command.
pub type KillSessionResponse = Result<SessionAck, KillSessionError>;

/// Response returned by the typed `select-session` command.
pub type SelectSessionResponse = Result<SessionAck, SelectSessionError>;

/// Re-export of the typed session list for callers that want to write
/// `Vec<SessionSummary>` without pulling the plugin-api crate in
/// directly.
pub type SessionsList = Vec<SessionSummary>;

/// Re-export of the typed session summary for the same reason.
pub type SessionSummaryResponse = SessionSummary;

/// Always-use this constant rather than hand-typing the string —
/// operation names are BPDL command identifiers.
pub const OP_NEW_SESSION: &str = "new-session";
pub const OP_KILL_SESSION: &str = "kill-session";
pub const OP_SELECT_SESSION: &str = "select-session";
pub const OP_LIST_SESSIONS: &str = "list-sessions";
pub const OP_GET_SESSION: &str = "get-session";

/// Kind for sessions-state queries. Passed to `invoke_service_raw`.
pub const QUERY_KIND: InvokeServiceKind = InvokeServiceKind::Query;
/// Kind for sessions-commands mutations. Passed to `invoke_service_raw`.
pub const COMMAND_KIND: InvokeServiceKind = InvokeServiceKind::Command;
