//! Typed-client helpers for the `bmux.contexts` plugin.
//!
//! Free functions accepting any [`TypedDispatchClient`] wrap the
//! contexts plugin's BPDL service calls so callers don't have to repeat
//! capability, interface, operation, and serde boilerplate.

use bmux_ipc::InvokeServiceKind;
use bmux_plugin_sdk::{TypedDispatchClient, TypedDispatchClientError};
use serde::Serialize;
use std::collections::BTreeMap;

use crate::capabilities::{CONTEXTS_READ, CONTEXTS_WRITE};
use crate::{contexts_commands, contexts_state};

/// Errors returned by contexts-plugin typed-client helpers.
#[derive(Debug, thiserror::Error)]
pub enum ContextsTypedClientError {
    #[error(transparent)]
    Dispatch(#[from] TypedDispatchClientError),
    #[error("failed to encode {op}: {details}")]
    Encode { op: String, details: String },
    #[error("failed to decode {op}: {details}")]
    Decode { op: String, details: String },
}

type Result<T> = core::result::Result<T, ContextsTypedClientError>;

async fn invoke<C, Req, Resp>(
    client: &mut C,
    capability: String,
    kind: InvokeServiceKind,
    interface: String,
    operation: String,
    request: &Req,
) -> Result<Resp>
where
    C: TypedDispatchClient,
    Req: Serialize,
    Resp: serde::de::DeserializeOwned,
{
    let payload = bmux_ipc::encode(request).map_err(|err| ContextsTypedClientError::Encode {
        op: operation.clone(),
        details: err.to_string(),
    })?;
    let response_bytes = client
        .invoke_service_raw(&capability, kind, &interface, &operation, payload)
        .await?;
    bmux_ipc::decode(&response_bytes).map_err(|err| ContextsTypedClientError::Decode {
        op: operation,
        details: err.to_string(),
    })
}

/// List all contexts visible to the caller.
///
/// # Errors
///
/// Returns an error if transport, encoding, or response decoding fails.
pub async fn list_contexts<C: TypedDispatchClient>(
    client: &mut C,
) -> Result<Vec<contexts_state::ContextSummary>> {
    invoke(
        client,
        CONTEXTS_READ.as_str().to_string(),
        InvokeServiceKind::Query,
        contexts_state::INTERFACE_ID.as_str().to_string(),
        contexts_state::OP_LIST_CONTEXTS.as_str().to_string(),
        &(),
    )
    .await
}

/// Fetch the caller's current context, if one is selected.
///
/// # Errors
///
/// Returns an error if transport, encoding, or response decoding fails.
pub async fn current_context<C: TypedDispatchClient>(
    client: &mut C,
) -> Result<Option<contexts_state::ContextSummary>> {
    invoke(
        client,
        CONTEXTS_READ.as_str().to_string(),
        InvokeServiceKind::Query,
        contexts_state::INTERFACE_ID.as_str().to_string(),
        contexts_state::OP_CURRENT_CONTEXT.as_str().to_string(),
        &(),
    )
    .await
}

/// Create a new context.
///
/// # Errors
///
/// Returns an error if transport, encoding, or response decoding fails.
pub async fn create_context<C: TypedDispatchClient>(
    client: &mut C,
    name: Option<String>,
    attributes: BTreeMap<String, String>,
) -> Result<
    core::result::Result<contexts_commands::ContextAck, contexts_commands::CreateContextError>,
> {
    #[derive(Serialize)]
    struct Args {
        name: Option<String>,
        attributes: BTreeMap<String, String>,
    }

    invoke(
        client,
        CONTEXTS_WRITE.as_str().to_string(),
        InvokeServiceKind::Command,
        contexts_commands::INTERFACE_ID.as_str().to_string(),
        contexts_commands::OP_CREATE_CONTEXT.as_str().to_string(),
        &Args { name, attributes },
    )
    .await
}

/// Select an existing context.
///
/// # Errors
///
/// Returns an error if transport, encoding, or response decoding fails.
pub async fn select_context<C: TypedDispatchClient>(
    client: &mut C,
    selector: contexts_state::ContextSelector,
) -> Result<
    core::result::Result<contexts_commands::ContextAck, contexts_commands::SelectContextError>,
> {
    #[derive(Serialize)]
    struct Args {
        selector: contexts_state::ContextSelector,
    }

    invoke(
        client,
        CONTEXTS_WRITE.as_str().to_string(),
        InvokeServiceKind::Command,
        contexts_commands::INTERFACE_ID.as_str().to_string(),
        contexts_commands::OP_SELECT_CONTEXT.as_str().to_string(),
        &Args { selector },
    )
    .await
}

/// Close an existing context.
///
/// # Errors
///
/// Returns an error if transport, encoding, or response decoding fails.
pub async fn close_context<C: TypedDispatchClient>(
    client: &mut C,
    selector: contexts_state::ContextSelector,
    force: bool,
) -> Result<core::result::Result<contexts_commands::ContextAck, contexts_commands::CloseContextError>>
{
    #[derive(Serialize)]
    struct Args {
        selector: contexts_state::ContextSelector,
        force: bool,
    }

    invoke(
        client,
        CONTEXTS_WRITE.as_str().to_string(),
        InvokeServiceKind::Command,
        contexts_commands::INTERFACE_ID.as_str().to_string(),
        contexts_commands::OP_CLOSE_CONTEXT.as_str().to_string(),
        &Args { selector, force },
    )
    .await
}
