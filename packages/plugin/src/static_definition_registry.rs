//! Process registry of inert bundled plugin definitions.
//!
//! Bundled implementations live inside the host binary and are not
//! discoverable via filesystem scans. The host registers each bundled
//! `plugin_definition()` here, keyed by plugin id; the loader creates a fresh
//! scoped instance from it. No implementation state is cached.

use bmux_plugin_sdk::definition::PluginDefinition;
use bmux_plugin_sdk::{PluginError, Result};
use std::collections::BTreeMap;
use std::sync::{OnceLock, RwLock};

static DEFINITIONS: OnceLock<RwLock<BTreeMap<String, PluginDefinition>>> = OnceLock::new();

/// Register only an inert declaration/factory. Each runtime creates its own
/// instance. Conflicts are explicit: an artifact cannot replace live code by
/// overwriting a process-global logical-name entry.
pub fn register_static_definition(definition: PluginDefinition) -> Result<()> {
    let mut entries = DEFINITIONS
        .get_or_init(|| RwLock::new(BTreeMap::new()))
        .write()
        .map_err(|_| PluginError::ServiceProtocol {
            details: "static definition registry poisoned".into(),
        })?;
    let id = definition.metadata.plugin_id.clone();
    if id.is_empty() || entries.contains_key(&id) {
        return Err(PluginError::ServiceProtocol {
            details: format!("empty or duplicate static definition '{id}'"),
        });
    }
    entries.insert(id, definition);
    drop(entries);
    Ok(())
}

/// This is declaration lookup only, not a cache of scoped implementation state.
pub fn static_definition(plugin_id: &str) -> Result<Option<PluginDefinition>> {
    DEFINITIONS
        .get_or_init(|| RwLock::new(BTreeMap::new()))
        .read()
        .map(|entries| entries.get(plugin_id).cloned())
        .map_err(|_| PluginError::ServiceProtocol {
            details: "static definition registry poisoned".into(),
        })
}
