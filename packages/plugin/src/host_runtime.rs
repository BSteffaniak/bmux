//! Host-side service caller trait and `HostRuntimeApi`.
//!
//! These traits require the host-side service dispatch implementation
//! (provided via `bmux_ipc`) and are therefore defined here rather than
//! in the SDK.
//!
//! `HostRuntimeApi` carries only truly generic primitives. Plugins
//! that speak in session/context/pane/client domain terms either use
//! [`ServiceCaller::execute_kernel_request`] for foundational access
//! to core IPC, or typed BPDL services (`call_service`) for
//! cross-plugin orchestration.

use bmux_plugin_sdk::{
    CORE_CLI_COMMAND_CAPABILITY, CORE_CLI_COMMAND_INTERFACE_V1,
    CORE_CLI_COMMAND_RUN_PATH_OPERATION_V1, CORE_CLI_COMMAND_RUN_PLUGIN_OPERATION_V1,
    CoreCliCommandRequest, CoreCliCommandResponse, LogWriteRequest, PluginCliCommandRequest,
    PluginCliCommandResponse, Result, ServiceKind, StorageGetRequest, StorageGetResponse,
    StorageSetRequest,
};
use serde::{Serialize, de::DeserializeOwned};

/// Authenticated application-owned kernel execution mechanism.
///
/// Implementations retain the actual authenticated invocation context and opaque
/// kernel grant. They enforce authorization and execution fencing at the side
/// effect, not from caller strings or fields in the encoded request. Synchronous
/// execution is restricted to blocking plugin execution workers by the host.
pub trait InvocationKernelDispatch: Send + Sync {
    /// Execute once using the existing negotiated IPC codec.
    ///
    /// # Errors
    /// Returns authorization, cancellation, fencing, protocol or executor errors.
    fn execute(
        &self,
        invocation: &bmux_plugin_runtime::InvocationScope,
        request: &[u8],
    ) -> Result<Vec<u8>>;
}

/// Kernel capability bound to one host-issued invocation.
///
/// Cloning retains authority but never extends admission beyond scope closure.
/// The opaque application dispatcher owns authenticated context; serializable
/// scope identities and plugin names are not substitutes for that capability.
#[derive(Clone)]
pub struct InvocationKernelPort {
    invocation: bmux_plugin_runtime::InvocationScope,
    dispatch: std::sync::Arc<dyn InvocationKernelDispatch>,
}

impl InvocationKernelPort {
    /// Construct only from trusted host wiring after granting kernel access.
    #[must_use]
    pub fn new(
        invocation: bmux_plugin_runtime::InvocationScope,
        dispatch: std::sync::Arc<dyn InvocationKernelDispatch>,
    ) -> Self {
        Self {
            invocation,
            dispatch,
        }
    }

    /// Attach the same kernel capability to static or ABI-adapted author APIs.
    #[must_use]
    pub fn bind_services(
        &self,
        services: bmux_plugin_sdk::PluginHostServices,
    ) -> bmux_plugin_sdk::PluginHostServices {
        let port = self.clone();
        services.with_kernel(move |request| port.execute_encoded(request))
    }

    /// Execute a bounded request exactly once. Never replay on response growth.
    ///
    /// # Errors
    /// Rejects oversized data, cancelled/closed admissions and executor failures.
    /// Cancellation or response failure after dispatch does not imply rollback.
    pub fn execute_encoded(&self, request: &[u8]) -> Result<Vec<u8>> {
        let protocol = |details: String| bmux_plugin_sdk::PluginError::ServiceProtocol { details };
        if request.len() > bmux_plugin_sdk::abi::MAX_BUFFER_BYTES {
            return Err(protocol(
                "kernel request exceeds negotiated buffer bound".into(),
            ));
        }
        if self
            .invocation
            .is_cancelled()
            .map_err(|error| protocol(error.to_string()))?
        {
            return Err(protocol(
                "kernel invocation cancelled before dispatch".into(),
            ));
        }
        let _admission = self
            .invocation
            .try_admit()
            .map_err(|error| protocol(error.to_string()))?;
        let response = self.dispatch.execute(&self.invocation, request)?;
        if response.len() > bmux_plugin_sdk::abi::MAX_BUFFER_BYTES {
            return Err(protocol(
                "kernel response exceeds negotiated buffer bound; do not replay operation".into(),
            ));
        }
        Ok(response)
    }
}

/// Trait for types that can dispatch cross-plugin service calls.
///
/// The three context types ([`NativeCommandContext`](crate::NativeCommandContext),
/// [`NativeLifecycleContext`](crate::NativeLifecycleContext),
/// [`NativeServiceContext`](crate::NativeServiceContext)) and the
/// long-lived [`TypedServiceCaller`](crate::TypedServiceCaller)
/// implement this trait.
pub trait ServiceCaller {
    /// Selected neutral connection roots, when explicitly bound by this caller.
    /// Missing scope never authorizes choosing a process-global storage root.
    fn host_connection(&self) -> Option<&bmux_plugin_sdk::HostConnectionInfo> {
        None
    }

    /// Resolve the current invocation's host-authenticated neutral origin.
    /// This is a host-context primitive, independent of domain plugin presence.
    /// No supplied identity or missing-provider fallback can establish locality.
    ///
    /// # Errors
    /// Fails closed when the bound bridge or representation is unavailable.
    fn current_stream_origin(&self) -> Result<crate::StreamOrigin> {
        match self.execute_kernel_request(bmux_ipc::Request::CurrentStreamProvenance)? {
            bmux_ipc::ResponsePayload::CurrentStreamProvenance { provenance } => Ok(provenance),
            _ => Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                details: "host stream origin unavailable".into(),
            }),
        }
    }
    /// Dispatch a raw service call with a binary payload.
    ///
    /// # Errors
    ///
    /// Returns an error when the capability is not accessible, the service
    /// is not registered, or the provider returns a transport-level error.
    fn call_service_raw(
        &self,
        capability: &str,
        kind: ServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>>;

    /// Open the selected owner's independent retained host capability.
    ///
    /// # Errors
    /// Unbound/unsupported access fails explicitly; no legacy registry fallback.
    fn open_retained_host(&self) -> Result<bmux_plugin_sdk::abi::retained::RetainedHostPort> {
        Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
            operation: "open_retained_host",
        })
    }

    /// Query authoritative selected-provider presence through bound authority.
    ///
    /// # Errors
    /// Unsupported lookup, denied access and registry failures remain errors,
    /// never `Absent`. The default does not inspect cached service inventories.
    fn selected_service_availability(
        &self,
        _key: bmux_plugin_sdk::typed_dispatch::ServiceAvailabilityKey,
    ) -> bmux_plugin_sdk::definition::PluginFuture<
        '_,
        Result<bmux_plugin_sdk::typed_dispatch::SelectedServiceAvailability>,
    > {
        Box::pin(async {
            Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
                operation: "selected_service_availability",
            })
        })
    }

    /// Dispatch through an explicitly asynchronous host capability.
    ///
    /// # Errors
    /// The default rejects unsupported dispatch before any synchronous effects.
    fn call_service_async(
        &self,
        _request: bmux_plugin_sdk::HostServiceOwnedRequest,
    ) -> bmux_plugin_sdk::definition::PluginFuture<'_, Result<Vec<u8>>> {
        Box::pin(async {
            Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
                operation: "call_service_async",
            })
        })
    }

    /// Dispatch a typed service call, serializing the request and
    /// deserializing the response.
    ///
    /// # Errors
    ///
    /// Returns an error when the capability is not accessible, the service
    /// is not registered, or the provider returns a transport-level error.
    fn call_service<Request, Response>(
        &self,
        capability: &str,
        kind: ServiceKind,
        interface_id: &str,
        operation: &str,
        request: &Request,
    ) -> Result<Response>
    where
        Request: Serialize,
        Response: DeserializeOwned,
    {
        let payload = bmux_plugin_sdk::encode_service_message(request)?;
        let response = self.call_service_raw(capability, kind, interface_id, operation, payload)?;
        bmux_plugin_sdk::decode_service_message(&response)
    }

    /// Dispatch an IPC request directly to the host kernel.
    ///
    /// Used by domain plugins (sessions, contexts, clients, windows)
    /// that own typed services backed by core state the host kernel
    /// exposes over IPC. The typed service layer would otherwise cycle
    /// back into the plugin's own handlers.
    ///
    /// # Errors
    ///
    /// Returns an error when the host kernel bridge is missing
    /// (typical in tests), or the server returns an error response.
    fn execute_kernel_request(
        &self,
        request: bmux_ipc::Request,
    ) -> Result<bmux_ipc::ResponsePayload>;
}

impl ServiceCaller for bmux_plugin_sdk::BoundHostServicePort {
    fn open_retained_host(&self) -> Result<bmux_plugin_sdk::abi::retained::RetainedHostPort> {
        Self::open_retained_host(self)
    }

    fn selected_service_availability(
        &self,
        key: bmux_plugin_sdk::typed_dispatch::ServiceAvailabilityKey,
    ) -> bmux_plugin_sdk::definition::PluginFuture<
        '_,
        Result<bmux_plugin_sdk::typed_dispatch::SelectedServiceAvailability>,
    > {
        Self::selected_service_availability(self, key)
    }

    fn call_service_raw(
        &self,
        capability: &str,
        kind: ServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>> {
        self.invoke(bmux_plugin_sdk::HostServiceRequest {
            capability,
            kind,
            interface: interface_id,
            operation,
            payload: &payload,
        })
    }

    fn execute_kernel_request(
        &self,
        _request: bmux_ipc::Request,
    ) -> Result<bmux_ipc::ResponsePayload> {
        Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
            operation: "execute_kernel_request",
        })
    }
}

/// Generic host runtime API.
///
/// Carries only domain-agnostic primitives: core-CLI command dispatch,
/// plugin-command dispatch, key-value storage, and logging.
/// Every blanket-implemented method is a thin wrapper over a well-known
/// host-provided service (`bmux.storage`, `bmux.logs.write`, etc.).
///
/// Domain-specific conveniences (session/context/pane/client helpers)
/// are not part of this trait. Foundational plugins reach core IPC via
/// [`ServiceCaller::execute_kernel_request`]; non-foundational plugins
/// use typed BPDL services through [`ServiceCaller::call_service`].
pub trait HostRuntimeApi: ServiceCaller {
    /// Run a core built-in CLI command path in-process.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn core_cli_command_run_path(
        &self,
        request: &CoreCliCommandRequest,
    ) -> Result<CoreCliCommandResponse> {
        self.call_service(
            CORE_CLI_COMMAND_CAPABILITY,
            ServiceKind::Command,
            CORE_CLI_COMMAND_INTERFACE_V1,
            CORE_CLI_COMMAND_RUN_PATH_OPERATION_V1,
            request,
        )
    }

    /// Run a plugin command in-process.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn plugin_command_run(
        &self,
        request: &PluginCliCommandRequest,
    ) -> Result<PluginCliCommandResponse> {
        self.call_service(
            CORE_CLI_COMMAND_CAPABILITY,
            ServiceKind::Command,
            CORE_CLI_COMMAND_INTERFACE_V1,
            CORE_CLI_COMMAND_RUN_PLUGIN_OPERATION_V1,
            request,
        )
    }

    /// Get a value from plugin storage.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn storage_get(&self, request: &StorageGetRequest) -> Result<StorageGetResponse> {
        self.call_service(
            "bmux.storage",
            ServiceKind::Query,
            "storage-query/v1",
            "get",
            request,
        )
    }

    /// Set a value in plugin storage.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn storage_set(&self, request: &StorageSetRequest) -> Result<()> {
        self.call_service(
            "bmux.storage",
            ServiceKind::Command,
            "storage-command/v1",
            "set",
            request,
        )
    }

    /// Write a log message.
    ///
    /// # Errors
    ///
    /// Returns an error when the service call fails.
    fn log_write(&self, request: &LogWriteRequest) -> Result<()> {
        self.call_service(
            "bmux.logs.write",
            ServiceKind::Command,
            "logging-command/v1",
            "write",
            request,
        )
    }
}

impl<T> HostRuntimeApi for T where T: ServiceCaller + ?Sized {}

#[cfg(test)]
mod kernel_port_tests {
    use super::*;
    use bmux_plugin_runtime::{
        ActivationScope, InstanceScope, InvocationScope, PluginConcurrencyConfig,
        PluginRuntimeScopeId,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct Executor {
        calls: Arc<AtomicUsize>,
        oversized: bool,
    }

    impl InvocationKernelDispatch for Executor {
        fn execute(&self, _invocation: &InvocationScope, request: &[u8]) -> Result<Vec<u8>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert!(matches!(
                bmux_ipc::decode::<bmux_ipc::Request>(request).unwrap(),
                bmux_ipc::Request::Ping
            ));
            if self.oversized {
                Ok(vec![0; bmux_plugin_sdk::abi::MAX_BUFFER_BYTES + 1])
            } else {
                Ok(bmux_ipc::encode(&bmux_ipc::ResponsePayload::Pong).unwrap())
            }
        }
    }

    fn scope() -> (ActivationScope, InvocationScope) {
        let instance = InstanceScope::new(PluginRuntimeScopeId::from_uuid(uuid::Uuid::new_v4()));
        let activation = instance
            .begin_activation(PluginConcurrencyConfig::Concurrent)
            .unwrap();
        activation.mark_active().unwrap();
        let invocation = activation.begin_invocation().unwrap();
        (activation, invocation)
    }

    #[tokio::test]
    async fn kernel_port_rejects_closed_invocation_and_activation() {
        for close_activation in [false, true] {
            let (activation, invocation) = scope();
            let calls = Arc::new(AtomicUsize::new(0));
            let port = InvocationKernelPort::new(
                invocation.clone(),
                Arc::new(Executor {
                    calls: calls.clone(),
                    oversized: false,
                }),
            );
            let request = bmux_ipc::encode(&bmux_ipc::Request::Ping).unwrap();
            port.execute_encoded(&request).unwrap();
            if close_activation {
                activation.close().unwrap();
            } else {
                invocation.close().unwrap();
            }
            assert!(port.execute_encoded(&request).is_err());
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            invocation.drain().await.unwrap();
            invocation.finish().unwrap();
            activation.drain().await.unwrap();
            activation.finish().unwrap();
        }
    }

    #[tokio::test]
    async fn bounds_reject_before_dispatch_and_never_replay_oversized_response() {
        let (activation, invocation) = scope();
        let calls = Arc::new(AtomicUsize::new(0));
        let port = InvocationKernelPort::new(
            invocation.clone(),
            Arc::new(Executor {
                calls: calls.clone(),
                oversized: true,
            }),
        );
        assert!(
            port.execute_encoded(&vec![0; bmux_plugin_sdk::abi::MAX_BUFFER_BYTES + 1])
                .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(
            port.execute_encoded(&bmux_ipc::encode(&bmux_ipc::Request::Ping).unwrap())
                .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        invocation.drain().await.unwrap();
        invocation.finish().unwrap();
        activation.drain().await.unwrap();
        activation.finish().unwrap();
    }
}
