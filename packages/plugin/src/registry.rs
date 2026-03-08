use crate::{
    HostMetadata, PluginCapability, PluginDeclaration, PluginError, PluginManifest, Result,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginCompatibilityReport {
    pub api_compatible: bool,
    pub abi_compatible: bool,
}

impl PluginCompatibilityReport {
    #[must_use]
    pub const fn is_loadable(&self) -> bool {
        self.api_compatible && self.abi_compatible
    }
}

#[derive(Debug, Clone)]
pub struct RegisteredPlugin {
    pub manifest_path: PathBuf,
    pub manifest: PluginManifest,
    pub declaration: PluginDeclaration,
}

#[derive(Debug, Default)]
pub struct PluginRegistry {
    plugins: BTreeMap<String, RegisteredPlugin>,
}

impl PluginRegistry {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            plugins: BTreeMap::new(),
        }
    }

    /// # Errors
    ///
    /// Returns an error when the manifest cannot be registered.
    pub fn register_manifest_path(&mut self, path: &Path) -> Result<()> {
        let manifest = PluginManifest::from_path(path)?;
        self.register_manifest(path, manifest)
    }

    /// # Errors
    ///
    /// Returns an error when the manifest is invalid or uses a duplicate id.
    pub fn register_manifest(&mut self, path: &Path, manifest: PluginManifest) -> Result<()> {
        let declaration = manifest.to_declaration()?;
        let plugin_id = declaration.id.as_str().to_string();

        if self.plugins.contains_key(&plugin_id) {
            return Err(PluginError::DuplicatePluginId { id: plugin_id });
        }

        self.plugins.insert(
            plugin_id,
            RegisteredPlugin {
                manifest_path: path.to_path_buf(),
                manifest,
                declaration,
            },
        );

        Ok(())
    }

    #[must_use]
    pub fn iter(&self) -> impl Iterator<Item = &RegisteredPlugin> {
        self.plugins.values()
    }

    #[must_use]
    pub fn compatibility_report(
        registered_plugin: &RegisteredPlugin,
        host: &HostMetadata,
    ) -> PluginCompatibilityReport {
        PluginCompatibilityReport {
            api_compatible: registered_plugin
                .declaration
                .plugin_api
                .contains(host.plugin_api_version),
            abi_compatible: registered_plugin
                .declaration
                .native_abi
                .contains(host.plugin_abi_version),
        }
    }

    /// # Errors
    ///
    /// Returns an error when any plugin is incompatible with the host or when a
    /// native entry file is missing.
    pub fn validate_against_host(
        &self,
        host: &HostMetadata,
        supported_capabilities: &[PluginCapability],
    ) -> Result<()> {
        for plugin in self.plugins.values() {
            let report = Self::compatibility_report(plugin, host);
            if !report.api_compatible {
                return Err(PluginError::IncompatibleApiVersion {
                    plugin_id: plugin.declaration.id.as_str().to_string(),
                    required: plugin.declaration.plugin_api.to_string(),
                    host: host.plugin_api_version,
                });
            }
            if !report.abi_compatible {
                return Err(PluginError::IncompatibleAbiVersion {
                    plugin_id: plugin.declaration.id.as_str().to_string(),
                    required: plugin.declaration.native_abi.to_string(),
                    host: host.plugin_abi_version,
                });
            }

            let supported = supported_capabilities.iter().copied().collect::<Vec<_>>();
            for capability in &plugin.declaration.capabilities {
                if !supported.contains(capability) {
                    return Err(PluginError::UnsupportedCapability {
                        plugin_id: plugin.declaration.id.as_str().to_string(),
                        capability: *capability,
                    });
                }
            }

            let entry_path = plugin.manifest.resolve_entry_path(
                plugin
                    .manifest_path
                    .parent()
                    .unwrap_or_else(|| Path::new(".")),
            );
            if !entry_path.exists() {
                return Err(PluginError::MissingEntryFile {
                    plugin_id: plugin.declaration.id.as_str().to_string(),
                    path: entry_path,
                });
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::PluginRegistry;
    use crate::{ApiVersion, HostMetadata, PluginCapability, PluginManifest};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be monotonic for test")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("bmux-plugin-test-{nanos}"));
        fs::create_dir_all(&dir).expect("temp dir should be created");
        dir
    }

    #[test]
    fn registry_rejects_duplicate_ids() {
        let manifest = PluginManifest::from_toml_str(
            r#"
id = "git.status"
name = "Git Status"
version = "0.1.0"
entry = "libgit_status.dylib"

[plugin_api]
minimum = "1.0"

[native_abi]
minimum = "1.0"
"#,
        )
        .expect("manifest should parse");

        let mut registry = PluginRegistry::new();
        let base = std::env::temp_dir().join("bmux-plugin-registry.toml");
        registry
            .register_manifest(&base, manifest.clone())
            .expect("first registration should work");
        assert!(registry.register_manifest(&base, manifest).is_err());
    }

    #[test]
    fn registry_validates_host_compatibility_and_entry_file() {
        let dir = temp_dir();
        let entry = dir.join("libgit_status.dylib");
        fs::write(&entry, []).expect("entry file should be written");

        let manifest = PluginManifest::from_toml_str(
            r#"
id = "git.status"
name = "Git Status"
version = "0.1.0"
entry = "libgit_status.dylib"
capabilities = ["commands"]

[plugin_api]
minimum = "1.0"

[native_abi]
minimum = "1.0"
"#,
        )
        .expect("manifest should parse");

        let manifest_path = dir.join("plugin.toml");
        let mut registry = PluginRegistry::new();
        registry
            .register_manifest(&manifest_path, manifest)
            .expect("manifest should register");

        let host = HostMetadata {
            product_name: "bmux".to_string(),
            product_version: "0.1.0".to_string(),
            plugin_api_version: ApiVersion::new(1, 0),
            plugin_abi_version: ApiVersion::new(1, 0),
        };

        registry
            .validate_against_host(&host, &[PluginCapability::Commands])
            .expect("plugin should validate");
    }
}
