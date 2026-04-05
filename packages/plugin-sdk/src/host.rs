use crate::{HostScope, PluginError, RegisteredService, Result, ServiceKind};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostMetadata {
    pub product_name: String,
    pub product_version: String,
    pub plugin_api_version: crate::ApiVersion,
    pub plugin_abi_version: crate::ApiVersion,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostConnectionInfo {
    pub config_dir: String,
    pub runtime_dir: String,
    pub data_dir: String,
    pub state_dir: String,
}

pub trait PluginHost: Send + Sync {
    fn plugin_id(&self) -> &str;
    fn metadata(&self) -> &HostMetadata;
    fn connection(&self) -> &HostConnectionInfo;
    fn required_capabilities(&self) -> &BTreeSet<HostScope>;
    fn provided_capabilities(&self) -> &BTreeSet<HostScope>;

    fn has_capability(&self, capability: &HostScope) -> bool {
        self.required_capabilities().contains(capability)
            || self.provided_capabilities().contains(capability)
    }

    fn available_services(&self) -> &[RegisteredService];

    /// Resolve a registered service by capability, kind, and interface ID.
    ///
    /// The default implementation checks that the plugin has access to the
    /// requested capability, then searches the available service list.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::CapabilityAccessDenied`] if the plugin does not
    /// hold the requested capability, or [`PluginError::ServiceProtocol`] if
    /// no matching service registration is found.
    fn resolve_service(
        &self,
        capability: &HostScope,
        kind: ServiceKind,
        interface_id: &str,
    ) -> Result<&RegisteredService> {
        if !self.has_capability(capability) {
            return Err(PluginError::CapabilityAccessDenied {
                plugin_id: self.plugin_id().to_string(),
                capability: capability.as_str().to_string(),
                operation: "resolve_service",
            });
        }

        self.available_services()
            .iter()
            .find(|service| {
                service.capability == *capability
                    && service.kind == kind
                    && service.interface_id == interface_id
            })
            .ok_or_else(|| PluginError::ServiceProtocol {
                details: format!(
                    "missing service registration for capability '{}' ({kind:?}) interface '{}'",
                    capability.as_str(),
                    interface_id,
                ),
            })
    }
}

pub struct PluginContext<'a> {
    pub host: &'a dyn PluginHost,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CURRENT_PLUGIN_ABI_VERSION, CURRENT_PLUGIN_API_VERSION, ProviderId};

    struct MockHost {
        plugin_id: String,
        metadata: HostMetadata,
        connection: HostConnectionInfo,
        required_capabilities: BTreeSet<HostScope>,
        provided_capabilities: BTreeSet<HostScope>,
        services: Vec<RegisteredService>,
    }

    impl MockHost {
        fn new(required: &[&str], provided: &[&str], services: Vec<RegisteredService>) -> Self {
            Self {
                plugin_id: "example.plugin".to_string(),
                metadata: HostMetadata {
                    product_name: "bmux".to_string(),
                    product_version: "0.1.0".to_string(),
                    plugin_api_version: CURRENT_PLUGIN_API_VERSION,
                    plugin_abi_version: CURRENT_PLUGIN_ABI_VERSION,
                },
                connection: HostConnectionInfo {
                    config_dir: "/tmp/config".to_string(),
                    runtime_dir: "/tmp/runtime".to_string(),
                    data_dir: "/tmp/data".to_string(),
                    state_dir: "/tmp/state".to_string(),
                },
                required_capabilities: required
                    .iter()
                    .map(|value| HostScope::new(*value).expect("capability should parse"))
                    .collect(),
                provided_capabilities: provided
                    .iter()
                    .map(|value| HostScope::new(*value).expect("capability should parse"))
                    .collect(),
                services,
            }
        }
    }

    impl PluginHost for MockHost {
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
            &self.services
        }
    }

    #[test]
    fn resolve_service_allows_required_capability() {
        let capability = HostScope::new("example.read").expect("capability should parse");
        let host = MockHost::new(
            &["example.read"],
            &[],
            vec![RegisteredService {
                capability: capability.clone(),
                kind: ServiceKind::Query,
                interface_id: "example-query/v1".to_string(),
                provider: ProviderId::Plugin("provider.plugin".to_string()),
            }],
        );

        let service =
            PluginHost::resolve_service(&host, &capability, ServiceKind::Query, "example-query/v1")
                .expect("service should resolve");
        assert_eq!(service.provider.to_string(), "provider.plugin");
    }

    #[test]
    fn resolve_service_rejects_missing_capability() {
        let capability = HostScope::new("example.write").expect("capability should parse");
        let host = MockHost::new(
            &["example.read"],
            &[],
            vec![RegisteredService {
                capability: capability.clone(),
                kind: ServiceKind::Command,
                interface_id: "example-command/v1".to_string(),
                provider: ProviderId::Plugin("provider.plugin".to_string()),
            }],
        );

        let error = PluginHost::resolve_service(
            &host,
            &capability,
            ServiceKind::Command,
            "example-command/v1",
        )
        .expect_err("missing capability should fail");
        assert!(error.to_string().contains("example.write"));
    }
}
