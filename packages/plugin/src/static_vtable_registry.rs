//! Global registry of statically-bundled plugin vtables.
//!
//! The plugin loader in this crate (`NativePluginLoader`,
//! `load_registered_plugin`) only knows how to load plugins from
//! dynamic libraries. Statically-bundled plugins live inside the
//! final CLI binary and are not discoverable via filesystem scans.
//!
//! This registry bridges the gap: at startup the CLI registers every
//! statically-bundled plugin's [`StaticPluginVtable`] (keyed by
//! plugin id). When a plugin-to-plugin `call_service_raw` path tries
//! to dispatch to a bundled plugin, it consults this registry and
//! uses [`load_static_plugin`](crate::load_static_plugin) rather
//! than trying to `dlopen` a non-existent dynamic library.

use bmux_plugin_sdk::definition::PluginDefinition;
use bmux_plugin_sdk::{PluginError, Result, StaticPluginVtable};
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

static REGISTRY: OnceLock<RwLock<BTreeMap<String, StaticPluginVtable>>> = OnceLock::new();

fn registry() -> &'static RwLock<BTreeMap<String, StaticPluginVtable>> {
    REGISTRY.get_or_init(|| RwLock::new(BTreeMap::new()))
}

/// Register a statically-bundled plugin's vtable keyed by its manifest
/// plugin id. Idempotent: re-registering an id overwrites the prior
/// entry (used by tests that reload the CLI process in-place).
pub fn register_static_vtable(plugin_id: &str, vtable: StaticPluginVtable) {
    if let Ok(mut guard) = registry().write() {
        guard.insert(plugin_id.to_string(), vtable);
    }
}

/// Look up a registered static vtable by plugin id. Returns `None`
/// when no statically-bundled plugin with that id is known (the
/// caller should fall back to the dynamic-library load path).
#[must_use]
pub fn static_vtable(plugin_id: &str) -> Option<StaticPluginVtable> {
    registry().read().ok()?.get(plugin_id).copied()
}
