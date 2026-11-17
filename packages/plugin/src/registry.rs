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

#[derive(Debug, Clone, Default)]
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
        // Bundled manifests are generated from the plugin's BPDL world, so the
        // declaration is exactly the manifest's; nothing is merged at runtime.
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
            PluginEntrypoint::Native => {
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

/// Inventory origin is packaging metadata, not plugin identity or product policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginCandidateSource {
    Bundled,
    Dynamic { manifest_path: PathBuf },
}

/// Inert candidate: discovery never opens executable code or creates an instance.
#[derive(Debug, Clone)]
pub struct PluginCandidate {
    pub source: PluginCandidateSource,
    pub plugin: RegisteredPlugin,
}

/// Explicit override is by manifest locator, never directory iteration order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PluginCandidatePreference {
    #[default]
    Default,
    Bundled,
    Dynamic {
        manifest_path: PathBuf,
    },
}

#[derive(Debug, Clone)]
pub struct PluginSelectionRequest {
    pub plugin_id: String,
    pub role: bmux_plugin_sdk::definition::ComponentRole,
    pub preference: PluginCandidatePreference,
    pub plugin_api: bmux_plugin_sdk::ApiVersion,
    pub native_abi: bmux_plugin_sdk::ApiVersion,
    pub contracts: Vec<bmux_plugin_sdk::definition::ContractDescriptor>,
}

/// Selection is final. Loading errors must propagate; this value has no fallback list.
#[derive(Debug, Clone)]
pub struct SelectedPluginCandidate {
    candidate: PluginCandidate,
    role: bmux_plugin_sdk::definition::ComponentRole,
}

#[derive(Debug, Clone, Default)]
pub struct PluginCandidateInventory {
    candidates: BTreeMap<String, Vec<PluginCandidate>>,
}

fn candidate_error(details: impl Into<String>) -> PluginError {
    PluginError::ServiceProtocol {
        details: details.into(),
    }
}

impl PluginCandidateInventory {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Import a declaration assembled by an existing registrar during migration.
    /// Dynamic locators are canonicalized through the same inventory path; bundled
    /// generated service descriptors are retained rather than reconstructed from TOML.
    ///
    /// # Errors
    /// Returns an error for invalid identity/declarations or duplicate bundled candidates.
    pub fn add_registered_plugin(&mut self, plugin: &RegisteredPlugin) -> Result<()> {
        plugin.declaration.validate()?;
        if plugin.manifest.to_declaration()? != plugin.declaration {
            return Err(candidate_error(
                "registered declaration does not match its manifest",
            ));
        }
        if !plugin.bundled_static {
            return self.add_dynamic_manifest(
                &plugin.search_root,
                &plugin.manifest_path,
                plugin.manifest.clone(),
            );
        }
        let candidates = self
            .candidates
            .entry(plugin.manifest.id.clone())
            .or_default();
        if candidates
            .iter()
            .any(|candidate| candidate.source == PluginCandidateSource::Bundled)
        {
            return Err(candidate_error("duplicate bundled plugin candidate"));
        }
        candidates.push(PluginCandidate {
            source: PluginCandidateSource::Bundled,
            plugin: plugin.clone(),
        });
        Ok(())
    }

    /// Inventory an explicit locator even when it is outside all discovery roots.
    /// The configured ID is an enforced precondition, not a lookup hint.
    /// Failed identity validation leaves this inventory unchanged.
    ///
    /// # Errors
    /// Returns an error for unreadable/invalid manifests or a configured ID mismatch.
    pub fn add_dynamic_manifest_path_for_plugin(
        &mut self,
        plugin_id: &str,
        manifest_path: &Path,
    ) -> Result<()> {
        let manifest_path = fs::canonicalize(manifest_path)?;
        let manifest = PluginManifest::from_path(&manifest_path)?;
        if manifest.id != plugin_id {
            return Err(candidate_error(format!(
                "selected manifest identifies '{}', expected '{plugin_id}'",
                manifest.id,
            )));
        }
        let root = manifest_path.parent().unwrap_or_else(|| Path::new("."));
        self.add_dynamic_manifest(root, &manifest_path, manifest)
    }

    /// Logical IDs only; enumeration does not imply enablement or source selection.
    pub fn plugin_ids(&self) -> impl Iterator<Item = &str> {
        self.candidates.keys().map(String::as_str)
    }

    /// Add a dynamic manifest without reading its executable artifact.
    /// Repeated roots identifying the same manifest are idempotent, not competing candidates.
    ///
    /// # Errors
    /// Returns an error for invalid declarations, unreadable locators or conflicting re-inventory.
    pub fn add_dynamic_manifest(
        &mut self,
        search_root: &Path,
        manifest_path: &Path,
        manifest: PluginManifest,
    ) -> Result<()> {
        let declaration = manifest.to_declaration()?;
        let manifest_path = fs::canonicalize(manifest_path)?;
        let candidates = self.candidates.entry(manifest.id.clone()).or_default();
        if let Some(existing) = candidates.iter().find(|candidate| {
            candidate.source
                == (PluginCandidateSource::Dynamic {
                    manifest_path: manifest_path.clone(),
                })
        }) {
            if existing.plugin.manifest != manifest {
                return Err(candidate_error(
                    "manifest changed during candidate inventory",
                ));
            }
            return Ok(());
        }
        candidates.push(PluginCandidate {
            source: PluginCandidateSource::Dynamic {
                manifest_path: manifest_path.clone(),
            },
            plugin: RegisteredPlugin {
                search_root: search_root.to_path_buf(),
                manifest_path,
                manifest,
                declaration,
                bundled_static: false,
            },
        });
        Ok(())
    }

    /// Add a bundled declaration from its generated manifest.
    ///
    /// # Errors
    /// Returns an error for an invalid or duplicate bundled candidate.
    pub fn add_bundled_manifest(&mut self, manifest_toml: &str) -> Result<()> {
        let mut registry = PluginRegistry::new();
        registry.register_bundled_manifest(manifest_toml)?;
        for (_, plugin) in registry.plugins {
            let candidates = self
                .candidates
                .entry(plugin.manifest.id.clone())
                .or_default();
            if candidates
                .iter()
                .any(|candidate| candidate.source == PluginCandidateSource::Bundled)
            {
                return Err(candidate_error("duplicate bundled plugin candidate"));
            }
            candidates.push(PluginCandidate {
                source: PluginCandidateSource::Bundled,
                plugin,
            });
        }
        Ok(())
    }

    pub fn iter(&self) -> impl Iterator<Item = &PluginCandidate> {
        self.candidates.values().flatten()
    }

    /// Select exactly one artifact before checking its compatibility. An incompatible
    /// chosen artifact is an error, not a reason to silently choose another candidate.
    ///
    /// # Errors
    /// Returns an error for missing/ambiguous selections or incompatible selected components.
    pub fn select(&self, request: &PluginSelectionRequest) -> Result<SelectedPluginCandidate> {
        let candidates = self
            .candidates
            .get(&request.plugin_id)
            .ok_or_else(|| candidate_error(format!("no candidates for '{}'", request.plugin_id)))?;
        let canonical_override = match &request.preference {
            PluginCandidatePreference::Dynamic { manifest_path } => {
                Some(fs::canonicalize(manifest_path)?)
            }
            _ => None,
        };
        let has_bundled = candidates
            .iter()
            .any(|candidate| candidate.source == PluginCandidateSource::Bundled);
        let mut eligible = candidates
            .iter()
            .filter(|candidate| match &request.preference {
                PluginCandidatePreference::Default => {
                    !has_bundled || candidate.source == PluginCandidateSource::Bundled
                }
                PluginCandidatePreference::Bundled => {
                    candidate.source == PluginCandidateSource::Bundled
                }
                PluginCandidatePreference::Dynamic { .. } => match &candidate.source {
                    PluginCandidateSource::Dynamic { manifest_path } => {
                        Some(manifest_path) == canonical_override.as_ref()
                    }
                    PluginCandidateSource::Bundled => false,
                },
            });
        let candidate = eligible
            .next()
            .ok_or_else(|| candidate_error("explicit plugin selection not found"))?;
        if eligible.next().is_some() {
            return Err(candidate_error(format!(
                "ambiguous candidates for '{}'; select an explicit manifest",
                request.plugin_id
            )));
        }
        let manifest = &candidate.plugin.manifest;
        let declaration = &candidate.plugin.declaration;
        if !manifest.component_roles.contains(&request.role) {
            return Err(candidate_error(
                "selected plugin does not export requested component role",
            ));
        }
        // An open upper bound never authorizes an unnegotiated major interface change.
        if (declaration.plugin_api.maximum.is_none()
            && declaration.plugin_api.minimum.major != request.plugin_api.major)
            || !declaration.plugin_api.contains(request.plugin_api)
            || (declaration.native_abi.maximum.is_none()
                && declaration.native_abi.minimum.major != request.native_abi.major)
            || !declaration.native_abi.contains(request.native_abi)
        {
            return Err(candidate_error(
                "selected plugin API/native ABI version is incompatible",
            ));
        }
        for required in &request.contracts {
            if !manifest.contracts.iter().any(|provided| {
                provided.interface_id == required.interface_id
                    && provided.major == required.major
                    && provided.minor >= required.minor
            }) {
                return Err(candidate_error(format!(
                    "selected plugin lacks compatible contract '{}'",
                    required.interface_id
                )));
            }
        }
        Ok(SelectedPluginCandidate {
            candidate: candidate.clone(),
            role: request.role,
        })
    }
}

fn manifest_identity_field(expected: &PluginManifest, observed: &PluginManifest) -> Result<String> {
    let expected = serde_json::to_value(expected).map_err(|error| {
        candidate_error(format!("serializing selected manifest identity: {error}"))
    })?;
    let observed = serde_json::to_value(observed).map_err(|error| {
        candidate_error(format!("serializing embedded manifest identity: {error}"))
    })?;
    Ok(expected
        .as_object()
        .and_then(|fields| {
            fields.iter().find_map(|(field, value)| {
                (observed.get(field) != Some(value)).then(|| field.clone())
            })
        })
        .unwrap_or_else(|| "unknown".into()))
}

impl SelectedPluginCandidate {
    #[must_use]
    pub const fn candidate(&self) -> &PluginCandidate {
        &self.candidate
    }

    #[must_use]
    pub const fn role(&self) -> bmux_plugin_sdk::definition::ComponentRole {
        self.role
    }

    /// Verify the dynamic entry bytes, using bounded memory, before executing code.
    /// Loader owners must bind this digest to the exact loaded bytes (e.g. a private
    /// immutable artifact copy); hashing a mutable pathname is not TOCTOU protection.
    ///
    /// # Errors
    /// Returns an error for absent/unreadable entries, bundled selection, or pin mismatch.
    pub fn validate_artifact_path(&self) -> Result<String> {
        use sha2::{Digest, Sha256};
        use std::io::Read;
        if self.candidate.plugin.bundled_static {
            return Err(candidate_error(
                "bundled artifacts require host executable identity",
            ));
        }
        let plugin = &self.candidate.plugin;
        let base = plugin
            .manifest_path
            .parent()
            .unwrap_or_else(|| Path::new("."));
        let path = plugin
            .manifest
            .resolve_entry_path(base)
            .ok_or_else(|| candidate_error("selected dynamic plugin has no entry artifact"))?;
        let mut file = fs::File::open(path)?;
        if !file.metadata()?.is_file() {
            return Err(candidate_error(
                "selected plugin entry is not a regular file",
            ));
        }
        let mut hash = Sha256::new();
        let mut buffer = vec![0_u8; 65536].into_boxed_slice();
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
        let mut digest = String::with_capacity(64);
        for byte in hash.finalize() {
            use std::fmt::Write;
            write!(digest, "{byte:02x}").expect("writing to String cannot fail");
        }
        self.validate_artifact_identity(&digest)?;
        Ok(digest)
    }

    /// Check an identity observed by the loader, never an identity asserted by a plugin.
    /// An unpinned digest is observation, not authenticity or authorization.
    ///
    /// # Errors
    /// Returns an error for malformed digest or a mismatch with the installation pin.
    pub fn validate_artifact_identity(&self, observed: &str) -> Result<()> {
        if observed.len() != 64 || !observed.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(candidate_error(
                "loader artifact identity must be a SHA-256 digest",
            ));
        }
        if self
            .candidate
            .plugin
            .manifest
            .artifact_sha256
            .as_ref()
            .is_some_and(|expected| !expected.eq_ignore_ascii_case(observed))
        {
            return Err(candidate_error(
                "selected plugin artifact identity mismatch",
            ));
        }
        Ok(())
    }

    /// Validate metadata returned by the selected artifact without activating it.
    /// Compare the complete embedded declaration, except packaging locators/pins,
    /// and require exact role, capability and contract identity.
    ///
    /// # Errors
    /// Returns an error for missing embedded declarations or any identity mismatch.
    pub fn validate_embedded_metadata(
        &self,
        metadata: &bmux_plugin_sdk::definition::PluginMetadata,
        observed_artifact_identity: &str,
    ) -> Result<()> {
        self.validate_artifact_identity(observed_artifact_identity)?;
        let expected = &self.candidate.plugin.manifest;
        let embedded = metadata
            .manifest_toml
            .as_deref()
            .ok_or_else(|| candidate_error("selected plugin has no embedded manifest"))?;
        let mut embedded = PluginManifest::from_toml_str(embedded)?;
        embedded.to_declaration()?;
        // Only executable locators and installation pins are packaging metadata;
        // runtime kind, arguments and worker semantics remain part of the contract.
        embedded.entry.clone_from(&expected.entry);
        embedded
            .artifact_sha256
            .clone_from(&expected.artifact_sha256);
        let roles_match = metadata.roles.len() == expected.component_roles.len()
            && expected
                .component_roles
                .iter()
                .all(|role| metadata.roles.iter().filter(|other| *other == role).count() == 1);
        let contracts_match = metadata.contracts.len() == expected.contracts.len()
            && expected.contracts.iter().all(|contract| {
                metadata
                    .contracts
                    .iter()
                    .filter(|other| *other == contract)
                    .count()
                    == 1
            });
        let capabilities_match = metadata.required_capabilities.len()
            == expected.required_capabilities.len()
            && expected.required_capabilities.iter().all(|capability| {
                metadata
                    .required_capabilities
                    .iter()
                    .filter(|other| other.as_str() == capability.as_str())
                    .count()
                    == 1
            });
        if &embedded != expected {
            let field = manifest_identity_field(expected, &embedded)?;
            return Err(candidate_error(format!(
                "selected artifact embedded manifest/contract identity mismatch: manifest.{field}"
            )));
        }
        for (field, matches) in [
            ("plugin_id", metadata.plugin_id == expected.id),
            ("version", metadata.version == expected.version),
            ("component_roles", roles_match),
            ("contracts", contracts_match),
            ("required_capabilities", capabilities_match),
        ] {
            if !matches {
                return Err(candidate_error(format!(
                    "selected artifact embedded manifest/contract identity mismatch: {field}"
                )));
            }
        }
        Ok(())
    }
}

impl PluginRegistry {
    /// Install an already selected candidate; never silently replace a registered plugin.
    /// The loader must validate embedded metadata before activation.
    ///
    /// # Errors
    /// Returns an error for duplicate logical plugin identity.
    pub fn register_selected_candidate(
        &mut self,
        selected: &SelectedPluginCandidate,
    ) -> Result<()> {
        let plugin = &selected.candidate.plugin;
        let id = plugin.declaration.id.as_str().to_string();
        if self.plugins.contains_key(&id) {
            return Err(PluginError::DuplicatePluginId { id });
        }
        self.plugins.insert(id, plugin.clone());
        Ok(())
    }
}

#[cfg(test)]
mod candidate_tests {
    use super::*;
    use bmux_plugin_sdk::ApiVersion;
    use bmux_plugin_sdk::definition::{ComponentRole, ContractDescriptor, PluginMetadata};

    fn manifest() -> &'static str {
        "id = 'example.plugin'\nname = 'Example'\nversion = '1.0.0'\nentry = 'plugin.bin'\n\
         component_roles = ['backend', 'frontend']\n\
         [[contracts]]\ninterface_id = 'example.interface'\nmajor = 1\nminor = 2\n\
         [plugin_api]\nminimum = '1.0'\nmaximum = '1.5'\n\
         [native_abi]\nminimum = '1.0'\nmaximum = '1.3'\n"
    }

    fn request() -> PluginSelectionRequest {
        PluginSelectionRequest {
            plugin_id: "example.plugin".into(),
            role: ComponentRole::Backend,
            preference: PluginCandidatePreference::Default,
            plugin_api: ApiVersion::new(1, 1),
            native_abi: ApiVersion::new(1, 1),
            contracts: vec![ContractDescriptor {
                interface_id: "example.interface".into(),
                major: 1,
                minor: 1,
            }],
        }
    }

    fn dynamic(inventory: &mut PluginCandidateInventory, root: &Path, name: &str) -> PathBuf {
        let path = root.join(name);
        fs::write(&path, manifest()).unwrap();
        inventory
            .add_dynamic_manifest(root, &path, PluginManifest::from_path(&path).unwrap())
            .unwrap();
        path
    }

    fn metadata() -> PluginMetadata {
        PluginMetadata {
            plugin_id: "example.plugin".into(),
            version: "1.0.0".into(),
            roles: vec![ComponentRole::Backend, ComponentRole::Frontend],
            required_capabilities: Vec::new(),
            contracts: vec![ContractDescriptor {
                interface_id: "example.interface".into(),
                major: 1,
                minor: 2,
            }],
            manifest_toml: Some(manifest().into()),
        }
    }

    #[test]
    fn legacy_service_alias_is_routing_identity_not_a_versioned_contract() {
        let text = "id = 'example.plugin'\nname = 'Example'\nversion = '1.0.0'\n\
             provided_capabilities = ['example.read']\n\
             [[services]]\ncapability = 'example.read'\ninterface_id = 'legacy-state'\nkind = 'query'\n".to_owned();
        let mut inventory = PluginCandidateInventory::new();
        inventory.add_bundled_manifest(&text).unwrap();
        let mut required = request();
        required.contracts.clear();
        let selected = inventory.select(&required).unwrap();
        assert_eq!(
            selected.candidate().plugin.declaration.services[0].interface_id,
            "legacy-state"
        );
        assert_eq!(
            selected.candidate().plugin.manifest.contracts,
            Vec::<bmux_plugin_sdk::ContractDescriptor>::new()
        );
        let metadata = PluginMetadata {
            plugin_id: "example.plugin".into(),
            version: "1.0.0".into(),
            roles: vec![ComponentRole::Backend],
            required_capabilities: Vec::new(),
            contracts: Vec::new(),
            manifest_toml: Some(text),
        };
        assert!(
            selected
                .validate_embedded_metadata(&metadata, &"a".repeat(64))
                .is_ok()
        );
        required.contracts = vec![ContractDescriptor {
            interface_id: "legacy-state".into(),
            major: 1,
            minor: 0,
        }];
        assert!(inventory.select(&required).is_err());
        let mut invented = metadata;
        invented.contracts = required.contracts;
        assert!(
            selected
                .validate_embedded_metadata(&invented, &"a".repeat(64))
                .is_err()
        );
    }

    #[test]
    fn explicit_manifest_identity_mismatch_does_not_mutate_inventory() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("selected.toml");
        fs::write(&path, manifest()).unwrap();
        let mut inventory = PluginCandidateInventory::new();
        assert!(
            inventory
                .add_dynamic_manifest_path_for_plugin("other.plugin", &path)
                .is_err()
        );
        assert_eq!(inventory.iter().count(), 0);
        inventory
            .add_dynamic_manifest_path_for_plugin("example.plugin", &path)
            .unwrap();
        assert_eq!(
            inventory.plugin_ids().collect::<Vec<_>>(),
            vec!["example.plugin"]
        );
        let mut selected = request();
        selected.preference = PluginCandidatePreference::Dynamic {
            manifest_path: path,
        };
        assert!(inventory.select(&selected).is_ok());
    }

    #[test]
    fn registrar_import_preserves_bundled_declaration_and_rejects_conflicts() {
        let mut registry = PluginRegistry::new();
        registry.register_bundled_manifest(manifest()).unwrap();
        let plugin = registry.get("example.plugin").unwrap();
        let mut inventory = PluginCandidateInventory::new();
        inventory.add_registered_plugin(plugin).unwrap();
        let selected = inventory.select(&request()).unwrap();
        assert_eq!(selected.candidate().plugin.declaration, plugin.declaration);
        assert!(inventory.add_registered_plugin(plugin).is_err());
        let mut invalid = plugin.clone();
        invalid.declaration.display_name = "Not the declared name".into();
        let mut empty = PluginCandidateInventory::new();
        assert!(empty.add_registered_plugin(&invalid).is_err());
        assert_eq!(empty.iter().count(), 0);
    }

    #[test]
    fn explicit_missing_selection_does_not_fall_back_to_bundled() {
        let root = tempfile::tempdir().unwrap();
        let mut inventory = PluginCandidateInventory::new();
        inventory.add_bundled_manifest(manifest()).unwrap();
        let mut selected = request();
        selected.preference = PluginCandidatePreference::Dynamic {
            manifest_path: root.path().join("missing.toml"),
        };
        assert!(inventory.select(&selected).is_err());
    }

    #[test]
    fn bundled_default_and_explicit_dynamic_override_are_separate() {
        let root = tempfile::tempdir().unwrap();
        let mut inventory = PluginCandidateInventory::new();
        let path = dynamic(&mut inventory, root.path(), "plugin.toml");
        inventory.add_bundled_manifest(manifest()).unwrap();
        assert_eq!(inventory.iter().count(), 2);
        assert_eq!(
            inventory.select(&request()).unwrap().candidate().source,
            PluginCandidateSource::Bundled
        );
        let mut selected = request();
        selected.preference = PluginCandidatePreference::Dynamic {
            manifest_path: path,
        };
        assert!(matches!(
            inventory.select(&selected).unwrap().candidate().source,
            PluginCandidateSource::Dynamic { .. }
        ));
    }

    #[test]
    fn ambiguous_dynamic_candidates_do_not_use_iteration_order() {
        let root = tempfile::tempdir().unwrap();
        let mut inventory = PluginCandidateInventory::new();
        let path = dynamic(&mut inventory, root.path(), "first.toml");
        dynamic(&mut inventory, root.path(), "second.toml");
        assert!(inventory.select(&request()).is_err());
        let mut selected = request();
        selected.preference = PluginCandidatePreference::Dynamic {
            manifest_path: path,
        };
        assert!(inventory.select(&selected).is_ok());
    }

    #[test]
    fn repeated_root_is_idempotent_but_duplicate_bundled_is_an_error() {
        let root = tempfile::tempdir().unwrap();
        let mut inventory = PluginCandidateInventory::new();
        let path = dynamic(&mut inventory, root.path(), "plugin.toml");
        inventory
            .add_dynamic_manifest(
                root.path(),
                &path,
                PluginManifest::from_path(&path).unwrap(),
            )
            .unwrap();
        assert_eq!(inventory.iter().count(), 1);
        inventory.add_bundled_manifest(manifest()).unwrap();
        assert!(inventory.add_bundled_manifest(manifest()).is_err());
    }

    #[test]
    fn selected_missing_artifact_errors_instead_of_falling_back_to_bundled() {
        let root = tempfile::tempdir().unwrap();
        let mut inventory = PluginCandidateInventory::new();
        let path = dynamic(&mut inventory, root.path(), "plugin.toml");
        inventory.add_bundled_manifest(manifest()).unwrap();
        let mut selected = request();
        selected.preference = PluginCandidatePreference::Dynamic {
            manifest_path: path,
        };
        // Metadata inventory succeeds despite the missing executable; only the selected
        // artifact is inspected, and this failure has no alternate-candidate outcome.
        let selected = inventory.select(&selected).unwrap();
        assert!(selected.validate_artifact_path().is_err());
        assert!(matches!(
            selected.candidate().source,
            PluginCandidateSource::Dynamic { .. }
        ));
    }

    #[test]
    fn incompatible_default_bundled_is_not_replaced_by_dynamic() {
        let root = tempfile::tempdir().unwrap();
        let mut inventory = PluginCandidateInventory::new();
        dynamic(&mut inventory, root.path(), "plugin.toml");
        inventory
            .add_bundled_manifest(&manifest().replace("maximum = '1.5'", "maximum = '1.0'"))
            .unwrap();
        assert!(inventory.select(&request()).is_err());
    }

    #[test]
    fn role_and_contract_bounds_are_checked_before_loading() {
        let mut inventory = PluginCandidateInventory::new();
        inventory.add_bundled_manifest(manifest()).unwrap();
        let mut selected = request();
        selected.role = ComponentRole::Frontend;
        assert!(inventory.select(&selected).is_ok());
        selected.contracts[0].minor = 3;
        assert!(inventory.select(&selected).is_err());
        selected.contracts[0].minor = 0;
        selected.contracts[0].major = 2;
        assert!(inventory.select(&selected).is_err());
        selected.contracts.clear();
        selected.native_abi = ApiVersion::new(1, 4);
        assert!(inventory.select(&selected).is_err());
        selected.native_abi = ApiVersion::new(1, 3);
        selected.plugin_api = ApiVersion::new(1, 5);
        assert!(inventory.select(&selected).is_ok());
    }

    #[test]
    fn backend_only_legacy_manifest_rejects_frontend() {
        let mut inventory = PluginCandidateInventory::new();
        let manifest = manifest().replace("component_roles = ['backend', 'frontend']", "");
        inventory.add_bundled_manifest(&manifest).unwrap();
        let mut selected = request();
        selected.role = ComponentRole::Frontend;
        assert!(inventory.select(&selected).is_err());
    }

    #[test]
    fn selected_artifact_digest_and_embedded_identity_are_validated() {
        let root = tempfile::tempdir().unwrap();
        let mut inventory = PluginCandidateInventory::new();
        let path = root.path().join("plugin.toml");
        let digest = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let pinned = format!("artifact_sha256 = '{digest}'\n{}", manifest());
        fs::write(&path, &pinned).unwrap();
        fs::write(root.path().join("plugin.bin"), []).unwrap();
        inventory
            .add_dynamic_manifest(
                root.path(),
                &path,
                PluginManifest::from_toml_str(&pinned).unwrap(),
            )
            .unwrap();
        let selected = inventory.select(&request()).unwrap();
        assert_eq!(selected.validate_artifact_path().unwrap(), digest);
        assert!(
            selected
                .validate_embedded_metadata(&metadata(), digest)
                .is_ok()
        );
        assert!(
            selected
                .validate_embedded_metadata(&metadata(), &"a".repeat(64))
                .is_err()
        );
        fs::write(root.path().join("plugin.bin"), b"changed").unwrap();
        assert!(selected.validate_artifact_path().is_err());
        let mut changed = metadata();
        changed.version = "2.0.0".into();
        assert!(
            selected
                .validate_embedded_metadata(&changed, digest)
                .is_err()
        );
        changed = metadata();
        changed.contracts[0].minor = 9;
        assert!(
            selected
                .validate_embedded_metadata(&changed, digest)
                .is_err()
        );
        changed = metadata();
        changed.manifest_toml = Some(manifest().replace("name = 'Example'", "name = 'Other'"));
        let error = selected
            .validate_embedded_metadata(&changed, digest)
            .unwrap_err()
            .to_string();
        assert!(error.contains("manifest.name"));
        assert!(!error.contains("Other"));
        changed = metadata();
        changed.roles.push(ComponentRole::Backend);
        assert!(
            selected
                .validate_embedded_metadata(&changed, digest)
                .is_err()
        );
        changed = metadata();
        changed
            .required_capabilities
            .push("unexpected.capability".into());
        assert!(
            selected
                .validate_embedded_metadata(&changed, digest)
                .is_err()
        );
        changed = metadata();
        changed.manifest_toml = None;
        assert!(
            selected
                .validate_embedded_metadata(&changed, digest)
                .is_err()
        );
    }

    #[test]
    fn invalid_component_identity_and_inverted_bounds_are_rejected() {
        for invalid in [
            manifest().replace(
                "component_roles = ['backend', 'frontend']",
                "component_roles = []",
            ),
            manifest().replace(
                "component_roles = ['backend', 'frontend']",
                "component_roles = ['backend', 'backend']",
            ),
            format!("artifact_sha256 = 'bad'\n{}", manifest()),
        ] {
            assert!(PluginManifest::from_toml_str(&invalid).is_err());
        }
        let invalid = manifest().replace("minimum = '1.0'", "minimum = '1.9'");
        assert!(
            PluginManifest::from_toml_str(&invalid)
                .unwrap()
                .to_declaration()
                .is_err()
        );
    }

    #[test]
    fn open_upper_bound_does_not_implicitly_select_a_new_major() {
        let mut inventory = PluginCandidateInventory::new();
        inventory
            .add_bundled_manifest(&manifest().replace("maximum = '1.5'", ""))
            .unwrap();
        let mut selected = request();
        selected.plugin_api = ApiVersion::new(2, 0);
        assert!(inventory.select(&selected).is_err());
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
provided_capabilities = ["bmux.tabs.read"]

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
provided_capabilities = ["bmux.tabs.read"]

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
            .get(&HostScope::new("bmux.tabs.read").expect("scope should parse"))
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
provided_capabilities = ["bmux.tabs.read"]

[[services]]
capability = "bmux.tabs.read"
kind = "query"
interface_id = "tabs-state"

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
provided_capabilities = ["bmux.tabs.read"]

[[services]]
capability = "bmux.tabs.read"
kind = "query"
interface_id = "tabs-state"

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
            HostScope::new("bmux.tabs.read").expect("scope should parse"),
            bmux_plugin_sdk::ServiceKind::Query,
            "tabs-state".to_string(),
        );
        let selected = providers
            .get(&key)
            .expect("service provider should be present");
        assert_eq!(selected.service.provider.to_string(), "one.plugin");
    }

    #[cfg(unix)]
    #[test]
    fn registry_validates_process_runtime_entry_command() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir();
        let script = dir.join("process-entry.sh");
        fs::write(&script, "#!/bin/sh\nexit 0\n").expect("write process entry");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755))
            .expect("make process entry executable");
        let manifest = PluginManifest::from_toml_str(
            r#"
id = "process.plugin"
name = "Process Plugin"
version = "0.1.0"
runtime = "process"
entry = "./process-entry.sh"

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
