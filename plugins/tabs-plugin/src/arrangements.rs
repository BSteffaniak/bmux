//! Versioned snapshot contract for the tabs plugin's durable arrangement data.
//! Capture the owner storage, not a live-context projection: temporarily absent
//! resources must survive export and partial restore.
use super::{TAB_ORDER_MUTATION, parse_stored_tab_order_value};
use bmux_plugin::{HostRuntimeApi, TypedServiceCaller, global_plugin_state_registry};
use bmux_plugin_sdk::{
    NativeLifecycleContext, PluginEventKind, StatefulPlugin, StatefulPluginError,
    StatefulPluginHandle, StatefulPluginResult, StatefulPluginSnapshot, StorageGetRequest,
    StorageSetRequest,
};
use bmux_snapshot_runtime::StatefulPluginRegistry;
use std::collections::BTreeMap;

const ID: PluginEventKind = PluginEventKind::from_static("bmux.tabs/arrangements");
fn key() -> bmux_plugin_sdk::StorageKey {
    bmux_plugin_sdk::storage_key!("tabs.arrangements.v1")
}

pub fn read(
    caller: &impl HostRuntimeApi,
) -> Result<Option<BTreeMap<uuid::Uuid, Vec<uuid::Uuid>>>, String> {
    caller
        .storage_get(&StorageGetRequest::new(key()))
        .map_err(|error| error.to_string())?
        .value
        .map(|bytes| decode(&bytes))
        .transpose()
}

fn decode(bytes: &[u8]) -> Result<BTreeMap<uuid::Uuid, Vec<uuid::Uuid>>, String> {
    let orders: BTreeMap<uuid::Uuid, Vec<uuid::Uuid>> = serde_json::from_slice(bytes)
        .map_err(|error| format!("invalid tab arrangements: {error}"))?;
    for order in orders.values() {
        let mut ids = std::collections::HashSet::new();
        if order.iter().any(|id| !ids.insert(*id)) {
            return Err("duplicate tab reference in arrangement".into());
        }
    }
    Ok(orders)
}

pub fn write(
    caller: &impl HostRuntimeApi,
    orders: &BTreeMap<uuid::Uuid, Vec<uuid::Uuid>>,
) -> Result<(), String> {
    caller
        .storage_set(&StorageSetRequest::new(
            key(),
            serde_json::to_vec(orders).map_err(|error| error.to_string())?,
        ))
        .map_err(|error| error.to_string())?;
    if let Some(flag) =
        global_plugin_state_registry().get::<bmux_snapshot_runtime::SnapshotDirtyFlagHandle>()
    {
        flag.read()
            .map_err(|_| "snapshot dirty flag lock poisoned")?
            .0
            .mark_dirty();
    }
    Ok(())
}

pub fn activate(context: &NativeLifecycleContext) -> Result<(), String> {
    let caller = TypedServiceCaller::from_lifecycle_context(context);
    {
        let _guard = TAB_ORDER_MUTATION
            .lock()
            .map_err(|_| "tab order lock poisoned")?;
        if read(&caller)?.is_none() {
            let root =
                std::path::Path::new(&context.connection.data_dir).join("plugin-storage/bmux.tabs");
            let mut orders = BTreeMap::new();
            let mut legacy = None;
            if root.try_exists().map_err(|error| error.to_string())? {
                for entry in std::fs::read_dir(root).map_err(|error| error.to_string())? {
                    let entry = entry.map_err(|error| error.to_string())?;
                    let name = entry.file_name();
                    let Some(name) = name.to_str() else { continue };
                    if name == "tabs.order.bin" {
                        legacy = Some(parse_stored_tab_order_value(
                            std::fs::read(entry.path()).map_err(|error| error.to_string())?,
                        )?);
                    } else if let Some(id) = name
                        .strip_prefix("tabs.order.")
                        .and_then(|name| name.strip_suffix(".bin"))
                    {
                        let id = uuid::Uuid::parse_str(id).map_err(|error| error.to_string())?;
                        orders.insert(
                            id,
                            parse_stored_tab_order_value(
                                std::fs::read(entry.path()).map_err(|error| error.to_string())?,
                            )?,
                        );
                    }
                }
            }
            if let Some(legacy) = legacy {
                orders.entry(uuid::Uuid::nil()).or_insert(legacy);
            }
            // One commit switches authority. Old files remain untouched for
            // interruption recovery and are never consulted after this commit.
            write(&caller, &orders)?;
        }
    }
    let registry = global_plugin_state_registry();
    let stateful = bmux_snapshot_runtime::get_or_init_stateful_registry(
        || registry.get::<StatefulPluginRegistry>(),
        |fresh| {
            registry.register::<StatefulPluginRegistry>(fresh);
        },
    );
    stateful
        .write()
        .map_err(|_| "stateful registry lock poisoned")?
        .push(StatefulPluginHandle::new(Arrangements { caller }));
    Ok(())
}

struct Arrangements {
    caller: TypedServiceCaller,
}

impl StatefulPlugin for Arrangements {
    fn id(&self) -> PluginEventKind {
        ID
    }
    fn restore_dependencies(&self) -> Vec<PluginEventKind> {
        vec![PluginEventKind::from_static("bmux.contexts/context-state")]
    }

    fn snapshot(&self) -> StatefulPluginResult<StatefulPluginSnapshot> {
        let _guard =
            TAB_ORDER_MUTATION
                .lock()
                .map_err(|_| StatefulPluginError::SnapshotFailed {
                    plugin: ID.as_str().into(),
                    details: "tab order lock poisoned".into(),
                })?;
        let orders = read(&self.caller)
            .and_then(|value| value.ok_or_else(|| "arrangement authority missing".into()))
            .map_err(|details| StatefulPluginError::SnapshotFailed {
                plugin: ID.as_str().into(),
                details,
            })?;
        let bytes =
            serde_json::to_vec(&orders).map_err(|error| StatefulPluginError::SnapshotFailed {
                plugin: ID.as_str().into(),
                details: error.to_string(),
            })?;
        Ok(StatefulPluginSnapshot::new(ID, 1, bytes))
    }
    fn validate_snapshot(&self, snapshot: &StatefulPluginSnapshot) -> StatefulPluginResult<()> {
        if snapshot.version != 1 {
            return Err(StatefulPluginError::UnsupportedVersion {
                plugin: ID.as_str().into(),
                version: snapshot.version,
                expected: vec![1],
            });
        }
        decode(&snapshot.bytes)
            .map(|_| ())
            .map_err(|details| StatefulPluginError::RestoreFailed {
                plugin: ID.as_str().into(),
                details,
            })
    }
    fn restore_snapshot(&self, snapshot: StatefulPluginSnapshot) -> StatefulPluginResult<()> {
        self.validate_snapshot(&snapshot)?;
        let restore = || -> Result<(), String> {
            let _guard = TAB_ORDER_MUTATION
                .lock()
                .map_err(|_| "tab order lock poisoned")?;
            write(&self.caller, &decode(&snapshot.bytes)?)?;
            super::publish_tab_list_snapshot(
                &self.caller,
                &super::TabRuntimeStateHandle::default(),
            );
            Ok(())
        };
        restore().map_err(|details| StatefulPluginError::RestoreFailed {
            plugin: ID.as_str().into(),
            details,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_corrupt_and_duplicate_references() {
        assert!(decode(b"not-json").is_err());
        let id = uuid::Uuid::nil();
        let bytes = format!("{{\"{id}\":[\"{id}\",\"{id}\"]}}");
        assert!(decode(bytes.as_bytes()).is_err());
    }
    #[test]
    fn round_trip_preserves_absent_resource_references() {
        let orders = BTreeMap::from([(
            uuid::Uuid::nil(),
            vec![uuid::Uuid::from_u128(99), uuid::Uuid::from_u128(1)],
        )]);
        assert_eq!(
            decode(&serde_json::to_vec(&orders).unwrap()).unwrap(),
            orders
        );
    }
}
