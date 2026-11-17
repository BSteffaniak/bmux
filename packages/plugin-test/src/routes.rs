//! The test host's service routing: every plugin (and the test itself)
//! reaches providers through the host, exactly as in a real server.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, Weak};

use bmux_plugin::LoadedExecution;
use bmux_plugin_sdk::abi::{NativeOperation, NativeOutcome};
use bmux_plugin_sdk::definition::PluginFuture;
use bmux_plugin_sdk::{
    ApiVersion, CancellationToken, HostConnectionInfo, HostMetadata, HostScope,
    HostServiceDispatch, HostServiceOwnedRequest, HostServiceRequest, NativeServiceContext,
    PluginError, PluginInvocationId, PortableServiceProvider, ProviderId, RegisteredService,
    ServiceKind, ServiceRequest,
};

pub fn failure(details: impl Into<String>) -> PluginError {
    PluginError::ServiceProtocol {
        details: details.into(),
    }
}

pub fn host_metadata() -> HostMetadata {
    HostMetadata {
        product_name: "bmux-test-host".into(),
        product_version: env!("CARGO_PKG_VERSION").into(),
        plugin_api_version: ApiVersion::new(1, 0),
        plugin_abi_version: ApiVersion::new(2, 0),
    }
}

pub const fn connection() -> HostConnectionInfo {
    HostConnectionInfo {
        config_dir: String::new(),
        config_dir_candidates: Vec::new(),
        runtime_dir: String::new(),
        data_dir: String::new(),
        state_dir: String::new(),
    }
}

/// Await one operation through its waker (no repolling), then acknowledge it.
pub async fn settle(
    execution: &LoadedExecution,
    id: u64,
) -> bmux_plugin_sdk::Result<NativeOutcome> {
    let outcome = std::future::poll_fn(|cx| match execution.poll(id, cx) {
        Ok(std::task::Poll::Ready(outcome)) => std::task::Poll::Ready(Ok(outcome)),
        Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
        Err(error) => std::task::Poll::Ready(Err(error)),
    })
    .await?;
    execution.acknowledge(id).await?;
    Ok(outcome)
}

/// Who serves one interface.
#[derive(Clone)]
enum Provider {
    /// A loaded plugin, reached through `PluginInstance::invoke` across its
    /// packaging boundary.
    Plugin {
        id: String,
        execution: Weak<LoadedExecution>,
        capability: String,
        kind: ServiceKind,
    },
    /// A fake registered by the test, reached through its checked adapter.
    Fake(Arc<dyn PortableServiceProvider>),
}

/// Interface id to provider.
#[derive(Default)]
pub struct Routes {
    providers: Mutex<BTreeMap<String, Provider>>,
}

impl Routes {
    pub fn add_plugin(
        &self,
        id: &str,
        execution: &Arc<LoadedExecution>,
        services: &[bmux_plugin_sdk::PluginService],
    ) -> bmux_plugin_sdk::Result<()> {
        let mut providers = self
            .providers
            .lock()
            .map_err(|_| failure("routes poisoned"))?;
        for service in services {
            if providers.contains_key(&service.interface_id) {
                return Err(failure(format!(
                    "interface '{}' already has a provider",
                    service.interface_id
                )));
            }
            providers.insert(
                service.interface_id.clone(),
                Provider::Plugin {
                    id: id.to_owned(),
                    execution: Arc::downgrade(execution),
                    capability: service.capability.as_str().to_owned(),
                    kind: service.kind,
                },
            );
        }
        drop(providers);
        Ok(())
    }

    pub fn add_fake(
        &self,
        provider: Arc<dyn PortableServiceProvider>,
    ) -> bmux_plugin_sdk::Result<()> {
        let interface = provider.descriptor().interface_id.as_str().to_owned();
        let mut providers = self
            .providers
            .lock()
            .map_err(|_| failure("routes poisoned"))?;
        if providers.contains_key(&interface) {
            return Err(failure(format!(
                "interface '{interface}' already has a provider"
            )));
        }
        providers.insert(interface, Provider::Fake(provider));
        drop(providers);
        Ok(())
    }

    fn provider(&self, interface: &str) -> bmux_plugin_sdk::Result<Provider> {
        self.providers
            .lock()
            .map_err(|_| failure("routes poisoned"))?
            .get(interface)
            .cloned()
            .ok_or_else(|| failure(format!("no provider serves '{interface}'")))
    }

    /// Serve one call from `caller`, which holds `granted` capabilities.
    pub fn call(
        self: &Arc<Self>,
        caller: &str,
        granted: Option<&BTreeSet<String>>,
        request: HostServiceOwnedRequest,
    ) -> PluginFuture<'static, bmux_plugin_sdk::Result<Vec<u8>>> {
        let routes = Arc::clone(self);
        let caller = caller.to_owned();
        let denied = granted.is_some_and(|granted| !granted.contains(&request.capability));
        Box::pin(async move {
            if denied {
                return Err(failure(format!(
                    "'{caller}' does not hold capability '{}'",
                    request.capability
                )));
            }
            match routes.provider(&request.interface)? {
                Provider::Fake(provider) => {
                    provider
                        .dispatch_checked(&request.operation, &request.payload, None)
                        .await
                }
                Provider::Plugin {
                    id,
                    execution,
                    capability,
                    kind,
                } => {
                    if capability != request.capability {
                        return Err(failure(format!(
                            "'{}' is served under '{capability}', not '{}'",
                            request.interface, request.capability
                        )));
                    }
                    let execution = execution
                        .upgrade()
                        .ok_or_else(|| failure(format!("provider '{id}' was unloaded")))?;
                    invoke(&execution, &id, &caller, &capability, kind, request).await
                }
            }
        })
    }
}

async fn invoke(
    execution: &LoadedExecution,
    provider: &str,
    caller: &str,
    capability: &str,
    kind: ServiceKind,
    request: HostServiceOwnedRequest,
) -> bmux_plugin_sdk::Result<Vec<u8>> {
    let context = NativeServiceContext {
        plugin_id: provider.to_owned(),
        request: ServiceRequest {
            caller_plugin_id: caller.to_owned(),
            service: RegisteredService {
                capability: HostScope::new(capability)?,
                kind,
                interface_id: request.interface,
                provider: ProviderId::Plugin(provider.to_owned()),
            },
            operation: request.operation,
            payload: request.payload,
        },
        required_capabilities: vec![],
        provided_capabilities: vec![],
        services: vec![],
        available_capabilities: vec![],
        enabled_plugins: vec![],
        plugin_search_roots: vec![],
        host: host_metadata(),
        connection: connection(),
        settings: None,
        plugin_settings_map: BTreeMap::new(),
        caller_client_id: None,
        cancellation: CancellationToken::new(),
        host_kernel_bridge: None,
        host_service_port: None,
    };
    let id = execution.begin(NativeOperation::Invoke {
        invocation_id: PluginInvocationId::new(),
        context,
    })?;
    match settle(execution, id).await? {
        NativeOutcome::Service(response) => match response.error {
            Some(error) => Err(failure(error.message)),
            None => Ok(response.payload),
        },
        NativeOutcome::Error { message, .. } => Err(failure(message)),
        other => Err(failure(format!("unexpected service outcome {other:?}"))),
    }
}

/// One plugin's host dispatch: its calls are attributed to it and checked
/// against the capabilities it was granted.
pub struct Dispatch {
    pub caller: String,
    pub granted: BTreeSet<String>,
    pub routes: Arc<Routes>,
}

impl HostServiceDispatch for Dispatch {
    fn invoke(
        &self,
        _: &bmux_plugin_runtime::InvocationScope,
        request: HostServiceRequest<'_>,
    ) -> bmux_plugin_sdk::Result<Vec<u8>> {
        Err(failure(format!(
            "the test host serves '{}' asynchronously only",
            request.interface
        )))
    }

    fn invoke_async(
        &self,
        _: &bmux_plugin_runtime::InvocationScope,
        request: HostServiceOwnedRequest,
    ) -> PluginFuture<'static, bmux_plugin_sdk::Result<Vec<u8>>> {
        self.routes.call(&self.caller, Some(&self.granted), request)
    }
}
