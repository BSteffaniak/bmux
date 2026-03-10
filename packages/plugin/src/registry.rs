use crate::{
    HostMetadata, HostScope, PluginDeclaration, PluginError, PluginManifest, RegisteredService,
    Result,
};
use semver::Version;
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
    pub search_root: PathBuf,
    pub manifest_path: PathBuf,
    pub manifest: PluginManifest,
    pub declaration: PluginDeclaration,
}

#[derive(Debug, Default)]
pub struct PluginRegistry {
    plugins: BTreeMap<String, RegisteredPlugin>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityProvider {
    pub capability: HostScope,
    pub provider_plugin_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceProvider {
    pub service: RegisteredService,
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
        self.register_manifest_from_root(
            path.parent().unwrap_or_else(|| Path::new(".")),
            path,
            manifest,
        )
    }

    /// # Errors
    ///
    /// Returns an error when the manifest is invalid or uses a duplicate id.
    pub fn register_manifest(&mut self, path: &Path, manifest: PluginManifest) -> Result<()> {
        self.register_manifest_from_root(
            path.parent().unwrap_or_else(|| Path::new(".")),
            path,
            manifest,
        )
    }

    /// # Errors
    ///
    /// Returns an error when the manifest is invalid or uses a duplicate id.
    pub fn register_manifest_from_root(
        &mut self,
        search_root: &Path,
        path: &Path,
        manifest: PluginManifest,
    ) -> Result<()> {
        let declaration = manifest.to_declaration()?;
        let plugin_id = declaration.id.as_str().to_string();

        if self.plugins.contains_key(&plugin_id) {
            return Err(PluginError::DuplicatePluginId { id: plugin_id });
        }

        self.plugins.insert(
            plugin_id,
            RegisteredPlugin {
                search_root: search_root.to_path_buf(),
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
    pub fn get(&self, plugin_id: &str) -> Option<&RegisteredPlugin> {
        self.plugins.get(plugin_id)
    }

    /// # Errors
    ///
    /// Returns an error when the requested plugins or any required dependency is
    /// missing, incompatible, or introduces a dependency cycle.
    pub fn activation_order_for<'a>(
        &'a self,
        plugin_ids: &[String],
    ) -> Result<Vec<&'a RegisteredPlugin>> {
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum VisitState {
            Visiting,
            Visited,
        }

        fn visit<'a>(
            registry: &'a PluginRegistry,
            requested: &[String],
            plugin_id: &str,
            states: &mut BTreeMap<String, VisitState>,
            stack: &mut Vec<String>,
            ordered: &mut Vec<&'a RegisteredPlugin>,
        ) -> Result<()> {
            match states.get(plugin_id) {
                Some(VisitState::Visited) => return Ok(()),
                Some(VisitState::Visiting) => {
                    let start = stack
                        .iter()
                        .position(|entry| entry == plugin_id)
                        .unwrap_or_default();
                    let mut cycle = stack[start..].to_vec();
                    cycle.push(plugin_id.to_string());
                    return Err(PluginError::PluginDependencyCycle { cycle });
                }
                None => {}
            }

            let plugin =
                registry
                    .get(plugin_id)
                    .ok_or_else(|| PluginError::MissingRequiredDependency {
                        plugin_id: plugin_id.to_string(),
                        dependency_id: plugin_id.to_string(),
                    })?;
            states.insert(plugin_id.to_string(), VisitState::Visiting);
            stack.push(plugin_id.to_string());

            for dependency in &plugin.declaration.dependencies {
                let dependency_id = dependency.plugin_id.as_str();
                let Some(registered_dependency) = registry.get(dependency_id) else {
                    if dependency.required {
                        return Err(PluginError::MissingRequiredDependency {
                            plugin_id: plugin_id.to_string(),
                            dependency_id: dependency_id.to_string(),
                        });
                    }
                    continue;
                };

                let dependency_version = Version::parse(
                    &registered_dependency.declaration.plugin_version,
                )
                .map_err(|error| PluginError::InvalidDependencyVersion {
                    plugin_id: plugin_id.to_string(),
                    dependency_id: dependency_id.to_string(),
                    version_req: dependency.version_req.clone(),
                    details: error.to_string(),
                })?;
                let version_req =
                    semver::VersionReq::parse(&dependency.version_req).map_err(|error| {
                        PluginError::InvalidDependencyVersion {
                            plugin_id: plugin_id.to_string(),
                            dependency_id: dependency_id.to_string(),
                            version_req: dependency.version_req.clone(),
                            details: error.to_string(),
                        }
                    })?;
                if !version_req.matches(&dependency_version) {
                    return Err(PluginError::IncompatibleDependencyVersion {
                        plugin_id: plugin_id.to_string(),
                        dependency_id: dependency_id.to_string(),
                        version_req: dependency.version_req.clone(),
                        found_version: registered_dependency.declaration.plugin_version.clone(),
                    });
                }

                if requested.iter().any(|entry| entry == dependency_id) {
                    visit(registry, requested, dependency_id, states, stack, ordered)?;
                } else if dependency.required {
                    return Err(PluginError::MissingRequiredDependency {
                        plugin_id: plugin_id.to_string(),
                        dependency_id: dependency_id.to_string(),
                    });
                }
            }

            stack.pop();
            states.insert(plugin_id.to_string(), VisitState::Visited);
            ordered.push(plugin);
            Ok(())
        }

        let mut ordered = Vec::new();
        let mut states = BTreeMap::new();
        let mut stack = Vec::new();
        for plugin_id in plugin_ids {
            visit(
                self,
                plugin_ids,
                plugin_id,
                &mut states,
                &mut stack,
                &mut ordered,
            )?;
        }
        Ok(ordered)
    }

    #[must_use]
    pub fn plugin_ids(&self) -> Vec<&str> {
        self.plugins.keys().map(String::as_str).collect()
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
    /// Returns an error when the plugin is incompatible with the host or when
    /// its native entry file is missing.
    pub fn capability_providers_for(
        &self,
        plugin_ids: &[String],
        core_capabilities: &[HostScope],
    ) -> Result<BTreeMap<HostScope, CapabilityProvider>> {
        let mut providers = BTreeMap::new();
        for capability in core_capabilities {
            providers.insert(
                capability.clone(),
                CapabilityProvider {
                    capability: capability.clone(),
                    provider_plugin_id: "core".to_string(),
                },
            );
        }

        for plugin in self.activation_order_for(plugin_ids)? {
            for capability in &plugin.declaration.provided_capabilities {
                if let Some(existing) = providers.get(capability) {
                    return Err(PluginError::DuplicateCapabilityProvider {
                        capability: capability.as_str().to_string(),
                        first_provider: existing.provider_plugin_id.clone(),
                        second_provider: plugin.declaration.id.as_str().to_string(),
                    });
                }
                providers.insert(
                    capability.clone(),
                    CapabilityProvider {
                        capability: capability.clone(),
                        provider_plugin_id: plugin.declaration.id.as_str().to_string(),
                    },
                );
            }
        }

        Ok(providers)
    }

    pub fn validate_registered_plugin(
        registered_plugin: &RegisteredPlugin,
        host: &HostMetadata,
        available_capabilities: &BTreeMap<HostScope, CapabilityProvider>,
    ) -> Result<()> {
        let report = Self::compatibility_report(registered_plugin, host);
        if !report.api_compatible {
            return Err(PluginError::IncompatibleApiVersion {
                plugin_id: registered_plugin.declaration.id.as_str().to_string(),
                required: registered_plugin.declaration.plugin_api.to_string(),
                host: host.plugin_api_version,
            });
        }
        if !report.abi_compatible {
            return Err(PluginError::IncompatibleAbiVersion {
                plugin_id: registered_plugin.declaration.id.as_str().to_string(),
                required: registered_plugin.declaration.native_abi.to_string(),
                host: host.plugin_abi_version,
            });
        }

        for capability in &registered_plugin.declaration.required_capabilities {
            if !available_capabilities.contains_key(capability) {
                return Err(PluginError::MissingRequiredCapability {
                    plugin_id: registered_plugin.declaration.id.as_str().to_string(),
                    capability: capability.as_str().to_string(),
                });
            }
        }

        let entry_path = registered_plugin.manifest.resolve_entry_path(
            registered_plugin
                .manifest_path
                .parent()
                .unwrap_or_else(|| Path::new(".")),
        );
        if !entry_path.exists() {
            return Err(PluginError::MissingEntryFile {
                plugin_id: registered_plugin.declaration.id.as_str().to_string(),
                path: entry_path,
            });
        }

        Ok(())
    }

    pub fn service_providers_for(
        &self,
        plugin_ids: &[String],
    ) -> Result<BTreeMap<(HostScope, crate::ServiceKind, String), ServiceProvider>> {
        let mut providers: BTreeMap<(HostScope, crate::ServiceKind, String), ServiceProvider> =
            BTreeMap::new();
        for plugin in self.activation_order_for(plugin_ids)? {
            for service in &plugin.declaration.services {
                let registered = RegisteredService {
                    capability: service.capability.clone(),
                    kind: service.kind,
                    interface_id: service.interface_id.clone(),
                    provider: crate::ServiceProviderId::Plugin(
                        plugin.declaration.id.as_str().to_string(),
                    ),
                };
                if let Some(existing) = providers.get(&registered.key()) {
                    return Err(PluginError::DuplicateServiceProvider {
                        capability: registered.capability.as_str().to_string(),
                        kind: registered.kind,
                        interface_id: registered.interface_id.clone(),
                        first_provider: existing.service.provider.to_string(),
                        second_provider: registered.provider.to_string(),
                    });
                }
                providers.insert(
                    registered.key(),
                    ServiceProvider {
                        service: registered,
                    },
                );
            }
        }
        Ok(providers)
    }

    /// # Errors
    ///
    /// Returns an error when any plugin is incompatible with the host or when a
    /// native entry file is missing.
    pub fn validate_against_host(
        &self,
        host: &HostMetadata,
        plugin_ids: &[String],
        core_capabilities: &[HostScope],
    ) -> Result<()> {
        let available_capabilities =
            self.capability_providers_for(plugin_ids, core_capabilities)?;
        for plugin in self.activation_order_for(plugin_ids)? {
            Self::validate_registered_plugin(plugin, host, &available_capabilities)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::PluginRegistry;
    use crate::{ApiVersion, HostMetadata, HostScope, PluginManifest};
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
required_capabilities = ["bmux.commands"]

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
            .validate_against_host(
                &host,
                &["git.status".to_string()],
                &[HostScope::new("bmux.commands").expect("scope should parse")],
            )
            .expect("plugin should validate");
    }

    #[test]
    fn activation_order_sorts_required_dependencies_first() {
        let dir = temp_dir();
        fs::write(dir.join("libsessions.dylib"), []).expect("sessions entry should be written");
        fs::write(dir.join("libfollow.dylib"), []).expect("follow entry should be written");

        let sessions = PluginManifest::from_toml_str(
            r#"
id = "bmux.sessions"
name = "Sessions"
version = "0.1.0"
entry = "libsessions.dylib"

[plugin_api]
minimum = "1.0"

[native_abi]
minimum = "1.0"
"#,
        )
        .expect("sessions manifest should parse");
        let follow = PluginManifest::from_toml_str(
            r#"
id = "bmux.follow"
name = "Follow"
version = "0.1.0"
entry = "libfollow.dylib"

[[dependencies]]
plugin_id = "bmux.sessions"
version_req = "^0.1"

[plugin_api]
minimum = "1.0"

[native_abi]
minimum = "1.0"
"#,
        )
        .expect("follow manifest should parse");

        let mut registry = PluginRegistry::new();
        registry
            .register_manifest(&dir.join("sessions.toml"), sessions)
            .expect("sessions registration should succeed");
        registry
            .register_manifest(&dir.join("follow.toml"), follow)
            .expect("follow registration should succeed");

        let order = registry
            .activation_order_for(&["bmux.follow".to_string(), "bmux.sessions".to_string()])
            .expect("dependency activation order should succeed");
        assert_eq!(order[0].declaration.id.as_str(), "bmux.sessions");
        assert_eq!(order[1].declaration.id.as_str(), "bmux.follow");
    }

    #[test]
    fn capability_providers_detect_duplicate_plugin_ownership() {
        let dir = temp_dir();
        fs::write(dir.join("one.dylib"), []).expect("one entry should be written");
        fs::write(dir.join("two.dylib"), []).expect("two entry should be written");

        let one = PluginManifest::from_toml_str(
            r#"
id = "one.plugin"
name = "One"
version = "0.1.0"
entry = "one.dylib"
provided_capabilities = ["bmux.windows.read"]

[plugin_api]
minimum = "1.0"

[native_abi]
minimum = "1.0"
"#,
        )
        .expect("first manifest should parse");
        let two = PluginManifest::from_toml_str(
            r#"
id = "two.plugin"
name = "Two"
version = "0.1.0"
entry = "two.dylib"
provided_capabilities = ["bmux.windows.read"]

[plugin_api]
minimum = "1.0"

[native_abi]
minimum = "1.0"
"#,
        )
        .expect("second manifest should parse");

        let mut registry = PluginRegistry::new();
        registry
            .register_manifest(&dir.join("one.toml"), one)
            .expect("first registration should succeed");
        registry
            .register_manifest(&dir.join("two.toml"), two)
            .expect("second registration should succeed");

        let error = registry
            .capability_providers_for(&["one.plugin".to_string(), "two.plugin".to_string()], &[])
            .expect_err("duplicate providers should fail");
        assert!(error.to_string().contains("bmux.windows.read"));
    }

    #[test]
    fn service_providers_detect_duplicate_service_registration() {
        let dir = temp_dir();
        fs::write(dir.join("one.dylib"), []).expect("one entry should be written");
        fs::write(dir.join("two.dylib"), []).expect("two entry should be written");

        let one = PluginManifest::from_toml_str(
            r#"
id = "one.plugin"
name = "One"
version = "0.1.0"
entry = "one.dylib"
provided_capabilities = ["bmux.windows.read"]

[[services]]
capability = "bmux.windows.read"
kind = "query"
interface_id = "window-query/v1"

[plugin_api]
minimum = "1.0"

[native_abi]
minimum = "1.0"
"#,
        )
        .expect("first manifest should parse");

        let two = PluginManifest::from_toml_str(
            r#"
id = "two.plugin"
name = "Two"
version = "0.1.0"
entry = "two.dylib"
provided_capabilities = ["bmux.windows.read"]

[[services]]
capability = "bmux.windows.read"
kind = "query"
interface_id = "window-query/v1"

[plugin_api]
minimum = "1.0"

[native_abi]
minimum = "1.0"
"#,
        )
        .expect("second manifest should parse");

        let mut registry = PluginRegistry::new();
        registry
            .register_manifest(&dir.join("one.toml"), one)
            .expect("first registration should succeed");
        registry
            .register_manifest(&dir.join("two.toml"), two)
            .expect("second registration should succeed");

        let error = registry
            .service_providers_for(&["one.plugin".to_string(), "two.plugin".to_string()])
            .expect_err("duplicate service providers should fail");
        assert!(error.to_string().contains("window-query/v1"));
    }
}
