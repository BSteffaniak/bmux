//! Workspace catalog snapshot participant. Attachment navigation is excluded.
use super::{WorkspaceRecord, WorkspaceState, load_catalog, save_catalog};
use bmux_plugin::{TypedServiceCaller, global_plugin_state_registry};
use bmux_plugin_sdk::{
    NativeLifecycleContext, PluginEventKind, StatefulPlugin, StatefulPluginError,
    StatefulPluginHandle, StatefulPluginResult, StatefulPluginSnapshot,
};
use bmux_snapshot_runtime::StatefulPluginRegistry;
use std::sync::{Arc, RwLock};

const ID: PluginEventKind = PluginEventKind::from_static("bmux.workspaces/catalog");
pub fn register(
    context: &NativeLifecycleContext,
    state: Arc<RwLock<WorkspaceState>>,
) -> Result<(), String> {
    let registry = global_plugin_state_registry();
    let participants = bmux_snapshot_runtime::get_or_init_stateful_registry(
        || registry.get::<StatefulPluginRegistry>(),
        |fresh| {
            registry.register::<StatefulPluginRegistry>(fresh);
        },
    );
    participants
        .write()
        .map_err(|_| "stateful registry lock poisoned")?
        .push(StatefulPluginHandle::new(Catalog {
            caller: TypedServiceCaller::from_lifecycle_context(context),
            state,
        }));
    Ok(())
}
struct Catalog {
    caller: TypedServiceCaller,
    state: Arc<RwLock<WorkspaceState>>,
}
fn decode(snapshot: &StatefulPluginSnapshot) -> StatefulPluginResult<Vec<WorkspaceRecord>> {
    if snapshot.version != 1 {
        return Err(StatefulPluginError::UnsupportedVersion {
            plugin: ID.as_str().into(),
            version: snapshot.version,
            expected: vec![1],
        });
    }
    let records: Vec<WorkspaceRecord> =
        serde_json::from_slice(&snapshot.bytes).map_err(|error| {
            StatefulPluginError::RestoreFailed {
                plugin: ID.as_str().into(),
                details: error.to_string(),
            }
        })?;
    let mut ids = std::collections::HashSet::new();
    let mut names = std::collections::HashSet::new();
    if records.iter().any(|record| {
        record.name.trim().is_empty() || !ids.insert(record.id) || !names.insert(&record.name)
    }) {
        return Err(StatefulPluginError::RestoreFailed {
            plugin: ID.as_str().into(),
            details: "invalid or duplicate workspace identity/name".into(),
        });
    }
    Ok(records)
}
impl StatefulPlugin for Catalog {
    fn id(&self) -> PluginEventKind {
        ID
    }
    fn snapshot(&self) -> StatefulPluginResult<StatefulPluginSnapshot> {
        let records =
            load_catalog(&self.caller).map_err(|details| StatefulPluginError::SnapshotFailed {
                plugin: ID.as_str().into(),
                details,
            })?;
        let bytes =
            serde_json::to_vec(&records).map_err(|error| StatefulPluginError::SnapshotFailed {
                plugin: ID.as_str().into(),
                details: error.to_string(),
            })?;
        Ok(StatefulPluginSnapshot::new(ID, 1, bytes))
    }
    fn validate_snapshot(&self, snapshot: &StatefulPluginSnapshot) -> StatefulPluginResult<()> {
        decode(snapshot).map(|_| ())
    }
    fn restore_snapshot(&self, snapshot: StatefulPluginSnapshot) -> StatefulPluginResult<()> {
        let records = decode(&snapshot)?;
        let mut state = self
            .state
            .write()
            .map_err(|_| StatefulPluginError::RestoreFailed {
                plugin: ID.as_str().into(),
                details: "workspace state lock poisoned".into(),
            })?;
        save_catalog(&self.caller, &records).map_err(|error| {
            StatefulPluginError::RestoreFailed {
                plugin: ID.as_str().into(),
                details: format!("{error:?}"),
            }
        })?;
        state.records = records;
        state.active_by_client.clear();
        state.previous_by_client.clear();
        state.selected_context_by_client_workspace.clear();
        drop(state);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_unsupported_versions_and_duplicate_identity() {
        assert!(decode(&StatefulPluginSnapshot::new(ID, 2, b"[]".to_vec())).is_err());
        let id = uuid::Uuid::nil();
        let bytes =
            format!("[{{\"id\":\"{id}\",\"name\":\"one\"}},{{\"id\":\"{id}\",\"name\":\"two\"}}]")
                .into_bytes();
        assert!(decode(&StatefulPluginSnapshot::new(ID, 1, bytes)).is_err());
    }
}
