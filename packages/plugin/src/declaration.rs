use bmux_plugin_sdk::{
    CommandExecutionKind, HostScope, PluginCommand, PluginContext, PluginError,
    PluginEventSubscription, PluginFeature, PluginService, Result, VersionRange,
};
use semver::VersionReq;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PluginId(String);

impl PluginId {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if is_valid_plugin_id(&value) {
            Ok(Self(value))
        } else {
            Err(PluginError::InvalidPluginId { id: value })
        }
    }

    #[must_use]
    pub const fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginEntrypoint {
    Native { symbol: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PluginExecutionClass {
    NativeFast,
    #[default]
    NativeStandard,
    Interpreter,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginLifecycle {
    #[serde(default = "default_true")]
    pub activate_on_startup: bool,
    #[serde(default)]
    pub receive_events: bool,
    #[serde(default = "default_true")]
    pub allow_hot_reload: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginDependency {
    pub plugin_id: PluginId,
    #[serde(default = "default_dependency_version_req")]
    pub version_req: String,
    #[serde(default = "default_true")]
    pub required: bool,
}

impl PluginDependency {
    /// # Errors
    ///
    /// Returns an error when the version requirement cannot be parsed.
    pub fn parsed_version_req(&self) -> Result<VersionReq> {
        VersionReq::parse(&self.version_req).map_err(|error| {
            PluginError::InvalidDependencyVersion {
                plugin_id: self.plugin_id.as_str().to_string(),
                dependency_id: self.plugin_id.as_str().to_string(),
                version_req: self.version_req.clone(),
                details: error.to_string(),
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginDeclaration {
    pub id: PluginId,
    pub display_name: String,
    pub plugin_version: String,
    pub plugin_api: VersionRange,
    pub native_abi: VersionRange,
    pub entrypoint: PluginEntrypoint,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub provider_priority: i32,
    #[serde(default)]
    pub execution_class: PluginExecutionClass,
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
    pub dependencies: Vec<PluginDependency>,
    #[serde(default)]
    pub lifecycle: PluginLifecycle,
}

impl PluginDeclaration {
    /// # Errors
    ///
    /// Returns an error when the declaration contains duplicate command names or
    /// runtime hook commands without a hot-path capability.
    pub fn validate(&self) -> Result<()> {
        let mut command_names = BTreeSet::new();
        let mut dependency_ids = BTreeSet::new();
        for command in &self.commands {
            if !command_names.insert(command.name.clone()) {
                return Err(PluginError::DuplicateCommand {
                    plugin_id: self.id.as_str().to_string(),
                    command: command.name.clone(),
                });
            }

            if matches!(command.execution, CommandExecutionKind::RuntimeHook)
                && !self
                    .required_capabilities
                    .iter()
                    .any(HostScope::is_hot_path)
            {
                return Err(PluginError::MissingRequiredCapability {
                    plugin_id: self.id.as_str().to_string(),
                    capability: "bmux.terminal.input_intercept".to_string(),
                });
            }

            let canonical_path = command.canonical_path();
            if canonical_path.is_empty() || canonical_path.iter().any(std::string::String::is_empty)
            {
                return Err(PluginError::InvalidPluginCommandPath {
                    plugin_id: self.id.as_str().to_string(),
                    command: command.name.clone(),
                });
            }

            let mut seen_paths = BTreeSet::from([canonical_path.clone()]);
            for alias in &command.aliases {
                if alias.is_empty() || alias.iter().any(std::string::String::is_empty) {
                    return Err(PluginError::InvalidPluginCommandPath {
                        plugin_id: self.id.as_str().to_string(),
                        command: command.name.clone(),
                    });
                }
                if !seen_paths.insert(alias.clone()) {
                    return Err(PluginError::DuplicatePluginCommandAlias {
                        plugin_id: self.id.as_str().to_string(),
                        command: command.name.clone(),
                    });
                }
            }
        }

        for capability in &self.provided_capabilities {
            if self.required_capabilities.contains(capability) {
                return Err(PluginError::CapabilitySelfRequirement {
                    plugin_id: self.id.as_str().to_string(),
                    capability: capability.as_str().to_string(),
                });
            }
        }

        for service in &self.services {
            service.validate(self.id.as_str())?;
            if !self.provided_capabilities.contains(&service.capability) {
                return Err(PluginError::UnownedServiceCapability {
                    plugin_id: self.id.as_str().to_string(),
                    capability: service.capability.as_str().to_string(),
                    interface_id: service.interface_id.clone(),
                });
            }
        }

        for dependency in &self.dependencies {
            if dependency.plugin_id == self.id {
                return Err(PluginError::PluginDependencyOnSelf {
                    plugin_id: self.id.as_str().to_string(),
                });
            }
            if !dependency_ids.insert(dependency.plugin_id.as_str().to_string()) {
                return Err(PluginError::DuplicatePluginDependency {
                    plugin_id: self.id.as_str().to_string(),
                    dependency_id: dependency.plugin_id.as_str().to_string(),
                });
            }
            VersionReq::parse(&dependency.version_req).map_err(|error| {
                PluginError::InvalidDependencyVersion {
                    plugin_id: self.id.as_str().to_string(),
                    dependency_id: dependency.plugin_id.as_str().to_string(),
                    version_req: dependency.version_req.clone(),
                    details: error.to_string(),
                }
            })?;
        }

        Ok(())
    }
}

impl Default for PluginLifecycle {
    fn default() -> Self {
        Self {
            activate_on_startup: true,
            receive_events: false,
            allow_hot_reload: true,
        }
    }
}

pub trait NativePlugin: Send + Sync {
    fn declaration(&self) -> &PluginDeclaration;

    fn activate(&mut self, _context: &mut PluginContext<'_>) -> Result<()> {
        Ok(())
    }

    fn deactivate(&mut self, _context: &mut PluginContext<'_>) -> Result<()> {
        Ok(())
    }
}

const fn default_true() -> bool {
    true
}

fn is_valid_plugin_id(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };

    if !first.is_ascii_lowercase() {
        return false;
    }

    chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '-' | '_' | '.'))
}

fn default_dependency_version_req() -> String {
    "*".to_string()
}

#[cfg(test)]
mod tests {
    use super::{PluginDeclaration, PluginDependency, PluginEntrypoint, PluginId};
    use bmux_plugin_sdk::{
        ApiVersion, CommandExecutionKind, HostScope, PluginCommand, VersionRange,
    };
    use std::collections::BTreeSet;

    #[test]
    fn plugin_id_requires_stable_ascii_format() {
        assert!(PluginId::new("git.status").is_ok());
        assert!(PluginId::new("GitStatus").is_err());
    }

    #[test]
    fn validate_rejects_duplicate_commands() {
        let declaration = PluginDeclaration {
            id: PluginId::new("example.plugin").expect("id should parse"),
            display_name: "Example".to_string(),
            plugin_version: "0.1.0".to_string(),
            plugin_api: VersionRange::at_least(ApiVersion::new(1, 0)),
            native_abi: VersionRange::at_least(ApiVersion::new(1, 0)),
            entrypoint: PluginEntrypoint::Native {
                symbol: "bmux_plugin_entry_v1".to_string(),
            },
            description: None,
            homepage: None,
            provider_priority: 0,
            execution_class: super::PluginExecutionClass::NativeStandard,
            required_capabilities: BTreeSet::new(),
            provided_capabilities: BTreeSet::new(),
            provided_features: BTreeSet::new(),
            services: Vec::new(),
            commands: vec![
                PluginCommand {
                    name: "run".to_string(),
                    path: Vec::new(),
                    aliases: Vec::new(),
                    summary: "run".to_string(),
                    description: None,
                    arguments: Vec::new(),
                    execution: CommandExecutionKind::ProviderExec,
                    expose_in_cli: true,
                },
                PluginCommand {
                    name: "run".to_string(),
                    path: Vec::new(),
                    aliases: Vec::new(),
                    summary: "again".to_string(),
                    description: None,
                    arguments: Vec::new(),
                    execution: CommandExecutionKind::ProviderExec,
                    expose_in_cli: true,
                },
            ],
            event_subscriptions: Vec::new(),
            dependencies: Vec::new(),
            lifecycle: super::PluginLifecycle::default(),
        };

        assert!(declaration.validate().is_err());
    }

    #[test]
    fn validate_allows_runtime_hook_when_hot_path_scope_exists() {
        let mut required_capabilities = BTreeSet::new();
        required_capabilities
            .insert(HostScope::new("bmux.terminal.input_intercept").expect("scope should parse"));

        let declaration = PluginDeclaration {
            id: PluginId::new("example.runtime").expect("id should parse"),
            display_name: "Runtime".to_string(),
            plugin_version: "0.1.0".to_string(),
            plugin_api: VersionRange::at_least(ApiVersion::new(1, 0)),
            native_abi: VersionRange::at_least(ApiVersion::new(1, 0)),
            entrypoint: PluginEntrypoint::Native {
                symbol: "bmux_plugin_entry_v1".to_string(),
            },
            description: None,
            homepage: None,
            provider_priority: 0,
            execution_class: super::PluginExecutionClass::NativeStandard,
            required_capabilities,
            provided_capabilities: BTreeSet::new(),
            provided_features: BTreeSet::new(),
            services: Vec::new(),
            commands: vec![PluginCommand {
                name: "runtime".to_string(),
                path: Vec::new(),
                aliases: Vec::new(),
                summary: "runtime".to_string(),
                description: None,
                arguments: Vec::new(),
                execution: CommandExecutionKind::RuntimeHook,
                expose_in_cli: true,
            }],
            event_subscriptions: Vec::new(),
            dependencies: Vec::new(),
            lifecycle: super::PluginLifecycle::default(),
        };

        assert!(declaration.validate().is_ok());
    }

    #[test]
    fn validate_rejects_duplicate_dependencies() {
        let declaration = PluginDeclaration {
            id: PluginId::new("example.plugin").expect("id should parse"),
            display_name: "Example".to_string(),
            plugin_version: "0.1.0".to_string(),
            plugin_api: VersionRange::at_least(ApiVersion::new(1, 0)),
            native_abi: VersionRange::at_least(ApiVersion::new(1, 0)),
            entrypoint: PluginEntrypoint::Native {
                symbol: "bmux_plugin_entry_v1".to_string(),
            },
            description: None,
            homepage: None,
            provider_priority: 0,
            execution_class: super::PluginExecutionClass::NativeStandard,
            required_capabilities: BTreeSet::new(),
            provided_capabilities: BTreeSet::new(),
            provided_features: BTreeSet::new(),
            services: Vec::new(),
            commands: Vec::new(),
            event_subscriptions: Vec::new(),
            dependencies: vec![
                PluginDependency {
                    plugin_id: PluginId::new("bmux.sessions").expect("dep id should parse"),
                    version_req: "^0.1".to_string(),
                    required: true,
                },
                PluginDependency {
                    plugin_id: PluginId::new("bmux.sessions").expect("dep id should parse"),
                    version_req: "^0.1".to_string(),
                    required: true,
                },
            ],
            lifecycle: super::PluginLifecycle::default(),
        };

        assert!(declaration.validate().is_err());
    }
}
