use crate::{
    ApiVersion, PluginCapability, PluginCommand, PluginDeclaration, PluginEntrypoint, PluginError,
    PluginEventSubscription, PluginId, Result, VersionRange,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginRuntime {
    Native,
}

impl Default for PluginRuntime {
    fn default() -> Self {
        Self::Native
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifestCompatibility {
    pub minimum: String,
    #[serde(default)]
    pub maximum: Option<String>,
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
    pub runtime: PluginRuntime,
    pub entry: PathBuf,
    #[serde(default = "default_entry_symbol")]
    pub entry_symbol: String,
    pub plugin_api: PluginManifestCompatibility,
    pub native_abi: PluginManifestCompatibility,
    #[serde(default)]
    pub capabilities: BTreeSet<PluginCapability>,
    #[serde(default)]
    pub commands: Vec<PluginCommand>,
    #[serde(default)]
    pub event_subscriptions: Vec<PluginEventSubscription>,
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
            capabilities: self.capabilities.clone(),
            commands: self.commands.clone(),
            event_subscriptions: self.event_subscriptions.clone(),
            lifecycle: crate::PluginLifecycle::default(),
        };
        declaration.validate()?;
        Ok(declaration)
    }

    #[must_use]
    pub fn resolve_entry_path(&self, base_dir: &Path) -> PathBuf {
        if self.entry.is_absolute() {
            self.entry.clone()
        } else {
            base_dir.join(&self.entry)
        }
    }
}

fn default_entry_symbol() -> String {
    crate::DEFAULT_NATIVE_ENTRY_SYMBOL.to_string()
}

#[cfg(test)]
mod tests {
    use super::PluginManifest;
    use crate::PluginCapability;

    #[test]
    fn parses_native_plugin_manifest() {
        let manifest = PluginManifest::from_toml_str(
            r#"
id = "git.status"
name = "Git Status"
version = "0.1.0"
runtime = "native"
entry = "libgit_status.dylib"
capabilities = ["commands", "event_subscription"]

[[commands]]
name = "hello"
summary = "hello"
execution = "host_callback"

[[event_subscriptions]]
kinds = ["system"]
names = ["server_started"]

[plugin_api]
minimum = "1.0"

[native_abi]
minimum = "1.0"
"#,
        )
        .expect("manifest should parse");

        assert_eq!(manifest.id, "git.status");
        assert!(manifest.capabilities.contains(&PluginCapability::Commands));
        assert_eq!(manifest.commands.len(), 1);
        assert_eq!(manifest.event_subscriptions.len(), 1);
    }
}
