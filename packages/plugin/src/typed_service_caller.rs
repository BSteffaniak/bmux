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

/// Codec adaptation for an already selected invocation capability. Errors are
/// terminal for this attempt; this path never consults the legacy bridge.
fn execute_selected_kernel(
    port: &bmux_plugin_sdk::PluginHostServices,
    request: &bmux_ipc::Request,
) -> Result<bmux_ipc::ResponsePayload> {
    let protocol = |details: String| PluginError::ServiceProtocol { details };
    let request = bmux_ipc::encode(request).map_err(|error| protocol(error.to_string()))?;
    if request.len() > bmux_plugin_sdk::abi::MAX_BUFFER_BYTES {
        return Err(protocol(
            "kernel request exceeds negotiated buffer bound".into(),
        ));
    }
    let response = port.execute_kernel_encoded(&request)?;
    if response.len() > bmux_plugin_sdk::abi::MAX_BUFFER_BYTES {
        return Err(protocol(
            "kernel response exceeds negotiated buffer bound; do not replay operation".into(),
        ));
    }
    bmux_ipc::decode(&response).map_err(|error| protocol(error.to_string()))
}

/// Standalone [`ServiceCaller`] usable by long-lived typed service
/// handles.
#[derive(Debug, Clone)]
pub struct TypedServiceCaller {
    plugin_id: String,
    host_service_port: Option<bmux_plugin_sdk::PluginHostServices>,
    caller_client_id: Option<uuid::Uuid>,
    cancellation: Option<bmux_plugin_sdk::CancellationToken>,
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
            host_service_port: context.host_service_port.cloned(),
            caller_client_id: None,
            cancellation: None,
            required_capabilities: context.required_capabilities.to_vec(),
            provided_capabilities: context.provided_capabilities.to_vec(),
            services: context.services.to_vec(),
            available_capabilities: context.available_capabilities.to_vec(),
            enabled_plugins: context.enabled_plugins.to_vec(),
            plugin_search_roots: context.plugin_search_roots.to_vec(),
            host: context.host.clone(),
            connection: context.connection.clone(),
            host_kernel_bridge: context.host_kernel_bridge.cloned(),
            plugin_settings_map: context.plugin_settings_map.clone(),
        }
    }

    /// Capture a standalone caller from a lifecycle context.
    #[must_use]
    pub fn from_lifecycle_context(context: &bmux_plugin_sdk::NativeLifecycleContext) -> Self {
        Self {
            caller_client_id: None,
            cancellation: None,
            plugin_id: context.plugin_id.clone(),
            host_service_port: context.host_service_port.clone(),
            required_capabilities: context.required_capabilities.clone(),
            provided_capabilities: context.provided_capabilities.clone(),
            services: context.services.clone(),
            available_capabilities: context.available_capabilities.clone(),
            enabled_plugins: context.enabled_plugins.clone(),
            plugin_search_roots: context.plugin_search_roots.clone(),
            host: context.host.clone(),
            connection: context.connection.clone(),
            host_kernel_bridge: context.host_kernel_bridge.clone(),
            plugin_settings_map: context.plugin_settings_map.clone(),
        }
    }

    /// Build an owned caller from a native command context.
    #[must_use]
    pub fn from_command_context(context: &bmux_plugin_sdk::NativeCommandContext) -> Self {
        Self {
            caller_client_id: context.caller_client_id,
            cancellation: None,
            plugin_id: context.plugin_id.clone(),
            host_service_port: context.host_service_port.clone(),
            required_capabilities: context.required_capabilities.clone(),
            provided_capabilities: context.provided_capabilities.clone(),
            services: context.services.clone(),
            available_capabilities: context.available_capabilities.clone(),
            enabled_plugins: context.enabled_plugins.clone(),
            plugin_search_roots: context.plugin_search_roots.clone(),
            host: context.host.clone(),
            connection: context.connection.clone(),
            host_kernel_bridge: context.host_kernel_bridge.clone(),
            plugin_settings_map: context.plugin_settings_map.clone(),
        }
    }

    /// Capture a standalone caller from a service invocation context.
    #[must_use]
    pub fn from_service_context(context: &bmux_plugin_sdk::NativeServiceContext) -> Self {
        Self {
            caller_client_id: context.caller_client_id,
            cancellation: Some(context.cancellation.clone()),
            plugin_id: context.plugin_id.clone(),
            host_service_port: context.host_service_port.clone(),
            required_capabilities: context.required_capabilities.clone(),
            provided_capabilities: context.provided_capabilities.clone(),
            services: context.services.clone(),
            available_capabilities: context.available_capabilities.clone(),
            enabled_plugins: context.enabled_plugins.clone(),
            plugin_search_roots: context.plugin_search_roots.clone(),
            host: context.host.clone(),
            connection: context.connection.clone(),
            host_kernel_bridge: context.host_kernel_bridge.clone(),
            plugin_settings_map: context.plugin_settings_map.clone(),
        }
    }

    /// Capture a legacy service context with a mandatory selected host port.
    /// M3 passes the host-issued capability alongside the serializable context;
    /// M11 uses this constructor until the SDK context carries that capability.
    /// This never derives authority from context payload identity fields.
    #[must_use]
    pub fn from_service_context_with_port(
        context: &bmux_plugin_sdk::NativeServiceContext,
        port: bmux_plugin_sdk::BoundHostServicePort,
    ) -> Self {
        Self::from_service_context(context).with_host_service_port(port)
    }

    /// Explicitly bind the host-owned invocation port. Once selected, dispatch
    /// never falls back to the legacy library bridge on a port error.
    #[must_use]
    pub fn with_host_service_port(mut self, port: bmux_plugin_sdk::BoundHostServicePort) -> Self {
        self.host_service_port = Some(bmux_plugin_sdk::PluginHostServices::from_bound(port));
        self
    }

    /// Reject side effects from a cancelled captured invocation.
    fn check_cancellation(&self) -> Result<()> {
        if self
            .cancellation
            .as_ref()
            .is_some_and(bmux_plugin_sdk::CancellationToken::is_cancelled)
        {
            return Err(PluginError::ServiceProtocol {
                details: "captured service invocation cancelled before dispatch".into(),
            });
        }
        Ok(())
    }

    /// Return the selected connection roots without inferring product authority.
    #[must_use]
    pub const fn connection(&self) -> &bmux_plugin_sdk::HostConnectionInfo {
        &self.connection
    }

    /// Return the plugin id this caller identifies as.
    #[must_use]
    pub const fn plugin_id(&self) -> &str {
        self.plugin_id.as_str()
    }

    /// Replace the host bridge used for remote dispatch and kernel requests.
    #[must_use]
    pub fn with_host_kernel_bridge(mut self, bridge: Option<HostKernelBridge>) -> Self {
        self.host_kernel_bridge = bridge;
        self
    }

    /// Replace the cancellation scope checked before each dispatch.
    #[must_use]
    pub fn with_cancellation(mut self, cancellation: bmux_plugin_sdk::CancellationToken) -> Self {
        self.cancellation = Some(cancellation);
        self
    }
}

impl ServiceCaller for TypedServiceCaller {
    fn open_retained_host(&self) -> Result<bmux_plugin_sdk::abi::retained::RetainedHostPort> {
        self.check_cancellation()?;
        self.host_service_port
            .as_ref()
            .ok_or(PluginError::UnsupportedHostOperation {
                operation: "open_retained_host",
            })?
            .open_retained_host()
    }

    fn selected_service_availability(
        &self,
        key: bmux_plugin_sdk::typed_dispatch::ServiceAvailabilityKey,
    ) -> bmux_plugin_sdk::definition::PluginFuture<
        '_,
        Result<bmux_plugin_sdk::typed_dispatch::SelectedServiceAvailability>,
    > {
        Box::pin(async move {
            self.check_cancellation()?;
            key.validate()?;
            let port =
                self.host_service_port
                    .as_ref()
                    .ok_or(PluginError::UnsupportedHostOperation {
                        operation: "selected_service_availability",
                    })?;
            let presence = port.selected_service_availability(key).await?;
            self.check_cancellation()?;
            Ok(presence)
        })
    }

    fn call_service_async(
        &self,
        request: bmux_plugin_sdk::HostServiceOwnedRequest,
    ) -> bmux_plugin_sdk::definition::PluginFuture<'_, Result<Vec<u8>>> {
        Box::pin(async move {
            self.check_cancellation()?;
            let port =
                self.host_service_port
                    .as_ref()
                    .ok_or(PluginError::UnsupportedHostOperation {
                        operation: "call_service_async",
                    })?;
            port.call_service_async(request).await
        })
    }

    fn host_connection(&self) -> Option<&bmux_plugin_sdk::HostConnectionInfo> {
        Some(&self.connection)
    }

    fn call_service_raw(
        &self,
        capability: &str,
        kind: ServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>> {
        self.check_cancellation()?;
        if let Some(port) = &self.host_service_port {
            return port.invoke(bmux_plugin_sdk::HostServiceRequest {
                capability,
                kind,
                interface: interface_id,
                operation,
                payload: &payload,
            });
        }
        call_service_raw(
            &self.plugin_id,
            self.caller_client_id,
            &self.required_capabilities,
            &self.provided_capabilities,
            &self.services,
            &self.available_capabilities,
            &self.enabled_plugins,
            &self.plugin_search_roots,
            &self.host,
            &self.connection,
            self.host_kernel_bridge.clone(),
            &self.plugin_settings_map,
            self.cancellation.as_ref(),
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
        self.check_cancellation()?;
        if let Some(port) = &self.host_service_port {
            return execute_selected_kernel(port, &request);
        }
        let bridge =
            self.host_kernel_bridge
                .clone()
                .ok_or(PluginError::UnsupportedHostOperation {
                    operation: "execute_kernel_request",
                })?;
        execute_kernel_request(Some(bridge), request)
    }
}

#[cfg(test)]
mod registration_capability_tests {
    use super::*;
    use bmux_plugin_runtime::{InstanceScope, PluginConcurrencyConfig, PluginRuntimeScopeId};
    use bmux_plugin_sdk::typed_dispatch::{SelectedServiceAvailability, ServiceAvailabilityKey};
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn registration_caller_preserves_activation_capability_not_temporary_invocation() {
        let instance = InstanceScope::new(PluginRuntimeScopeId::from_uuid(uuid::Uuid::new_v4()));
        let activation = instance
            .begin_activation(PluginConcurrencyConfig::Concurrent)
            .unwrap();
        activation.mark_active().unwrap();
        let registration = activation.begin_invocation().unwrap();
        let issued = Arc::new(Mutex::new(Vec::new()));
        let owner = activation.clone();
        let observed = issued.clone();
        let services = bmux_plugin_sdk::PluginHostServices::new(|_| {
            panic!("availability must not invoke service")
        })
        .with_service_availability(move |key| {
            let owner = owner.clone();
            let observed = observed.clone();
            Box::pin(async move {
                key.validate()?;
                let invocation =
                    owner
                        .begin_invocation()
                        .map_err(|error| PluginError::ServiceProtocol {
                            details: error.to_string(),
                        })?;
                {
                    let _admission = invocation.try_admit().unwrap();
                    observed.lock().unwrap().push(invocation.id());
                }
                invocation.close().unwrap();
                invocation.finish().unwrap();
                Ok(SelectedServiceAvailability::Present)
            })
        });
        let host = HostMetadata {
            product_name: "test".into(),
            product_version: "1".into(),
            plugin_api_version: bmux_plugin_sdk::ApiVersion::new(1, 0),
            plugin_abi_version: bmux_plugin_sdk::ApiVersion::new(1, 0),
        };
        let connection = HostConnectionInfo {
            config_dir: "/config".into(),
            config_dir_candidates: Vec::new(),
            runtime_dir: "/runtime".into(),
            data_dir: "/data".into(),
            state_dir: "/state".into(),
        };
        let settings = BTreeMap::new();
        let context = TypedServiceRegistrationContext {
            plugin_id: "example.provider",
            host_service_port: Some(&services),
            host_kernel_bridge: None,
            required_capabilities: &[],
            provided_capabilities: &[],
            services: &[],
            available_capabilities: &[],
            enabled_plugins: &[],
            plugin_search_roots: &[],
            host: &host,
            connection: &connection,
            plugin_settings_map: &settings,
        };
        let caller = TypedServiceCaller::from_registration_context(&context);
        let unbound =
            TypedServiceCaller::from_registration_context(&TypedServiceRegistrationContext {
                host_service_port: None,
                ..context
            });
        registration.close().unwrap();
        registration.finish().unwrap();
        let key = || ServiceAvailabilityKey {
            capability: "example.read".into(),
            kind: ServiceKind::Query,
            interface_id: "example/v1".into(),
        };
        assert!(matches!(
            unbound.selected_service_availability(key()).await,
            Err(PluginError::UnsupportedHostOperation {
                operation: "selected_service_availability"
            })
        ));
        for _ in 0..2 {
            assert_eq!(
                caller.selected_service_availability(key()).await.unwrap(),
                SelectedServiceAvailability::Present
            );
        }
        {
            let ids = issued.lock().unwrap();
            assert_eq!(ids.len(), 2);
            assert_ne!(ids[0], ids[1]);
            assert_ne!(ids[0], registration.id());
        }
        activation.close().unwrap();
        assert!(caller.selected_service_availability(key()).await.is_err());
        activation.drain().await.unwrap();
        activation.finish().unwrap();
    }
}

#[cfg(test)]
mod selected_kernel_tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn services() -> bmux_plugin_sdk::PluginHostServices {
        bmux_plugin_sdk::PluginHostServices::new(|_| panic!("kernel must not use service fallback"))
    }

    #[test]
    fn service_only_capability_rejects_kernel_without_fallback() {
        assert!(execute_selected_kernel(&services(), &bmux_ipc::Request::Ping).is_err());
    }

    #[test]
    fn selected_kernel_failure_is_terminal() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let port = services().with_kernel(move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
            Err(PluginError::ServiceProtocol {
                details: "application grant denied".into(),
            })
        });
        let error = execute_selected_kernel(&port, &bmux_ipc::Request::Ping).unwrap_err();
        assert!(error.to_string().contains("application grant denied"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn selected_kernel_uses_plain_ipc_request_and_payload_codec() {
        let port = services().with_kernel(|request| {
            assert!(matches!(
                bmux_ipc::decode::<bmux_ipc::Request>(request).unwrap(),
                bmux_ipc::Request::Ping
            ));
            Ok(bmux_ipc::encode(&bmux_ipc::ResponsePayload::Pong).unwrap())
        });
        assert!(matches!(
            execute_selected_kernel(&port, &bmux_ipc::Request::Ping).unwrap(),
            bmux_ipc::ResponsePayload::Pong
        ));
    }

    #[test]
    fn selected_kernel_request_bound_precedes_dispatch() {
        let port = services().with_kernel(|_| panic!("oversized request must not dispatch"));
        let request = bmux_ipc::Request::DeviceSealBroker {
            payload: vec![0; bmux_plugin_sdk::abi::MAX_BUFFER_BYTES + 1],
        };
        assert!(execute_selected_kernel(&port, &request).is_err());
    }

    #[test]
    fn selected_kernel_rejects_malformed_and_oversized_response_without_retry() {
        for oversized in [false, true] {
            let calls = Arc::new(AtomicUsize::new(0));
            let seen = calls.clone();
            let port = services().with_kernel(move |_| {
                seen.fetch_add(1, Ordering::SeqCst);
                Ok(if oversized {
                    vec![0; bmux_plugin_sdk::abi::MAX_BUFFER_BYTES + 1]
                } else {
                    Vec::new()
                })
            });
            assert!(execute_selected_kernel(&port, &bmux_ipc::Request::Ping).is_err());
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
    }
}
