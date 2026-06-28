//! Temporary Bcode manifest compatibility adapter.
//!
//! This module is migration-only. BMUX's canonical manifest remains
//! [`crate::PluginManifest`]; Bcode-only fields are adapted into generic
//! contributions/extensions here and must not be added to core manifest
//! structs.

use crate::{
    PluginManifest, PluginManifestCompatibility, PluginManifestKeybindings, PluginRuntime,
};
use bmux_plugin_sdk::{PluginCommand, PluginContribution, PluginError, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BcodeManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub concurrency: bmux_plugin_runtime::PluginConcurrencyConfig,
    #[serde(default)]
    pub services: Vec<bmux_plugin_sdk::PluginService>,
    #[serde(default)]
    pub event_subscriptions: Vec<bmux_plugin_sdk::PluginEventSubscription>,
    #[serde(default)]
    pub command_contributions: Vec<PluginCommand>,
    #[serde(default)]
    pub tui_surfaces: BTreeMap<String, toml::Value>,
    #[serde(default)]
    pub agent_defaults: BTreeMap<String, toml::Value>,
    #[serde(default)]
    pub config: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AdaptedBcodeManifest {
    pub manifest: PluginManifest,
    pub contributions: Vec<PluginContribution>,
    pub compatibility_payloads: BTreeMap<String, toml::Value>,
}

/// Parse a Bcode manifest and adapt it into canonical BMUX manifest data.
///
/// # Errors
///
/// Returns when the source is not valid supported Bcode compatibility TOML.
pub fn parse_bcode_manifest(source: &str) -> Result<AdaptedBcodeManifest> {
    let bcode: BcodeManifest = toml::from_str(source)?;
    let mut manifest = PluginManifest {
        id: bcode.id,
        name: bcode.name,
        version: bcode.version,
        description: None,
        homepage: None,
        provider_priority: 0,
        execution_class: crate::PluginExecutionClass::default(),
        concurrency: bcode.concurrency,
        owns_namespaces: BTreeSet::default(),
        owns_paths: BTreeSet::default(),
        runtime: PluginRuntime::Native,
        entry: None,
        entry_args: Vec::new(),
        process_persistent_worker: false,
        entry_symbol: bmux_plugin_sdk::DEFAULT_NATIVE_ENTRY_SYMBOL.to_string(),
        plugin_api: PluginManifestCompatibility::default(),
        native_abi: PluginManifestCompatibility::default(),
        required_capabilities: BTreeSet::default(),
        provided_capabilities: BTreeSet::default(),
        provided_features: BTreeSet::default(),
        services: bcode.services,
        commands: Vec::new(),
        event_subscriptions: bcode.event_subscriptions,
        event_publications: Vec::new(),
        extensions: Vec::new(),
        dependencies: Vec::new(),
        keybindings: PluginManifestKeybindings::default(),
        ready_signals: Vec::new(),
    };

    if !bcode.tui_surfaces.is_empty() {
        manifest.extensions.push(crate::PluginManifestExtension {
            extension_point: "bcode.tui_surface/v1".to_string(),
            payload: toml::to_string(&bcode.tui_surfaces)
                .map_err(|error| PluginError::ServiceProtocol {
                    details: format!(
                        "failed to encode bcode tui surface compatibility payload: {error}"
                    ),
                })?
                .into_bytes(),
        });
    }

    let compatibility_payloads = [
        ("bcode.agent_defaults".to_string(), bcode.agent_defaults),
        ("bcode.config".to_string(), bcode.config),
    ]
    .into_iter()
    .filter_map(|(name, value)| {
        (!value.is_empty()).then(|| (name, toml::Value::Table(value.into_iter().collect())))
    })
    .collect();

    let contributions = bcode
        .command_contributions
        .into_iter()
        .map(PluginContribution::command)
        .collect();

    Ok(AdaptedBcodeManifest {
        manifest,
        contributions,
        compatibility_payloads,
    })
}

#[cfg(test)]
mod tests {
    use super::parse_bcode_manifest;

    #[test]
    fn current_bcode_manifest_parses_through_compatibility_adapter() {
        let adapted = parse_bcode_manifest(
            r#"
id = "bcode.test"
name = "Bcode Test"
version = "0.1.0"
concurrency = { mode = "concurrent" }

[[command_contributions]]
name = "ask"
summary = "Ask the model"
execution = "provider_exec"

[tui_surfaces.main]
title = "Main"

[agent_defaults]
model = "test-model"

[config]
foo = "bar"
"#,
        )
        .expect("bcode manifest should adapt");
        assert_eq!(adapted.manifest.id, "bcode.test");
        assert_eq!(adapted.contributions.len(), 1);
        assert_eq!(adapted.contributions[0].id(), "command:ask");
        assert_eq!(
            adapted.manifest.extensions[0].extension_point,
            "bcode.tui_surface/v1"
        );
        assert!(
            adapted
                .compatibility_payloads
                .contains_key("bcode.agent_defaults")
        );
        assert!(adapted.compatibility_payloads.contains_key("bcode.config"));
    }

    #[test]
    fn bcode_only_fields_do_not_leak_into_canonical_manifest() {
        let adapted = parse_bcode_manifest(
            r#"
id = "bcode.test"
name = "Bcode Test"
version = "0.1.0"

[tui_surfaces.main]
title = "Main"
"#,
        )
        .expect("bcode manifest should adapt");
        let serialized = toml::to_string(&adapted.manifest).expect("manifest should serialize");
        assert!(!serialized.contains("tui_surfaces"));
        assert!(!serialized.contains("agent_defaults"));
    }
}
