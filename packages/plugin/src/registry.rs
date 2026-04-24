use crate::{PluginDeclaration, PluginEntrypoint, PluginManifest};
use bmux_plugin_sdk::{HostMetadata, HostScope, PluginError, RegisteredService, Result};
use semver::Version;
use std::collections::BTreeMap;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
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
    /// `true` when the plugin is statically linked into the binary and has no
    /// corresponding entry file on disk.
    pub bundled_static: bool,
}

#[derive(Debug, Default)]
pub struct PluginRegistry {
    plugins: BTreeMap<String, RegisteredPlugin>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityProvider {
    pub capability: HostScope,
    pub provider: bmux_plugin_sdk::ProviderId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceProvider {
    pub service: RegisteredService,
}

impl PluginRegistry {
    fn plugin_preferred_over(&self, candidate_id: &str, current_id: &str) -> bool {
        let Some(candidate) = self.get(candidate_id) else {
            return false;
        };
        let Some(current) = self.get(current_id) else {
            return true;
        };

        candidate.declaration.provider_priority > current.declaration.provider_priority
            || (candidate.declaration.provider_priority == current.declaration.provider_priority
                && candidate_id < current_id)
    }

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
                bundled_static: false,
            },
        );

        Ok(())
    }

    /// Register a statically-linked bundled plugin from an embedded manifest
    /// TOML string.  No filesystem paths are involved; the `search_root` and
    /// `manifest_path` are set to sentinel values.
    ///
    /// # Errors
    ///
    /// Returns an error when the manifest is invalid or uses a duplicate id.
    pub fn register_bundled_manifest(&mut self, manifest_toml: &str) -> Result<()> {
        let manifest = PluginManifest::from_toml_str(manifest_toml)?;
        let declaration = manifest.to_declaration()?;
        let plugin_id = declaration.id.as_str().to_string();

        if self.plugins.contains_key(&plugin_id) {
            return Err(PluginError::DuplicatePluginId { id: plugin_id });
        }

        let sentinel = PathBuf::from("<bundled-static>");
        self.plugins.insert(
            plugin_id,
            RegisteredPlugin {
                search_root: sentinel.clone(),
                manifest_path: sentinel,
                manifest,
                declaration,
                bundled_static: true,
            },
        );

        Ok(())
    }

    pub fn iter(&self) -> impl Iterator<Item = &RegisteredPlugin> {
        self.plugins.values()
    }

    #[must_use]
    pub fn get(&self, plugin_id: &str) -> Option<&RegisteredPlugin> {
        self.plugins.get(plugin_id)
    }

    /// Look up the `accepts_repeat` policy for a specific plugin
    /// command.
    ///
    /// Returns `false` when the plugin / command is unknown or the
    /// command did not declare `accepts_repeat = true` in its
    /// manifest. This is the signal the attach runtime uses to
    /// decide whether keyboard auto-repeat should fire additional
    /// invocations for a `PluginCommand` binding.
    #[must_use]
    pub fn command_accepts_repeat(&self, plugin_id: &str, command_name: &str) -> bool {
        self.get(plugin_id)
            .and_then(|plugin| {
                plugin
                    .declaration
                    .commands
                    .iter()
                    .find(|cmd| cmd.name == command_name)
            })
            .is_some_and(|cmd| cmd.accepts_repeat)
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
                    provider: bmux_plugin_sdk::ProviderId::Host,
                },
            );
        }

        for plugin in self.activation_order_for(plugin_ids)? {
            let plugin_id = plugin.declaration.id.as_str().to_string();
            for capability in &plugin.declaration.provided_capabilities {
                if let Some(existing) = providers.get(capability)
                    && match &existing.provider {
                        bmux_plugin_sdk::ProviderId::Host => true,
                        bmux_plugin_sdk::ProviderId::Plugin(existing_id) => {
                            !self.plugin_preferred_over(&plugin_id, existing_id)
                        }
                    }
                {
                    continue;
                }
                providers.insert(
                    capability.clone(),
                    CapabilityProvider {
                        capability: capability.clone(),
                        provider: bmux_plugin_sdk::ProviderId::Plugin(plugin_id.clone()),
                    },
                );
            }
        }

        Ok(providers)
    }

    /// Validate a plugin against host metadata and capabilities.
    ///
    /// # Errors
    ///
    /// Returns an error when the plugin is incompatible with the host or when
    /// its native entry file is missing.
    pub fn validate_registered_plugin(
        registered_plugin: &RegisteredPlugin,
        host: &HostMetadata,
        available_capabilities: &BTreeMap<HostScope, CapabilityProvider>,
    ) -> Result<()> {
        Self::validate_plugin_compat(registered_plugin, host, available_capabilities)?;

        match &registered_plugin.declaration.entrypoint {
            PluginEntrypoint::Native { .. } => {
                if let Some(entry_path) = registered_plugin.manifest.resolve_entry_path(
                    registered_plugin
                        .manifest_path
                        .parent()
                        .unwrap_or_else(|| Path::new(".")),
                ) && !entry_path.exists()
                {
                    return Err(PluginError::MissingEntryFile {
                        plugin_id: registered_plugin.declaration.id.as_str().to_string(),
                        path: entry_path,
                    });
                }
            }
            PluginEntrypoint::Process { command, .. } => {
                match process_command_status(registered_plugin, command) {
                    ProcessCommandStatus::Available => {}
                    ProcessCommandStatus::Missing(path) => {
                        return Err(PluginError::MissingEntryFile {
                            plugin_id: registered_plugin.declaration.id.as_str().to_string(),
                            path,
                        });
                    }
                    ProcessCommandStatus::NotExecutable(path) => {
                        return Err(PluginError::ProcessEntryNotExecutable {
                            plugin_id: registered_plugin.declaration.id.as_str().to_string(),
                            path,
                        });
                    }
                }
            }
        }

        Ok(())
    }

    /// Validate a statically-linked plugin against host metadata and
    /// capabilities.  This is identical to [`Self::validate_registered_plugin`]
    /// except it does **not** check for an entry file on disk (since the plugin
    /// is compiled into the binary).
    ///
    /// # Errors
    ///
    /// Returns an error when the plugin is incompatible with the host or
    /// a required capability is missing.
    pub fn validate_static_plugin(
        registered_plugin: &RegisteredPlugin,
        host: &HostMetadata,
        available_capabilities: &BTreeMap<HostScope, CapabilityProvider>,
    ) -> Result<()> {
        Self::validate_plugin_compat(registered_plugin, host, available_capabilities)
    }

    fn validate_plugin_compat(
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

        Ok(())
    }

    /// Build a map of service providers for the given plugins.
    ///
    /// # Errors
    ///
    /// Returns an error when dependency resolution fails for any of the
    /// requested plugins.
    pub fn service_providers_for(
        &self,
        plugin_ids: &[String],
    ) -> Result<BTreeMap<(HostScope, bmux_plugin_sdk::ServiceKind, String), ServiceProvider>> {
        let mut providers: BTreeMap<
            (HostScope, bmux_plugin_sdk::ServiceKind, String),
            ServiceProvider,
        > = BTreeMap::new();
        for plugin in self.activation_order_for(plugin_ids)? {
            let plugin_id = plugin.declaration.id.as_str().to_string();
            for service in &plugin.declaration.services {
                let registered = RegisteredService {
                    capability: service.capability.clone(),
                    kind: service.kind,
                    interface_id: service.interface_id.clone(),
                    provider: bmux_plugin_sdk::ProviderId::Plugin(plugin_id.clone()),
                };
                if let Some(existing) = providers.get(&registered.key())
                    && match &existing.service.provider {
                        bmux_plugin_sdk::ProviderId::Host => true,
                        bmux_plugin_sdk::ProviderId::Plugin(existing_id) => {
                            !self.plugin_preferred_over(&plugin_id, existing_id)
                        }
                    }
                {
                    continue;
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
            if plugin.bundled_static {
                Self::validate_static_plugin(plugin, host, &available_capabilities)?;
            } else {
                Self::validate_registered_plugin(plugin, host, &available_capabilities)?;
            }
        }

        Ok(())
    }
}

enum ProcessCommandStatus {
    Available,
    Missing(PathBuf),
    NotExecutable(PathBuf),
}

fn process_command_status(
    registered_plugin: &RegisteredPlugin,
    command: &str,
) -> ProcessCommandStatus {
    let command_path = Path::new(command);
    if command_path.components().count() > 1 {
        let resolved = registered_plugin.manifest.resolve_entry_path(
            registered_plugin
                .manifest_path
                .parent()
                .unwrap_or_else(|| Path::new(".")),
        );
        return match resolved {
            Some(path) if command_is_executable(&path) => ProcessCommandStatus::Available,
            Some(path) if path.exists() => ProcessCommandStatus::NotExecutable(path),
            Some(path) => ProcessCommandStatus::Missing(path),
            None => ProcessCommandStatus::Missing(PathBuf::from(command)),
        };
    }

    let mut first_non_exec: Option<PathBuf> = None;
    let is_available = std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths)
            .map(|path| path.join(command))
            .any(|candidate| {
                if command_is_executable(&candidate) {
                    return true;
                }
                if first_non_exec.is_none() && candidate.exists() {
                    first_non_exec = Some(candidate);
                }
                false
            })
    });

    if is_available {
        ProcessCommandStatus::Available
    } else if let Some(path) = first_non_exec {
        ProcessCommandStatus::NotExecutable(path)
    } else {
        ProcessCommandStatus::Missing(PathBuf::from(command))
    }
}

fn command_is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::PluginRegistry;
    use crate::PluginManifest;
    use bmux_plugin_sdk::{ApiVersion, HostMetadata, HostScope};
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
    fn capability_providers_choose_deterministic_plugin_provider() {
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

        let providers = registry
            .capability_providers_for(&["one.plugin".to_string(), "two.plugin".to_string()], &[])
            .expect("provider selection should succeed");
        let selected = providers
            .get(&HostScope::new("bmux.windows.read").expect("scope should parse"))
            .expect("capability should be present");
        assert_eq!(selected.provider.to_string(), "one.plugin");
    }

    #[test]
    fn service_providers_choose_highest_priority_then_plugin_id() {
        let dir = temp_dir();
        fs::write(dir.join("one.dylib"), []).expect("one entry should be written");
        fs::write(dir.join("two.dylib"), []).expect("two entry should be written");

        let one = PluginManifest::from_toml_str(
            r#"
id = "one.plugin"
name = "One"
version = "0.1.0"
entry = "one.dylib"
provider_priority = 10
provided_capabilities = ["bmux.windows.read"]

[[services]]
capability = "bmux.windows.read"
kind = "query"
interface_id = "windows-state"

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
interface_id = "windows-state"

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

        let providers = registry
            .service_providers_for(&["one.plugin".to_string(), "two.plugin".to_string()])
            .expect("service provider selection should succeed");
        let key = (
            HostScope::new("bmux.windows.read").expect("scope should parse"),
            bmux_plugin_sdk::ServiceKind::Query,
            "windows-state".to_string(),
        );
        let selected = providers
            .get(&key)
            .expect("service provider should be present");
        assert_eq!(selected.service.provider.to_string(), "one.plugin");
    }

    #[cfg(unix)]
    #[test]
    fn registry_validates_process_runtime_entry_command() {
        let dir = temp_dir();
        let manifest = PluginManifest::from_toml_str(
            r#"
id = "process.plugin"
name = "Process Plugin"
version = "0.1.0"
runtime = "process"
entry = "/usr/bin/true"

[plugin_api]
minimum = "1.0"

[native_abi]
minimum = "1.0"
"#,
        )
        .expect("manifest should parse");

        let mut registry = PluginRegistry::new();
        registry
            .register_manifest(&dir.join("plugin.toml"), manifest)
            .expect("manifest should register");

        let host = HostMetadata {
            product_name: "bmux".to_string(),
            product_version: "0.1.0".to_string(),
            plugin_api_version: ApiVersion::new(1, 0),
            plugin_abi_version: ApiVersion::new(1, 0),
        };

        registry
            .validate_against_host(&host, &["process.plugin".to_string()], &[])
            .expect("process plugin command should validate");
    }

    #[cfg(unix)]
    #[test]
    fn registry_rejects_non_executable_process_runtime_entry() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir();
        let script = dir.join("process-entry.sh");
        fs::write(&script, "#!/bin/sh\nexit 0\n").expect("script should be written");
        let mut permissions = fs::metadata(&script)
            .expect("script metadata should be readable")
            .permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&script, permissions).expect("script permissions should update");

        let manifest = PluginManifest::from_toml_str(&format!(
            r#"
id = "process.plugin"
name = "Process Plugin"
version = "0.1.0"
runtime = "process"
entry = "{}"

[plugin_api]
minimum = "1.0"

[native_abi]
minimum = "1.0"
"#,
            script.display()
        ))
        .expect("manifest should parse");

        let mut registry = PluginRegistry::new();
        registry
            .register_manifest(&dir.join("plugin.toml"), manifest)
            .expect("manifest should register");

        let host = HostMetadata {
            product_name: "bmux".to_string(),
            product_version: "0.1.0".to_string(),
            plugin_api_version: ApiVersion::new(1, 0),
            plugin_abi_version: ApiVersion::new(1, 0),
        };

        let error = registry
            .validate_against_host(&host, &["process.plugin".to_string()], &[])
            .expect_err("non-executable process entry should fail validation");
        assert!(error.to_string().contains("not executable"));
    }
}
