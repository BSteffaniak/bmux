use crate::{HostScope, PluginError, RegisteredService, Result, ServiceKind, TypedServiceHandle};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostMetadata {
    pub product_name: String,
    pub product_version: String,
    pub plugin_api_version: crate::ApiVersion,
    pub plugin_abi_version: crate::ApiVersion,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostConnectionInfo {
    /// Canonical configuration directory. On macOS this is typically
    /// `~/Library/Application Support/bmux`; on Linux `~/.config/bmux`.
    /// Plugins that want to read user configuration files should
    /// prefer [`HostConnectionInfo::probe_config_file`] over joining
    /// against this path directly, because the host may expose a
    /// fallback chain through `config_dir_candidates`.
    pub config_dir: String,
    /// Ordered fallback chain for config-file lookups. The first entry
    /// is typically equal to `config_dir`; on macOS a second entry
    /// points at `~/.config/bmux` so XDG-style setups (e.g. home-manager
    /// managed dotfiles) resolve correctly. When empty, consumers fall
    /// back to `[config_dir]` alone. Serialized payloads that predate
    /// this field (recording playbooks, persisted state snapshots)
    /// round-trip cleanly thanks to `#[serde(default)]`.
    #[serde(default)]
    pub config_dir_candidates: Vec<String>,
    pub runtime_dir: String,
    pub data_dir: String,
    pub state_dir: String,
}

impl HostConnectionInfo {
    /// Probe the candidate config-dir chain for a file with the given
    /// relative path. Returns the first `candidate.join(relative)`
    /// that exists on disk.
    ///
    /// When `config_dir_candidates` is empty (older host or minimal
    /// test construction), the probe falls back to checking
    /// `config_dir.join(relative)` alone. This keeps the helper
    /// forward-compatible with payloads serialized before the
    /// candidate chain was introduced.
    #[must_use]
    pub fn probe_config_file(&self, relative: impl AsRef<Path>) -> Option<PathBuf> {
        let relative = relative.as_ref();
        let candidates: Vec<&str> = if self.config_dir_candidates.is_empty() {
            vec![self.config_dir.as_str()]
        } else {
            self.config_dir_candidates
                .iter()
                .map(String::as_str)
                .collect()
        };
        candidates
            .into_iter()
            .map(|dir| Path::new(dir).join(relative))
            .find(|path| path.exists())
    }

    /// Return the ordered list of candidate config directories as
    /// `Path` references. Falls back to `[config_dir]` when the
    /// explicit chain is empty, matching [`Self::probe_config_file`].
    pub fn config_dir_candidate_paths(&self) -> Vec<PathBuf> {
        if self.config_dir_candidates.is_empty() {
            vec![PathBuf::from(&self.config_dir)]
        } else {
            self.config_dir_candidates
                .iter()
                .map(PathBuf::from)
                .collect()
        }
    }
}

/// Result of looking up a registered service with optional typed dispatch.
///
/// Always carries the descriptor; optionally carries an in-process typed
/// handle when the provider is a native Rust plugin that registered a
/// trait impl via `register_typed_services`.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedService<'a> {
    descriptor: &'a RegisteredService,
    typed: Option<&'a TypedServiceHandle>,
}

impl<'a> ResolvedService<'a> {
    /// Construct a resolved service.
    #[must_use]
    pub const fn new(
        descriptor: &'a RegisteredService,
        typed: Option<&'a TypedServiceHandle>,
    ) -> Self {
        Self { descriptor, typed }
    }

    /// The service descriptor carrying capability, kind, interface id,
    /// and provider identity.
    #[must_use]
    pub const fn descriptor(&self) -> &'a RegisteredService {
        self.descriptor
    }

    /// The typed handle, if the provider registered one in-process.
    #[must_use]
    pub const fn typed(&self) -> Option<&'a TypedServiceHandle> {
        self.typed
    }
}

impl AsRef<RegisteredService> for ResolvedService<'_> {
    fn as_ref(&self) -> &RegisteredService {
        self.descriptor
    }
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

    /// Return the typed handle for a given service key, if one was
    /// registered by an in-process provider.
    ///
    /// The default implementation always returns `None`; hosts that
    /// support typed dispatch override this to look up their typed map.
    fn typed_handle(
        &self,
        _capability: &HostScope,
        _kind: ServiceKind,
        _interface_id: &str,
    ) -> Option<&TypedServiceHandle> {
        None
    }

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

    /// Resolve a registered service and any in-process typed handle
    /// registered for it.
    ///
    /// Consumers that can use typed dispatch call this variant and, if
    /// a typed handle is present, bypass byte-encoded calls by invoking
    /// the provider's trait directly. Consumers with no typed support
    /// use [`Self::resolve_service`] and the byte transport.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::resolve_service`].
    fn resolve_typed_service(
        &self,
        capability: &HostScope,
        kind: ServiceKind,
        interface_id: &str,
    ) -> Result<ResolvedService<'_>> {
        let descriptor = self.resolve_service(capability, kind, interface_id)?;
        let typed = self.typed_handle(capability, kind, interface_id);
        Ok(ResolvedService::new(descriptor, typed))
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
                    config_dir_candidates: vec!["/tmp/config".to_string()],
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

    #[test]
    fn resolve_typed_service_returns_descriptor_and_no_handle_by_default() {
        let capability = HostScope::new("example.read").expect("cap");
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
        let resolved = PluginHost::resolve_typed_service(
            &host,
            &capability,
            ServiceKind::Query,
            "example-query/v1",
        )
        .expect("resolves");
        assert_eq!(resolved.descriptor().interface_id, "example-query/v1");
        assert!(resolved.typed().is_none());
    }
}
