use std::collections::BTreeMap;

use anyhow::{Context, Result};
use bmux_control_catalog_plugin_api::control_catalog_state;
use bmux_ipc::{
    ContextSessionBindingSummary, ContextSummary, ControlCatalogSnapshot, SessionSummary,
};
use bmux_plugin_sdk::TypedDispatchClient;

pub async fn control_catalog_snapshot<C: TypedDispatchClient>(
    client: &mut C,
    since_revision: Option<u64>,
) -> Result<ControlCatalogSnapshot> {
    let snapshot = control_catalog_state::client::snapshot(client, since_revision)
        .await
        .context("control-catalog snapshot dispatch failed")?;
    Ok(map_snapshot(snapshot))
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn map_snapshot(snapshot: control_catalog_state::Snapshot) -> ControlCatalogSnapshot {
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
        .map(|binding| ContextSessionBindingSummary {
            context_id: binding.context_id,
            session_id: binding.session_id,
        })
        .collect::<Vec<_>>();

    let binding_by_context = snapshot
        .context_session_bindings
        .iter()
        .map(|binding| (binding.context_id, binding.session_id))
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
