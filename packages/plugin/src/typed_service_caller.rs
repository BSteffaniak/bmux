//! Standalone [`ServiceCaller`] implementation for typed service
//! providers.
//!
//! Typed handles registered via `register_typed_services` can live
//! longer than any single [`crate::loader::NativeServiceContext`] or
//! [`crate::loader::NativeLifecycleContext`], so they need a
//! `ServiceCaller` they own outright. [`TypedServiceCaller`] captures
//! the same metadata every context carries (plugin identity,
//! capability lists, host description, kernel bridge) and drives the
//! public [`crate::loader::call_service_raw`] helper for each call.
//!
//! Providers build one of these at registration time from the fields
//! of a [`bmux_plugin_sdk::TypedServiceRegistrationContext`] and stash
//! it on their typed handle. The handle's trait methods then dispatch
//! host calls through the caller exactly like
//! [`crate::host_runtime::HostRuntimeApi`] defaults expect.

use bmux_plugin_sdk::{
    HostConnectionInfo, HostKernelBridge, HostMetadata, PluginError, RegisteredService, Result,
    ServiceKind, TypedServiceRegistrationContext,
};
use std::collections::BTreeMap;

use crate::host_runtime::ServiceCaller;
use crate::loader::{call_service_raw, execute_kernel_request};

/// Standalone [`ServiceCaller`] usable by long-lived typed service
/// handles.
#[derive(Debug, Clone)]
pub struct TypedServiceCaller {
    plugin_id: String,
    required_capabilities: Vec<String>,
    provided_capabilities: Vec<String>,
    services: Vec<RegisteredService>,
    available_capabilities: Vec<String>,
    enabled_plugins: Vec<String>,
    plugin_search_roots: Vec<String>,
    host: HostMetadata,
    connection: HostConnectionInfo,
    host_kernel_bridge: Option<HostKernelBridge>,
    plugin_settings_map: BTreeMap<String, toml::Value>,
}

impl TypedServiceCaller {
    /// Construct a caller by snapshotting every field of a
    /// [`TypedServiceRegistrationContext`].
    #[must_use]
    pub fn from_registration_context(context: &TypedServiceRegistrationContext<'_>) -> Self {
        Self {
            plugin_id: context.plugin_id.to_string(),
            required_capabilities: context.required_capabilities.to_vec(),
            provided_capabilities: context.provided_capabilities.to_vec(),
            services: context.services.to_vec(),
            available_capabilities: context.available_capabilities.to_vec(),
            enabled_plugins: context.enabled_plugins.to_vec(),
            plugin_search_roots: context.plugin_search_roots.to_vec(),
            host: context.host.clone(),
            connection: context.connection.clone(),
            host_kernel_bridge: context.host_kernel_bridge.copied(),
            plugin_settings_map: context.plugin_settings_map.clone(),
        }
    }

    /// Return the plugin id this caller identifies as.
    #[must_use]
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }
}

impl ServiceCaller for TypedServiceCaller {
    fn call_service_raw(
        &self,
        capability: &str,
        kind: ServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>> {
        call_service_raw(
            &self.plugin_id,
            &self.required_capabilities,
            &self.provided_capabilities,
            &self.services,
            &self.available_capabilities,
            &self.enabled_plugins,
            &self.plugin_search_roots,
            &self.host,
            &self.connection,
            self.host_kernel_bridge,
            &self.plugin_settings_map,
            capability,
            kind,
            interface_id,
            operation,
            payload,
        )
    }

    fn execute_kernel_request(
        &self,
        request: bmux_ipc::Request,
    ) -> Result<bmux_ipc::ResponsePayload> {
        let bridge = self
            .host_kernel_bridge
            .ok_or(PluginError::UnsupportedHostOperation {
                operation: "execute_kernel_request",
            })?;
        execute_kernel_request(Some(bridge), request)
    }
}
