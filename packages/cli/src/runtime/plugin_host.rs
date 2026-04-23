use bmux_config::{BmuxConfig, ConfigPaths};
use bmux_plugin_sdk::{
    HostConnectionInfo, HostMetadata, HostScope, PluginHost, RegisteredService, ServiceKind,
    TypedServiceHandle, TypedServiceKey, TypedServiceRegistry,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub struct CliPluginHost {
    plugin_id: String,
    metadata: HostMetadata,
    connection: HostConnectionInfo,
    required_capabilities: BTreeSet<HostScope>,
    provided_capabilities: BTreeSet<HostScope>,
    available_services: Vec<RegisteredService>,
    typed_services: Arc<BTreeMap<TypedServiceKey, TypedServiceHandle>>,
}

impl CliPluginHost {
    pub fn for_plugin(
        plugin_id: impl Into<String>,
        metadata: HostMetadata,
        paths: &ConfigPaths,
        _config: BmuxConfig,
        required_capabilities: BTreeSet<HostScope>,
        provided_capabilities: BTreeSet<HostScope>,
        available_services: Vec<RegisteredService>,
    ) -> Self {
        Self {
            plugin_id: plugin_id.into(),
            metadata,
            connection: HostConnectionInfo {
                config_dir: paths.config_dir.to_string_lossy().into_owned(),
                config_dir_candidates: paths
                    .config_dir_candidates()
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect(),
                runtime_dir: paths.runtime_dir.to_string_lossy().into_owned(),
                data_dir: paths.data_dir.to_string_lossy().into_owned(),
                state_dir: paths.state_dir.to_string_lossy().into_owned(),
            },
            required_capabilities,
            provided_capabilities,
            available_services,
            typed_services: Arc::new(BTreeMap::new()),
        }
    }

    /// Replace the typed services map this host exposes.
    ///
    /// Consumers calling [`PluginHost::resolve_typed_service`] on this
    /// host will receive a [`bmux_plugin_sdk::ResolvedService`] whose
    /// `typed()` handle is looked up in this map.
    #[must_use]
    #[allow(dead_code)] // Consumed by host wiring landing in a follow-up.
    pub fn with_typed_services(
        mut self,
        typed_services: Arc<BTreeMap<TypedServiceKey, TypedServiceHandle>>,
    ) -> Self {
        self.typed_services = typed_services;
        self
    }

    /// Install a freshly built registry of typed services.
    #[allow(dead_code)] // Consumed by host wiring landing in a follow-up.
    pub fn set_typed_services_from_registry(&mut self, registry: TypedServiceRegistry) {
        self.typed_services = Arc::new(registry.into_entries());
    }
}

impl PluginHost for CliPluginHost {
    fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    fn metadata(&self) -> &HostMetadata {
        &self.metadata
    }

    fn connection(&self) -> &HostConnectionInfo {
        &self.connection
    }

    fn required_capabilities(&self) -> &BTreeSet<HostScope> {
        &self.required_capabilities
    }

    fn provided_capabilities(&self) -> &BTreeSet<HostScope> {
        &self.provided_capabilities
    }

    fn available_services(&self) -> &[RegisteredService] {
        &self.available_services
    }

    fn typed_handle(
        &self,
        capability: &HostScope,
        kind: ServiceKind,
        interface_id: &str,
    ) -> Option<&TypedServiceHandle> {
        self.typed_services
            .get(&(capability.clone(), kind, interface_id.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::CliPluginHost;
    use bmux_config::{BmuxConfig, ConfigPaths};
    use bmux_plugin_sdk::{
        CURRENT_PLUGIN_ABI_VERSION, CURRENT_PLUGIN_API_VERSION, HostMetadata, HostScope,
        PluginHost, RegisteredService, ServiceKind,
    };
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    fn service(capability: &str, kind: ServiceKind, interface_id: &str) -> RegisteredService {
        RegisteredService {
            capability: HostScope::new(capability).expect("capability should parse"),
            kind,
            interface_id: interface_id.to_string(),
            provider: bmux_plugin_sdk::ProviderId::Plugin("provider.plugin".to_string()),
        }
    }

    fn host(
        required: &[&str],
        provided: &[&str],
        services: Vec<RegisteredService>,
    ) -> CliPluginHost {
        CliPluginHost::for_plugin(
            "example.plugin",
            HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: CURRENT_PLUGIN_API_VERSION,
                plugin_abi_version: CURRENT_PLUGIN_ABI_VERSION,
            },
            &ConfigPaths::new(
                PathBuf::from("/tmp/config"),
                PathBuf::from("/tmp/runtime"),
                PathBuf::from("/tmp/data"),
                PathBuf::from("/tmp/state"),
            ),
            BmuxConfig::default(),
            required
                .iter()
                .map(|value| HostScope::new(*value).expect("capability should parse"))
                .collect::<BTreeSet<_>>(),
            provided
                .iter()
                .map(|value| HostScope::new(*value).expect("capability should parse"))
                .collect::<BTreeSet<_>>(),
            services,
        )
    }

    #[test]
    fn has_required_and_provided_capabilities() {
        let host = host(
            &["example.read"],
            &["example.write"],
            vec![service(
                "example.read",
                ServiceKind::Query,
                "example-query/v1",
            )],
        );

        assert_eq!(PluginHost::plugin_id(&host), "example.plugin");
        assert!(PluginHost::has_capability(
            &host,
            &HostScope::new("example.read").expect("capability should parse")
        ));
        assert!(PluginHost::has_capability(
            &host,
            &HostScope::new("example.write").expect("capability should parse")
        ));
        assert!(!PluginHost::has_capability(
            &host,
            &HostScope::new("example.admin").expect("capability should parse")
        ));
    }
}
