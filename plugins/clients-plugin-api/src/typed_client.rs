//! Typed-client helpers for the `bmux.clients` plugin.
//!
//! These free functions accept any `C: TypedDispatchClient` and
//! encapsulate the encode → `invoke_service_raw` → decode cycle so
//! callers (`packages/cli` today, any downstream integration
//! tomorrow) don't have to duplicate interface/operation strings and
//! serde boilerplate.
//!
//! The helpers are intentionally independent of `bmux_client` — they
//! only depend on the narrow `TypedDispatchClient` surface in
//! `bmux_plugin_sdk`. Anything that implements the trait (including
//! test fakes) can drive them.

use bmux_ipc::InvokeServiceKind;
use bmux_plugin_sdk::{TypedDispatchClient, TypedDispatchClientError};
use serde::Serialize;
use uuid::Uuid;

use crate::capabilities::{CLIENTS_READ, CLIENTS_WRITE};
use crate::clients_commands::{self, ClientAck, SetFollowingError};
use crate::clients_state::{self, ClientQueryError, ClientSummary};

/// Errors returned by clients-plugin typed-client helpers.
#[derive(Debug, thiserror::Error)]
pub enum ClientsTypedClientError {
    #[error(transparent)]
    Dispatch(#[from] TypedDispatchClientError),
    #[error("failed to encode {op}: {details}")]
    Encode { op: &'static str, details: String },
    #[error("failed to decode {op}: {details}")]
    Decode { op: &'static str, details: String },
    #[error("server reported no current client")]
    NoCurrentClient,
    #[error("clients query error: {0:?}")]
    Query(ClientQueryError),
    #[error("set-following rejected: {0:?}")]
    SetFollowing(SetFollowingError),
}

type Result<T> = core::result::Result<T, ClientsTypedClientError>;

/// Return the server-assigned client UUID for this connection.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side query
/// fails.
pub async fn whoami<C: TypedDispatchClient>(client: &mut C) -> Result<Uuid> {
    let payload = bmux_ipc::encode(&()).map_err(|err| ClientsTypedClientError::Encode {
        op: "current-client",
        details: err.to_string(),
    })?;
    let response_bytes = client
        .invoke_service_raw(
            CLIENTS_READ.as_str(),
            InvokeServiceKind::Query,
            clients_state::INTERFACE_ID.as_str(),
            "current-client",
            payload,
        )
        .await?;
    let result: core::result::Result<ClientSummary, ClientQueryError> =
        bmux_ipc::decode(&response_bytes).map_err(|err| ClientsTypedClientError::Decode {
            op: "current-client",
            details: err.to_string(),
        })?;
    match result {
        Ok(summary) => Ok(summary.id),
        Err(err) => Err(ClientsTypedClientError::Query(err)),
    }
}

/// Follow another client's active session focus.
///
/// # Errors
///
/// Returns an error if transport fails or the server rejects the
/// follow request.
pub async fn follow_client<C: TypedDispatchClient>(
    client: &mut C,
    target_client_id: Uuid,
    global: bool,
) -> Result<()> {
    set_following(client, Some(target_client_id), global).await
}

/// Stop following any current follow target.
///
/// # Errors
///
/// Returns an error if transport fails or the server rejects the
/// unfollow request.
pub async fn unfollow<C: TypedDispatchClient>(client: &mut C) -> Result<()> {
    set_following(client, None, false).await
}

async fn set_following<C: TypedDispatchClient>(
    client: &mut C,
    target_client_id: Option<Uuid>,
    global: bool,
) -> Result<()> {
    #[derive(Serialize)]
    struct SetFollowingArgs {
        target_client_id: Option<Uuid>,
        global: bool,
    }
    let payload = bmux_ipc::encode(&SetFollowingArgs {
        target_client_id,
        global,
    })
    .map_err(|err| ClientsTypedClientError::Encode {
        op: "set-following",
        details: err.to_string(),
    })?;
    let response_bytes = client
        .invoke_service_raw(
            CLIENTS_WRITE.as_str(),
            InvokeServiceKind::Command,
            clients_commands::INTERFACE_ID.as_str(),
            "set-following",
            payload,
        )
        .await?;
    let result: core::result::Result<ClientAck, SetFollowingError> =
        bmux_ipc::decode(&response_bytes).map_err(|err| ClientsTypedClientError::Decode {
            op: "set-following",
            details: err.to_string(),
        })?;
    match result {
        Ok(_ack) => Ok(()),
        Err(err) => Err(ClientsTypedClientError::SetFollowing(err)),
    }
}
