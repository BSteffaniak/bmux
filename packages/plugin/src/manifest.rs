use crate::{PluginDeclaration, PluginDependency, PluginEntrypoint, PluginId};
use bmux_plugin_sdk::{
    ApiVersion, HostScope, PluginCommand, PluginError, PluginEventSubscription, PluginFeature,
    PluginService, Result, VersionRange,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum PluginRuntime {
    #[default]
    Native,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifestCompatibility {
    pub minimum: String,
    #[serde(default)]
    pub maximum: Option<String>,
}

impl Default for PluginManifestCompatibility {
    fn default() -> Self {
        Self {
            minimum: "1.0".to_string(),
            maximum: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifestDependency {
    pub plugin_id: String,
    #[serde(default = "default_dependency_version_req")]
    pub version_req: String,
    #[serde(default = "default_true")]
    pub required: bool,
}

impl PluginManifestCompatibility {
    /// # Errors
    ///
    /// Returns an error when any declared version cannot be parsed.
    pub fn to_version_range(&self) -> std::result::Result<VersionRange, String> {
        let minimum = self.minimum.parse::<ApiVersion>()?;
        let maximum = self
            .maximum
            .as_deref()
            .map(str::parse::<ApiVersion>)
            .transpose()?;
        Ok(VersionRange { minimum, maximum })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub provider_priority: i32,
    #[serde(default)]
    pub runtime: PluginRuntime,
    #[serde(default)]
    pub entry: Option<PathBuf>,
    #[serde(default = "default_entry_symbol")]
    pub entry_symbol: String,
    #[serde(default)]
    pub plugin_api: PluginManifestCompatibility,
    #[serde(default)]
    pub native_abi: PluginManifestCompatibility,
    #[serde(default)]
    #[serde(alias = "required_host_scopes")]
    pub required_capabilities: BTreeSet<HostScope>,
    #[serde(default)]
    pub provided_capabilities: BTreeSet<HostScope>,
    #[serde(default)]
    pub provided_features: BTreeSet<PluginFeature>,
    #[serde(default)]
    pub services: Vec<PluginService>,
    #[serde(default)]
    pub commands: Vec<PluginCommand>,
    #[serde(default)]
    pub event_subscriptions: Vec<PluginEventSubscription>,
    #[serde(default)]
    pub dependencies: Vec<PluginManifestDependency>,
    #[serde(default)]
    pub keybindings: PluginManifestKeybindings,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PluginManifestKeybindings {
    #[serde(default)]
    pub runtime: BTreeMap<String, String>,
    #[serde(default)]
    pub global: BTreeMap<String, String>,
    #[serde(default)]
    pub scroll: BTreeMap<String, String>,
}

impl PluginManifest {
    /// # Errors
    ///
    /// Returns an error when the manifest cannot be parsed.
    pub fn from_toml_str(value: &str) -> Result<Self> {
        Ok(toml::from_str(value)?)
    }

    /// # Errors
    ///
    /// Returns an error when the manifest cannot be read or parsed.
    pub fn from_path(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)?;
        Self::from_toml_str(&contents)
    }

    /// # Errors
    ///
    /// Returns an error when the manifest cannot be converted into a checked
    /// declaration.
    pub fn to_declaration(&self) -> Result<PluginDeclaration> {
        let declaration = PluginDeclaration {
            id: PluginId::new(self.id.clone())?,
            display_name: self.name.clone(),
            plugin_version: self.version.clone(),
            plugin_api: self.plugin_api.to_version_range().map_err(|details| {
                PluginError::InvalidVersionRange {
                    plugin_id: self.id.clone(),
                    field: "plugin_api",
                    details,
                }
            })?,
            native_abi: self.native_abi.to_version_range().map_err(|details| {
                PluginError::InvalidVersionRange {
                    plugin_id: self.id.clone(),
                    field: "native_abi",
                    details,
                }
            })?,
            entrypoint: PluginEntrypoint::Native {
                symbol: self.entry_symbol.clone(),
            },
            description: self.description.clone(),
            homepage: self.homepage.clone(),
            provider_priority: self.provider_priority,
            required_capabilities: self.required_capabilities.clone(),
            provided_capabilities: self.provided_capabilities.clone(),
            provided_features: self.provided_features.clone(),
            services: self.services.clone(),
            commands: self.commands.clone(),
            event_subscriptions: self.event_subscriptions.clone(),
            dependencies: self
                .dependencies
                .iter()
                .map(|dependency| {
                    Ok(PluginDependency {
                        plugin_id: PluginId::new(dependency.plugin_id.clone())?,
                        version_req: dependency.version_req.clone(),
                        required: dependency.required,
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            lifecycle: crate::PluginLifecycle::default(),
        };
        declaration.validate()?;
        Ok(declaration)
    }

    #[must_use]
    pub fn resolve_entry_path(&self, base_dir: &Path) -> Option<PathBuf> {
        let entry = self.entry.as_ref()?;
        if entry.is_absolute() {
            Some(entry.clone())
        } else {
            Some(base_dir.join(entry))
        }
    }
}

fn default_entry_symbol() -> String {
    bmux_plugin_sdk::DEFAULT_NATIVE_ENTRY_SYMBOL.to_string()
}

const fn default_true() -> bool {
    true
}

fn default_dependency_version_req() -> String {
    "*".to_string()
}

#[cfg(test)]
mod tests {
    use super::PluginManifest;
    use bmux_plugin_sdk::HostScope;

    #[test]
    fn parses_native_plugin_manifest() {
        let manifest = PluginManifest::from_toml_str(
            r#"
id = "git.status"
name = "Git Status"
version = "0.1.0"
runtime = "native"
entry = "libgit_status.dylib"
required_capabilities = ["bmux.commands", "bmux.events.subscribe"]

[[commands]]
name = "hello"
summary = "hello"
execution = "provider_exec"

[[event_subscriptions]]
kinds = ["system"]
names = ["server_started"]
"#,
        )
        .expect("manifest should parse");

        assert_eq!(manifest.id, "git.status");
        assert!(
            manifest
                .required_capabilities
                .contains(&HostScope::new("bmux.commands").expect("scope should parse"))
        );
        assert_eq!(manifest.commands.len(), 1);
        assert_eq!(manifest.event_subscriptions.len(), 1);
        assert!(manifest.keybindings.runtime.is_empty());
        assert!(manifest.keybindings.global.is_empty());
        assert!(manifest.keybindings.scroll.is_empty());
    }

    #[test]
    fn parses_manifest_keybinding_proposals() {
        let manifest = PluginManifest::from_toml_str(
            r#"
id = "bmux.windows"
name = "Windows"
version = "0.1.0"
runtime = "native"
entry = "libwindows.dylib"

[keybindings.runtime]
c = "plugin:bmux.windows:new-window"
"alt+w" = "plugin:bmux.windows:switch-window"
"#,
        )
        .expect("manifest should parse");

        assert_eq!(
            manifest.keybindings.runtime.get("c").map(String::as_str),
            Some("plugin:bmux.windows:new-window")
        );
    }

    #[test]
    fn plugin_api_and_native_abi_default_to_1_0() {
        let manifest = PluginManifest::from_toml_str(
            r#"
id = "test.minimal"
name = "Minimal"
version = "0.1.0"
"#,
        )
        .expect("manifest should parse without plugin_api/native_abi/entry");

        assert_eq!(manifest.plugin_api.minimum, "1.0");
        assert!(manifest.plugin_api.maximum.is_none());
        assert_eq!(manifest.native_abi.minimum, "1.0");
        assert!(manifest.native_abi.maximum.is_none());
        assert!(manifest.entry.is_none());

        // Verify conversion to declaration also works
        let declaration = manifest.to_declaration().expect("declaration should build");
        assert_eq!(declaration.id.as_str(), "test.minimal");
    }

    #[test]
    fn entry_is_optional_and_resolves_when_present() {
        let manifest = PluginManifest::from_toml_str(
            r#"
id = "test.with_entry"
name = "With Entry"
version = "0.1.0"
entry = "libfoo.dylib"
"#,
        )
        .expect("manifest should parse");

        assert_eq!(
            manifest.entry.as_deref(),
            Some(std::path::Path::new("libfoo.dylib"))
        );
        assert_eq!(
            manifest.resolve_entry_path(std::path::Path::new("/base")),
            Some(std::path::PathBuf::from("/base/libfoo.dylib"))
        );

        let no_entry = PluginManifest::from_toml_str(
            r#"
id = "test.no_entry"
name = "No Entry"
version = "0.1.0"
"#,
        )
        .expect("manifest should parse without entry");

        assert!(no_entry.entry.is_none());
        assert!(
            no_entry
                .resolve_entry_path(std::path::Path::new("/base"))
                .is_none()
        );
    }

    #[test]
    fn explicit_plugin_api_and_native_abi_override_defaults() {
        let manifest = PluginManifest::from_toml_str(
            r#"
id = "test.custom_compat"
name = "Custom Compat"
version = "0.1.0"
entry = "unused.dylib"

[plugin_api]
minimum = "2.0"
maximum = "3.0"

[native_abi]
minimum = "1.5"
"#,
        )
        .expect("manifest with explicit compat should parse");

        assert_eq!(manifest.plugin_api.minimum, "2.0");
        assert_eq!(manifest.plugin_api.maximum.as_deref(), Some("3.0"));
        assert_eq!(manifest.native_abi.minimum, "1.5");
        assert!(manifest.native_abi.maximum.is_none());

        let declaration = manifest.to_declaration().expect("declaration should build");
        assert_eq!(declaration.id.as_str(), "test.custom_compat");
    }
}
