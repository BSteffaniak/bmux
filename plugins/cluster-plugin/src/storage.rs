#![allow(clippy::wildcard_imports)] // Private domain modules share crate-private models.

use super::*;

pub fn pane_binding_storage_key(pane_id: &str) -> bmux_plugin_sdk::StorageKey {
    bmux_plugin_sdk::StorageKey::new(format!("{CLUSTER_PANE_BINDING_PREFIX}{pane_id}"))
        .expect("pane binding storage key should use storage-safe pane identifiers")
}

pub fn set_cluster_pane_binding(
    caller: &impl ClusterRuntimeOps,
    pane_id: &str,
    binding: Option<&ClusterPaneBinding>,
) -> Result<(), String> {
    let value = if let Some(binding) = binding {
        serde_json::to_vec(binding)
            .map_err(|error| format!("failed encoding pane metadata: {error}"))?
    } else {
        Vec::new()
    };
    caller
        .storage_set(&StorageSetRequest::new(
            pane_binding_storage_key(pane_id),
            value,
        ))
        .map_err(|error| format!("failed writing pane metadata: {error}"))
}

pub fn get_cluster_pane_binding(
    caller: &impl ClusterRuntimeOps,
    pane_id: &str,
) -> Result<Option<ClusterPaneBinding>, String> {
    let response = caller
        .storage_get(&StorageGetRequest::new(pane_binding_storage_key(pane_id)))
        .map_err(|error| format!("failed reading pane metadata: {error}"))?;
    let Some(value) = response.value else {
        return Ok(None);
    };
    if value.is_empty() {
        return Ok(None);
    }
    serde_json::from_slice::<ClusterPaneBinding>(&value)
        .map(Some)
        .map_err(|error| format!("failed decoding pane metadata: {error}"))
}

pub fn get_cluster_connection_events(
    caller: &impl ClusterRuntimeOps,
) -> Result<Vec<ClusterConnectionEvent>, String> {
    let response = caller
        .storage_get(&StorageGetRequest::new(bmux_plugin_sdk::storage_key!(
            "cluster.connection.events"
        )))
        .map_err(|error| format!("failed reading connection events: {error}"))?;
    let Some(value) = response.value else {
        return Ok(Vec::new());
    };
    if value.is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_slice::<Vec<ClusterConnectionEvent>>(&value)
        .map_err(|error| format!("failed decoding connection events: {error}"))
}

pub fn set_cluster_connection_events(
    caller: &impl ClusterRuntimeOps,
    events: &[ClusterConnectionEvent],
) -> Result<(), String> {
    let value = serde_json::to_vec(events)
        .map_err(|error| format!("failed encoding connection events: {error}"))?;
    caller
        .storage_set(&StorageSetRequest::new(
            bmux_plugin_sdk::storage_key!("cluster.connection.events"),
            value,
        ))
        .map_err(|error| format!("failed writing connection events: {error}"))
}

pub fn append_cluster_connection_event(
    caller: &impl ClusterRuntimeOps,
    event: ClusterConnectionEvent,
) -> Result<(), String> {
    let mut events = get_cluster_connection_events(caller)?;
    events.push(event);
    if events.len() > CLUSTER_CONNECTION_EVENTS_MAX {
        let to_drop = events.len() - CLUSTER_CONNECTION_EVENTS_MAX;
        events.drain(0..to_drop);
    }
    set_cluster_connection_events(caller, &events)
}

pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}
