//! Typed-client helpers for the `bmux.control_catalog` plugin.
//!
//! Free functions accepting any `C: TypedDispatchClient` that wrap
//! the `control-catalog-state::snapshot` typed query so callers
//! don't have to repeat the interface/operation strings + serde
//! boilerplate.
//!
//! This helper also contains the translation from the plugin-native
//! `control_catalog_state::Snapshot` shape (BPDL-generated) to the
//! wire-stable `bmux_ipc::ControlCatalogSnapshot` shape used by
//! cross-process attach clients. The translation was historically
//! done inside `bmux_client::StreamingBmuxClient::control_catalog_snapshot`;
//! it lives here now so `bmux_client` doesn't name the plugin-api
//! types at all.

use std::collections::BTreeMap;

use bmux_ipc::{
    ContextSessionBindingSummary, ContextSummary, ControlCatalogSnapshot, InvokeServiceKind,
    SessionSummary,
};
use bmux_plugin_sdk::{TypedDispatchClient, TypedDispatchClientError};
use serde::Serialize;

use crate::capabilities::CATALOG_READ;
use crate::control_catalog_state::{self, Snapshot};

/// Errors returned by control-catalog typed-client helpers.
#[derive(Debug, thiserror::Error)]
pub enum ControlCatalogTypedClientError {
    #[error(transparent)]
    Dispatch(#[from] TypedDispatchClientError),
    #[error("failed to encode catalog snapshot request: {0}")]
    Encode(String),
    #[error("failed to decode catalog snapshot response: {0}")]
    Decode(String),
}

type Result<T> = core::result::Result<T, ControlCatalogTypedClientError>;

/// Request a snapshot of the control catalog from the
/// `bmux.control_catalog` plugin. If `since_revision` is supplied the
/// server may return an unchanged-since response (the caller should
/// still accept a full snapshot).
///
/// The returned [`ControlCatalogSnapshot`] is the wire-stable shape
/// used by attach UIs; this helper translates the plugin-native
/// [`Snapshot`] record into it.
///
/// # Errors
///
/// Returns an error if transport, encoding, or decoding fails.
pub async fn control_catalog_snapshot<C: TypedDispatchClient>(
    client: &mut C,
    since_revision: Option<u64>,
) -> Result<ControlCatalogSnapshot> {
    #[derive(Serialize)]
    struct Args {
        since_revision: Option<u64>,
    }
    let payload = bmux_ipc::encode(&Args { since_revision })
        .map_err(|e| ControlCatalogTypedClientError::Encode(e.to_string()))?;
    let response_bytes = client
        .invoke_service_raw(
            CATALOG_READ.as_str(),
            InvokeServiceKind::Query,
            control_catalog_state::INTERFACE_ID.as_str(),
            "snapshot",
            payload,
        )
        .await?;
    let typed: Snapshot = bmux_ipc::decode(&response_bytes)
        .map_err(|e| ControlCatalogTypedClientError::Decode(e.to_string()))?;
    Ok(map_snapshot(typed))
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn map_snapshot(snapshot: Snapshot) -> ControlCatalogSnapshot {
    let sessions = snapshot
        .sessions
        .into_iter()
        .map(|row| SessionSummary {
            id: row.id,
            name: row.name,
            client_count: row.client_count as usize,
        })
        .collect::<Vec<_>>();

    let context_session_bindings = snapshot
        .context_session_bindings
        .iter()
        .map(|b| ContextSessionBindingSummary {
            context_id: b.context_id,
            session_id: b.session_id,
        })
        .collect::<Vec<_>>();

    let binding_by_context = snapshot
        .context_session_bindings
        .iter()
        .map(|b| (b.context_id, b.session_id))
        .collect::<BTreeMap<_, _>>();

    let contexts = snapshot
        .contexts
        .into_iter()
        .map(|row| {
            let mut attributes = BTreeMap::new();
            if let Some(session_id) = binding_by_context.get(&row.id) {
                attributes.insert("bmux.session_id".to_string(), session_id.to_string());
            }
            ContextSummary {
                id: row.id,
                name: row.name,
                attributes,
            }
        })
        .collect::<Vec<_>>();

    ControlCatalogSnapshot {
        revision: snapshot.revision,
        sessions,
        contexts,
        context_session_bindings,
    }
}
