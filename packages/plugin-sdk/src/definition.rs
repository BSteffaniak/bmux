//! One author implementation, independent of static or native packaging.
//!
//! Definitions are inert. Factories create a fresh implementation for each host
//! scope. The host owns admission, activation identity, draining and destruction;
//! dropping an observer is not permission to destroy a running implementation.

mod activation_capabilities;
#[cfg(test)]
mod retained_opener_tests;
pub use activation_capabilities::ActivationCapabilities;

use crate::{
    NativeCommandContext, NativeLifecycleContext, NativeServiceContext,
    NativeStreamingServiceContext, PluginCommandError, PluginEvent, ServiceResponse,
    TypedServiceRegistry,
};
use serde::{Deserialize, Serialize};
use std::{future::Future, pin::Pin, sync::Arc};

/// An object-safe asynchronous author operation. Only Rust-native adapters use
/// this type; neither its layout nor its executor crosses the native ABI.
pub type PluginFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type PluginResult<T> = std::result::Result<T, PluginCommandError>;

/// Host-selected component, not a resource identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentRole {
    Backend,
    Frontend,
}

/// Host-issued creation context. Metadata discovery never creates this scope.
/// UUIDs are observations supplied by the trusted host, not plugin credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginCreateContext {
    /// Present only when loading a fresh instance for this CLI command.
    /// Older hosts omit it; older factories retain their instance-bound behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli_command: Option<String>,
    pub runtime_scope: RuntimeScopeId,
    pub instance: LoadedInstanceId,
    pub role: ComponentRole,
}

// Runtime owns identity; these short author-facing names are aliases, not
// competing UUID wrappers. Boundary conversion never proves admission.
pub use bmux_plugin_runtime::{
    PluginActivationId as ActivationId, PluginInstanceId as LoadedInstanceId,
    PluginRuntimeScopeId as RuntimeScopeId,
};

/// Portable activation registration mutation. Authority comes from the bound
///
/// host callback, not a caller-supplied activation/resource identity. Publication
/// and withdrawal are deliberately absent: only the lifecycle owner drives them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum RegistrationMutation {
    Register {
        contract: crate::RegistrationContract,
        value: Vec<u8>,
    },
    SetRetained {
        contract: crate::RegistrationContract,
        expected_revision: u64,
        value: Vec<u8>,
    },
    Emit {
        contract: crate::RegistrationContract,
        value: Vec<u8>,
    },
    MarkReady {
        contract: crate::RegistrationContract,
    },
}
impl RegistrationMutation {
    /// # Errors
    /// Returns an error if the bound scope is unavailable, the request is invalid,
    /// or the implementation rejects the operation.
    pub fn validate(&self) -> PluginResult<()> {
        let (contract, value) = match self {
            Self::Register { contract, value }
            | Self::SetRetained {
                contract, value, ..
            }
            | Self::Emit { contract, value } => (contract, value.as_slice()),
            Self::MarkReady { contract } => (contract, &[][..]),
        };
        contract.validate().map_err(PluginCommandError::failed)?;
        if value.len() > crate::abi::MAX_BUFFER_BYTES {
            return Err(PluginCommandError::failed(
                "registration value exceeds native bound",
            ));
        }
        Ok(())
    }
}

/// Safe Rust host-service capability. Static wiring wraps M6's bound port;
/// native wiring wraps the retained C callback gate, never serializable pointers.
type ServiceInvocation =
    dyn for<'a> Fn(crate::HostServiceRequest<'a>) -> crate::Result<Vec<u8>> + Send + Sync;
type AsyncServiceInvocation =
    dyn Fn(crate::HostServiceOwnedRequest) -> crate::HostServiceFuture + Send + Sync;
type ServiceAvailabilityInvocation = dyn Fn(
        crate::typed_dispatch::ServiceAvailabilityKey,
    )
        -> PluginFuture<'static, crate::Result<crate::typed_dispatch::SelectedServiceAvailability>>
    + Send
    + Sync;
type KernelInvocation = dyn Fn(&[u8]) -> crate::Result<Vec<u8>> + Send + Sync;
type RegistrationInvocation = dyn Fn(RegistrationMutation) -> crate::Result<Vec<u8>> + Send + Sync;

/// Safe Rust host-service capability.
///
/// Static wiring wraps M6's bound port; native wiring wraps the retained C
/// callback gate. Equality compares the identity of all retained callback
/// allocations with `Arc::ptr_eq`, never closure behavior, wire metadata, or
/// permission equivalence. Clones compare equal; independently wrapped ports
/// compare unequal even if they happen to call the same underlying service.
/// Replacing a registration or kernel callback changes capability identity. Equality
/// does not establish that the capability is currently admitted or authorized.
#[derive(Clone)]
pub struct PluginHostServices {
    invoke: Arc<ServiceInvocation>,
    retained_host: Option<Arc<crate::host_services::RetainedHostOpener>>,
    retained_host_v2: Option<Arc<crate::host_services::RetainedHostOpener>>,
    asynchronous: Option<Arc<AsyncServiceInvocation>>,
    availability: Option<Arc<ServiceAvailabilityInvocation>>,
    registration: Option<Arc<RegistrationInvocation>>,
    kernel: Option<Arc<KernelInvocation>>,
    subscriptions: Option<Arc<crate::EventSubscriptionInvocation>>,
    publisher: Option<Arc<crate::host_services::EventPublisher>>,
}
// Capability equality is identity equality, never equality of payload metadata.
impl PartialEq for PluginHostServices {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.invoke, &other.invoke)
            && match (&self.retained_host, &other.retained_host) {
                (None, None) => true,
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                _ => false,
            }
            && match (&self.retained_host_v2, &other.retained_host_v2) {
                (None, None) => true,
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                _ => false,
            }
            && match (&self.asynchronous, &other.asynchronous) {
                (None, None) => true,
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                _ => false,
            }
            && match (&self.availability, &other.availability) {
                (None, None) => true,
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                _ => false,
            }
            && match (&self.kernel, &other.kernel) {
                (None, None) => true,
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                _ => false,
            }
            && match (&self.registration, &other.registration) {
                (None, None) => true,
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                _ => false,
            }
            && match (&self.subscriptions, &other.subscriptions) {
                (None, None) => true,
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                _ => false,
            }
            && match (&self.publisher, &other.publisher) {
                (None, None) => true,
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                _ => false,
            }
    }
}

impl std::fmt::Debug for PluginHostServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PluginHostServices(bound)")
    }
}
impl PluginHostServices {
    pub fn new(
        invoke: impl for<'a> Fn(crate::HostServiceRequest<'a>) -> crate::Result<Vec<u8>>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            invoke: Arc::new(invoke),
            retained_host: None,
            retained_host_v2: None,
            asynchronous: None,
            availability: None,
            registration: None,
            kernel: None,
            subscriptions: None,
            publisher: None,
        }
    }

    /// Bind the host's event subscription route for this activation.
    #[must_use]
    pub fn with_event_subscriptions(
        mut self,
        subscribe: impl Fn(&str, bool) -> crate::Result<crate::HostEventSubscription>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.subscriptions = Some(Arc::new(subscribe));
        self
    }

    /// Subscribe to a host event channel (`state` for a retained state
    /// channel). Works in both packaging modes; the handle is what generated
    /// clients build `subscribe_<event>()` on.
    ///
    /// # Errors
    /// The host bound no event route to these services, the activation is
    /// closed, or the channel cannot be observed serialized.
    pub fn subscribe_event(
        &self,
        kind: &str,
        state: bool,
    ) -> crate::Result<crate::HostEventSubscription> {
        let subscribe =
            self.subscriptions
                .as_ref()
                .ok_or(crate::PluginError::UnsupportedHostOperation {
                    operation: "subscribe_event",
                })?;
        subscribe(kind, state)
    }
    /// Bind the host's event publication route for this activation:
    /// `register(kind, state)` creates a serialized channel owned by the
    /// activation, `publish(kind, json)` publishes one JSON payload on it.
    #[must_use]
    pub fn with_event_publisher(
        mut self,
        register: impl Fn(&str, bool) -> crate::Result<()> + Send + Sync + 'static,
        publish: impl Fn(&str, &[u8]) -> crate::Result<()> + Send + Sync + 'static,
    ) -> Self {
        self.publisher = Some(Arc::new(crate::host_services::EventPublisher::new(
            register, publish,
        )));
        self
    }

    /// Register a serialized event channel owned by the activation these
    /// services belong to (`state` for a retained state channel, whose
    /// subscribers receive JSON `null` until the first publication). The
    /// host releases it with the activation. Works in both packaging modes.
    ///
    /// # Errors
    /// The host bound no publication route, the activation is closed, or
    /// the host refused the channel.
    pub fn register_event_channel(&self, kind: &str, state: bool) -> crate::Result<()> {
        self.publisher()?.register(kind, state)
    }

    /// Publish one JSON payload on a channel registered through
    /// [`Self::register_event_channel`] (or any channel that accepts
    /// serialized input).
    ///
    /// # Errors
    /// The host bound no publication route, the activation is closed, or
    /// the channel rejected the payload.
    pub fn publish_event(&self, kind: &str, payload: &[u8]) -> crate::Result<()> {
        if payload.len() > crate::abi::MAX_BUFFER_BYTES {
            return Err(crate::PluginError::ServiceProtocol {
                details: "event payload exceeds transport bound".into(),
            });
        }
        self.publisher()?.publish(kind, payload)
    }

    /// Whether the host bound an event publication route.
    #[must_use]
    pub const fn publishes_events(&self) -> bool {
        self.publisher.is_some()
    }

    fn publisher(&self) -> crate::Result<&crate::host_services::EventPublisher> {
        self.publisher
            .as_deref()
            .ok_or(crate::PluginError::UnsupportedHostOperation {
                operation: "publish_event",
            })
    }

    /// Bind an invocation-scoped encoded kernel capability. The host is
    /// responsible for foundational-plugin authorization and live cancellation.
    #[must_use]
    pub fn with_kernel(
        mut self,
        execute: impl Fn(&[u8]) -> crate::Result<Vec<u8>> + Send + Sync + 'static,
    ) -> Self {
        self.kernel = Some(Arc::new(execute));
        self
    }
    /// Execute opaque `bmux_ipc`-encoded `Request` / `ResponsePayload` bytes.
    /// This never discovers authority from request metadata or process globals.
    ///
    /// # Errors
    /// Returns `UnsupportedHostOperation` without a bound grant, rejects buffers
    /// larger than the native transport bound, and preserves execution errors.
    pub fn execute_kernel_encoded(&self, request: &[u8]) -> crate::Result<Vec<u8>> {
        let execute = self
            .kernel
            .as_ref()
            .ok_or(crate::PluginError::UnsupportedHostOperation {
                operation: "execute_kernel_encoded",
            })?;
        if request.len() > crate::abi::MAX_BUFFER_BYTES {
            return Err(crate::PluginError::ServiceProtocol {
                details: "kernel request exceeds transport bound".into(),
            });
        }
        let response = execute(request)?;
        if response.len() > crate::abi::MAX_BUFFER_BYTES {
            return Err(crate::PluginError::ServiceProtocol {
                details: "kernel response exceeds transport bound".into(),
            });
        }
        Ok(response)
    }
    /// Bind the selected activation's independent retained host ledger opener.
    ///
    /// Native adapters must acquire the negotiated independent table while the
    /// invocation is valid. Static hosts bind the same selected owner ledger.
    /// Rebindings of other callbacks preserve this opener and its identity.
    #[must_use]
    pub fn with_retained_host(
        mut self,
        open: impl Fn() -> crate::Result<crate::abi::retained::RetainedHostPort> + Send + Sync + 'static,
    ) -> Self {
        self.retained_host = Some(Arc::new(open));
        self
    }

    /// Open an independently owned retained table, not a borrowed invocation port.
    ///
    /// # Errors
    /// Missing negotiation or a closed/failed selected opener remains an error.
    /// There is no synchronous, invocation-local or ambient registry fallback.
    /// Success requires explicit operation settlement and port release later;
    /// dropping the capability never acknowledges work or proves cleanup.
    pub fn open_retained_host(&self) -> crate::Result<crate::abi::retained::RetainedHostPort> {
        self.retained_host
            .as_ref()
            .ok_or(crate::PluginError::UnsupportedHostOperation {
                operation: "open_retained_host",
            })?()
    }

    /// Bind the actual selected recovery-v2 independent retained-table opener.
    /// Other callback decorators preserve this opener and its allocation identity.
    #[must_use]
    pub fn with_retained_host_v2(
        mut self,
        open: impl Fn() -> crate::Result<crate::abi::retained::RetainedHostPort> + Send + Sync + 'static,
    ) -> Self {
        self.retained_host_v2 = Some(Arc::new(open));
        self
    }

    /// Open the explicitly selected recovery-v2 owner while origin authority is valid.
    ///
    /// # Errors
    /// Missing negotiation is unsupported. A selected rejection is returned
    /// unchanged, never retried through v1 or an ambient/invocation-local owner.
    /// Success transfers independent ownership requiring explicit settlement.
    pub fn open_retained_host_v2(&self) -> crate::Result<crate::abi::retained::RetainedHostPort> {
        self.retained_host_v2
            .as_ref()
            .ok_or(crate::PluginError::UnsupportedHostOperation {
                operation: "open_retained_host_v2",
            })?()
    }

    /// Bind an authoritative invocation-owned availability lookup.
    /// The host callback validates admission and caller access before lookup.
    #[must_use]
    pub fn with_service_availability(
        mut self,
        callback: impl Fn(
            crate::typed_dispatch::ServiceAvailabilityKey,
        ) -> PluginFuture<
            'static,
            crate::Result<crate::typed_dispatch::SelectedServiceAvailability>,
        > + Send
        + Sync
        + 'static,
    ) -> Self {
        self.availability = Some(Arc::new(callback));
        self
    }

    /// Observe exact selected-provider presence without invoking the service.
    ///
    /// # Errors
    /// Rejects invalid keys and unsupported negotiation. Lookup/access failures
    /// remain errors, never `Absent`; no synchronous or inventory fallback.
    pub fn selected_service_availability(
        &self,
        key: crate::typed_dispatch::ServiceAvailabilityKey,
    ) -> PluginFuture<'static, crate::Result<crate::typed_dispatch::SelectedServiceAvailability>>
    {
        let callback = self.availability.clone();
        Box::pin(async move {
            key.validate()?;
            let callback = callback.ok_or(crate::PluginError::UnsupportedHostOperation {
                operation: "selected_service_availability",
            })?;
            callback(key).await
        })
    }

    /// Bind the trusted host's asynchronous execution route. No worker marker or
    /// caller authority is accepted from request metadata.
    #[must_use]
    pub fn with_async(
        mut self,
        callback: impl Fn(
            crate::HostServiceOwnedRequest,
        ) -> PluginFuture<'static, crate::Result<Vec<u8>>>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.asynchronous = Some(Arc::new(callback));
        self
    }

    /// Schedule only through an explicitly selected asynchronous capability.
    ///
    /// # Errors
    /// Rejects unsupported async dispatch and oversized requests/responses.
    /// There is no ambient executor or synchronous dispatch fallback.
    pub fn call_service_async(
        &self,
        request: crate::HostServiceOwnedRequest,
    ) -> PluginFuture<'static, crate::Result<Vec<u8>>> {
        let callback = self.asynchronous.clone();
        Box::pin(async move {
            request.validate()?;
            let callback = callback.ok_or(crate::PluginError::UnsupportedHostOperation {
                operation: "call_service_async",
            })?;
            let response = callback(request).await?;
            if response.len() > crate::abi::MAX_BUFFER_BYTES {
                return Err(crate::PluginError::ServiceProtocol {
                    details: "async host response exceeds bound; operation must not be replayed"
                        .into(),
                });
            }
            Ok(response)
        })
    }

    #[must_use]
    pub fn with_registrations(
        mut self,
        registration: impl Fn(RegistrationMutation) -> crate::Result<Vec<u8>> + Send + Sync + 'static,
    ) -> Self {
        self.registration = Some(Arc::new(registration));
        self
    }
    /// # Errors
    /// Returns an error if the bound scope is unavailable, the request is invalid,
    /// or the implementation rejects the operation.
    pub fn registration(&self, mutation: RegistrationMutation) -> crate::Result<Vec<u8>> {
        mutation
            .validate()
            .map_err(|error| crate::PluginError::ServiceProtocol {
                details: error.to_string(),
            })?;
        self.registration
            .as_ref()
            .ok_or_else(|| crate::PluginError::ServiceProtocol {
                details: "activation registration port not bound".into(),
            })?(mutation)
    }
    #[must_use]
    pub fn from_bound(port: crate::BoundHostServicePort) -> Self {
        let asynchronous = port.clone();
        let availability = port.clone();
        let retained_host = port.clone();
        let bound_v2 = port.clone();
        Self::new(move |request| port.invoke(request))
            .with_retained_host(move || retained_host.open_retained_host())
            .with_retained_host_v2(move || bound_v2.open_retained_host_v2())
            .with_async(move |request| asynchronous.invoke_async(request))
            .with_service_availability(move |key| availability.selected_service_availability(key))
    }
    /// # Errors
    /// Returns an error if the bound scope is unavailable, the request is invalid,
    /// or the implementation rejects the operation.
    pub fn invoke(&self, request: crate::HostServiceRequest<'_>) -> crate::Result<Vec<u8>> {
        (self.invoke)(request)
    }
}

impl crate::TypedDispatchClient for PluginHostServices {
    fn requires_service_contracts(&self) -> bool {
        true
    }
    fn invoke_service_raw(
        &mut self,
        capability: &str,
        kind: bmux_ipc::InvokeServiceKind,
        interface: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> impl Future<Output = std::result::Result<Vec<u8>, crate::TypedDispatchClientError>> + Send
    {
        let future = self.call_service_async(crate::HostServiceOwnedRequest {
            capability: capability.into(),
            kind: match kind {
                bmux_ipc::InvokeServiceKind::Query => crate::ServiceKind::Query,
                bmux_ipc::InvokeServiceKind::Command => crate::ServiceKind::Command,
            },
            interface: interface.into(),
            operation: operation.into(),
            payload,
        });
        let interface = interface.to_owned();
        let operation = operation.to_owned();
        async move {
            future.await.map_err(|error| {
                crate::TypedDispatchClientError::transport(interface, operation, error.to_string())
            })
        }
    }
}

/// Live Rust-native invocation authority; never serialized as an executable port.
#[derive(Debug, Clone)]
pub struct PluginCallContext {
    /// Actual selected executor and authenticated invocation authority.
    pub callback_execution: Option<crate::PluginCallbackContext>,
    pub invocation_id: crate::PluginInvocationId,
    pub cancellation: crate::ServiceCancellation,
    pub host_services: Option<PluginHostServices>,
}

/// Lifecycle authority is bound by the host, never decoded from request payloads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginLifecycleContext {
    /// Actual host-selected execution authority, never decoded from wire.
    #[serde(skip)]
    pub callback_execution: Option<crate::PluginCallbackContext>,
    pub scope: PluginCreateContext,
    pub activation: ActivationId,
    pub native: NativeLifecycleContext,
    /// Host-supplied in-process values for this exact activation, filled by a
    /// host-registered activation binder from the loader's actual scope.
    /// Never serialized: dynamic artifacts cannot receive them.
    #[serde(skip)]
    pub capabilities: ActivationCapabilities,
}

/// A command's exit code, error text and generic metadata, returned as data.
///
/// The same value is produced on static and native paths. No exported "take
/// last outcome" symbol or cross-call thread-local slot is involved: the value
/// itself crosses the native ABI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandOutcome {
    pub exit_code: i32,
    pub error: Option<String>,
    /// Generic, host-interpreted metadata (for example
    /// [`crate::COMMAND_OUTCOME_STATUS_MESSAGE_KEY`]). The SDK attaches no
    /// domain meaning to keys.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub metadata: std::collections::BTreeMap<String, serde_json::Value>,
}
impl From<PluginResult<i32>> for CommandOutcome {
    fn from(result: PluginResult<i32>) -> Self {
        match result {
            Ok(exit_code) => Self {
                exit_code,
                error: None,
                metadata: std::collections::BTreeMap::new(),
            },
            Err(error) => Self {
                exit_code: error.code,
                error: Some(error.message),
                metadata: std::collections::BTreeMap::new(),
            },
        }
    }
}
impl CommandOutcome {
    /// Run one synchronous command body and return its result together with
    /// any metadata it recorded through
    /// [`crate::record_command_outcome_metadata`].
    ///
    /// The capture is bracketed around `body` on the current thread, in the
    /// SDK copy linked into the plugin, so it works identically for
    /// statically bundled and dynamically loaded plugins. Any enclosing
    /// capture is restored afterwards. `body` must not suspend: metadata
    /// recorded after an `.await` on another thread is not observed.
    pub fn capture(body: impl FnOnce() -> PluginResult<i32>) -> Self {
        let (result, metadata) = crate::host_services::capture_command_outcome_metadata(body);
        let mut outcome = Self::from(result);
        outcome.metadata = metadata;
        outcome
    }
}

/// Generated BPDL compatibility evidence, interpreted without rehashing.
///
/// The generator uses canonical interface SHA-256 v2 (including its
/// `bmux_codec` typed-stable/envelope-v1 domain). Empty evidence explicitly
/// means unresolved/ambiguous wire references, not a zero-length digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContractEvidence<'a> {
    Unavailable,
    CanonicalSha256V2(&'a [u8; 32]),
}
impl<'a> ContractEvidence<'a> {
    /// Interpret the generator's existing checked-v2 representation.
    ///
    /// # Errors
    /// Rejects nonempty evidence that is not the supported 32-byte identity.
    pub fn from_generated(evidence: Option<&'a [u8]>) -> PluginResult<Self> {
        match evidence {
            None | Some([]) => Ok(Self::Unavailable),
            Some(bytes) => <&[u8; 32]>::try_from(bytes)
                .map(Self::CanonicalSha256V2)
                .map_err(|_| {
                    PluginCommandError::failed(
                        "unsupported generated contract evidence representation",
                    )
                }),
        }
    }
}

/// Contract declarations are data; generated typed adapters remain Rust-native.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractDescriptor {
    pub interface_id: String,
    pub major: u16,
    pub minor: u16,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginMetadata {
    pub plugin_id: String,
    pub version: String,
    pub roles: Vec<ComponentRole>,
    pub required_capabilities: Vec<String>,
    pub contracts: Vec<ContractDescriptor>,
    /// Optional embedded declaration for existing discovery/install tooling.
    pub manifest_toml: Option<String>,
}

/// Creates scoped implementations. Factory values may be shared; the instances
/// they return must not share accidental process-global product state.
pub trait PluginFactory: Send + Sync + 'static {
    /// # Errors
    /// Returns an error if the bound scope is unavailable, the request is invalid,
    /// or the implementation rejects the operation.
    fn create(&self, context: PluginCreateContext) -> PluginResult<Arc<dyn PluginInstance>>;
}
impl<F> PluginFactory for F
where
    F: Fn(PluginCreateContext) -> PluginResult<Arc<dyn PluginInstance>> + Send + Sync + 'static,
{
    fn create(&self, context: PluginCreateContext) -> PluginResult<Arc<dyn PluginInstance>> {
        self(context)
    }
}

/// One trait implementation supplies lifecycle and portable dispatch. Generated
///
/// BPDL providers expose the same implementation through `register_typed_services`.
/// Methods use shared references so nested calls never need an SDK-held exclusive
/// implementation lock. Implementors synchronize their own short state changes.
pub trait PluginInstance: Send + Sync + 'static {
    fn activate(&self, _context: PluginLifecycleContext) -> PluginFuture<'_, PluginResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn activate_scoped(
        &self,
        mut context: PluginLifecycleContext,
        services: Option<PluginHostServices>,
    ) -> PluginFuture<'_, PluginResult<()>> {
        if context.native.host_service_port.is_none() {
            context.native.host_service_port = services;
        }
        self.activate(context)
    }
    fn deactivate_scoped(
        &self,
        mut context: PluginLifecycleContext,
        services: Option<PluginHostServices>,
    ) -> PluginFuture<'_, PluginResult<()>> {
        // Cleanup receives its fresh lifecycle root, never a closed retained
        // ordinary activation caller or executable data from the wire.
        context.native.host_service_port = services;
        self.deactivate(context)
    }
    /// Standard activation entry for hosts with an owned async task scope.
    fn activate_scoped_async(
        &self,
        mut context: PluginLifecycleContext,
        services: Option<PluginHostServices>,
        runtime: crate::HostAsyncHandle,
    ) -> PluginFuture<'_, PluginResult<()>> {
        if context.native.host_service_port.is_none() {
            context.native.host_service_port = services;
        }
        self.activate_with_async(context, runtime)
    }

    fn activate_with_async(
        &self,
        context: PluginLifecycleContext,
        _runtime: crate::HostAsyncHandle,
    ) -> PluginFuture<'_, PluginResult<()>> {
        let services = context.native.host_service_port.clone();
        self.activate_scoped(context, services)
    }
    fn deactivate(&self, _context: PluginLifecycleContext) -> PluginFuture<'_, PluginResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn command(
        &self,
        _call: PluginCallContext,
        context: NativeCommandContext,
    ) -> PluginFuture<'_, CommandOutcome> {
        Box::pin(async move { Err(PluginCommandError::unknown_command(&context.command)).into() })
    }
    fn invoke(
        &self,
        _call: PluginCallContext,
        _context: NativeServiceContext,
    ) -> PluginFuture<'_, PluginResult<ServiceResponse>> {
        Box::pin(async {
            Err(PluginCommandError::unavailable(
                "service invocation unsupported",
            ))
        })
    }
    fn invoke_streaming(
        &self,
        _call: PluginCallContext,
        _context: NativeStreamingServiceContext,
    ) -> PluginFuture<'_, PluginResult<ServiceResponse>> {
        // Never silently discard the caller's event sink.
        Box::pin(async {
            Err(PluginCommandError::unavailable(
                "streaming invocation unsupported",
            ))
        })
    }
    /// Dispatch an event with actual invocation authority when required.
    fn event_scoped(
        &self,
        _call: PluginCallContext,
        event: PluginEvent,
    ) -> PluginFuture<'_, PluginResult<()>> {
        self.event(event)
    }
    fn event(&self, _event: PluginEvent) -> PluginFuture<'_, PluginResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn extension(
        &self,
        _call: PluginCallContext,
        _contract: String,
        _payload: Vec<u8>,
    ) -> PluginFuture<'_, PluginResult<Vec<u8>>> {
        Box::pin(async { Err(PluginCommandError::unavailable("extension unsupported")) })
    }
    /// # Errors
    /// Returns an error if the bound scope is unavailable, the request is invalid,
    /// or the implementation rejects the operation.
    fn portable_services(&self) -> PluginResult<Vec<Arc<dyn crate::PortableServiceProvider>>> {
        Ok(Vec::new())
    }
    fn service_contract_for(&self, _interface: &str) -> Option<&'static [u8]> {
        None
    }
    /// Context-aware registration is the host entry point. The shorter hook
    /// remains available to providers that require no host ports or inventory.
    /// # Errors
    /// Returns an error if the bound scope is unavailable, the request is invalid,
    /// or the implementation rejects the operation.
    fn register_typed_services_with_context(
        &self,
        _context: crate::TypedServiceRegistrationContext<'_>,
        registry: &mut TypedServiceRegistry,
    ) -> PluginResult<()> {
        self.register_typed_services(registry)
    }
    /// # Errors
    /// Returns an error if the bound scope is unavailable, the request is invalid,
    /// or the implementation rejects the operation.
    fn register_contributions(
        &self,
        _registrar: &mut crate::ContributionRegistrar,
    ) -> PluginResult<()> {
        Ok(())
    }
    /// # Errors
    /// Returns an error if the bound scope is unavailable, the request is invalid,
    /// or the implementation rejects the operation.
    fn registrations(&self) -> PluginResult<InstanceRegistrations> {
        let mut registrar = crate::ContributionRegistrar::new();
        self.register_contributions(&mut registrar)?;
        let contracts = self.registration_contracts()?;
        let mut seen = std::collections::BTreeSet::new();
        for contract in &contracts {
            contract.validate().map_err(PluginCommandError::failed)?;
            if !seen.insert(contract.clone()) {
                return Err(PluginCommandError::failed(
                    "duplicate registration contract",
                ));
            }
        }
        Ok(InstanceRegistrations {
            contributions: registrar.into_contributions(),
            contracts,
        })
    }
    /// Additional versioned registrations (state, readiness and frontend
    /// extensions) discovered on this exact instance before publication.
    /// # Errors
    /// Returns an error if the bound scope is unavailable, the request is invalid,
    /// or the implementation rejects the operation.
    fn registration_contracts(&self) -> PluginResult<Vec<crate::RegistrationContract>> {
        Ok(Vec::new())
    }
    /// Stateful participants are an optional native optimization; portable
    /// orchestration uses their declared typed BPDL contracts.
    /// # Errors
    /// Returns an error if the bound scope is unavailable, the request is invalid,
    /// or the implementation rejects the operation.
    fn stateful_participants(&self) -> PluginResult<Vec<crate::StatefulPluginHandle>> {
        Ok(Vec::new())
    }
    /// Publish generated typed adapters for this exact scoped implementation.
    /// The host must keep the registry activation-owned and revoke it on closure.
    /// # Errors
    /// Returns an error if the bound scope is unavailable, the request is invalid,
    /// or the implementation rejects the operation.
    fn register_typed_services(&self, _registry: &mut TypedServiceRegistry) -> PluginResult<()> {
        Ok(())
    }
}

/// Export-free portable declaration. The embedded manifest is decoded into
///
/// existing SDK contracts; policy/dependency interpretation stays host-owned.
/// Frontend capabilities are generic extension contracts, never core product fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginDeclarations {
    /// Exact generated identities; never inferred from legacy routing aliases.
    #[serde(default)]
    pub modeled_service_contracts: Vec<DeclaredServiceContract>,
    #[serde(default)]
    pub services: Vec<crate::PluginService>,
    #[serde(default)]
    pub commands: Vec<crate::PluginCommand>,
    #[serde(default)]
    pub event_subscriptions: Vec<crate::PluginEventSubscription>,
    #[serde(default)]
    pub event_publications: Vec<crate::PluginEventPublication>,
    #[serde(default)]
    pub ready_signals: Vec<crate::ReadySignalDecl>,
    #[serde(default)]
    pub extensions: Vec<ExtensionDeclaration>,
    /// Why this plugin must ship bundled. Set by plugins that use host
    /// facilities plug-rt does not proxy into dynamic libraries (sockets,
    /// child processes, a private executor). The dynamic loader refuses such
    /// a plugin with a typed error instead of letting it fail at runtime.
    #[serde(default)]
    pub bundled_only: Option<String>,
}
/// Owned portable representation of a generated modeled contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclaredServiceContract {
    pub capability: String,
    pub kind: crate::ServiceKind,
    pub interface_id: String,
    pub contract: Vec<u8>,
}
impl DeclaredServiceContract {
    /// # Errors
    /// Rejects invalid capability/interface identities or unavailable evidence.
    pub fn validate(&self) -> PluginResult<()> {
        crate::HostScope::new(&self.capability)
            .map_err(|error| PluginCommandError::failed(error.to_string()))?;
        if self.interface_id.is_empty()
            || self.interface_id.trim() != self.interface_id
            || self.interface_id.len() > crate::service_contract::MAX_INTERFACE_ID_BYTES
            || self.contract.len() != crate::service_contract::MAX_SERVICE_CONTRACT_BYTES
        {
            return Err(PluginCommandError::failed(
                "invalid modeled service contract",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionDeclaration {
    pub extension_point: String,
    #[serde(default)]
    pub payload: Vec<u8>,
}

/// One registration result for the same instance/activation, portable across
/// packaging modes. Typed services and state participant handles remain native.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstanceRegistrations {
    pub contributions: Vec<crate::PluginContribution>,
    pub contracts: Vec<crate::RegistrationContract>,
}

/// Inert definition shared by static registration and dynamic descriptor adapters.
#[derive(Clone)]
pub struct PluginDefinition {
    pub metadata: PluginMetadata,
    factory: Arc<dyn PluginFactory>,
}
impl PluginDefinition {
    #[must_use]
    pub fn new(metadata: PluginMetadata, factory: impl PluginFactory) -> Self {
        Self {
            metadata,
            factory: Arc::new(factory),
        }
    }
    /// Inert service/command/event/readiness/frontend declarations consumed by
    /// both packaging modes. Missing declaration is empty; corrupt is an error.
    /// # Errors
    /// Returns an error if the bound scope is unavailable, the request is invalid,
    /// or the implementation rejects the operation.
    pub fn declarations(&self) -> PluginResult<PluginDeclarations> {
        let declarations: PluginDeclarations = self.metadata.manifest_toml.as_deref().map_or_else(
            || Ok(PluginDeclarations::default()),
            |manifest| {
                toml::from_str(manifest)
                    .map_err(|error| PluginCommandError::failed(error.to_string()))
            },
        )?;
        let mut identities = std::collections::BTreeSet::new();
        for descriptor in &declarations.modeled_service_contracts {
            descriptor.validate()?;
            if !identities.insert((
                descriptor.capability.clone(),
                descriptor.kind,
                descriptor.interface_id.clone(),
            )) {
                return Err(PluginCommandError::failed(
                    "duplicate modeled service contract",
                ));
            }
        }
        Ok(declarations)
    }
    /// Validate explicit embedded identity against the author metadata.
    ///
    /// Legacy manifests may omit identity fields; when supplied they must agree
    /// exactly.
    /// # Errors
    /// Rejects malformed declarations, duplicate contracts and identity conflicts.
    pub fn validate(&self) -> PluginResult<()> {
        self.declarations()?;
        validate_contracts(&self.metadata.contracts)?;
        if let Some(manifest) = &self.metadata.manifest_toml {
            let value: toml::Value = toml::from_str(manifest)
                .map_err(|error| PluginCommandError::failed(error.to_string()))?;
            if let Some(roles) = value.get("component_roles") {
                let roles: Vec<ComponentRole> =
                    roles.clone().try_into().map_err(|error: toml::de::Error| {
                        PluginCommandError::failed(error.to_string())
                    })?;
                if roles != self.metadata.roles {
                    return Err(PluginCommandError::failed(
                        "embedded component roles conflict with metadata",
                    ));
                }
            }
            if let Some(contracts) = value.get("contracts") {
                let contracts: Vec<ContractDescriptor> =
                    contracts
                        .clone()
                        .try_into()
                        .map_err(|error: toml::de::Error| {
                            PluginCommandError::failed(error.to_string())
                        })?;
                validate_contracts(&contracts)?;
                if contracts != self.metadata.contracts {
                    return Err(PluginCommandError::failed(
                        "embedded contracts conflict with metadata",
                    ));
                }
            }
        }
        Ok(())
    }
    /// # Errors
    /// Returns an error if the bound scope is unavailable, the request is invalid,
    /// or the implementation rejects the operation.
    pub fn create(&self, context: PluginCreateContext) -> PluginResult<Arc<dyn PluginInstance>> {
        self.validate()?;
        if !self.metadata.roles.contains(&context.role) {
            return Err(PluginCommandError::unavailable(
                "component role not provided",
            ));
        }
        self.factory.create(context)
    }
}

fn validate_contracts(contracts: &[ContractDescriptor]) -> PluginResult<()> {
    let mut ids = std::collections::BTreeSet::new();
    for contract in contracts {
        if contract.interface_id.trim().is_empty()
            || contract.major == 0
            || !ids.insert(&contract.interface_id)
        {
            return Err(PluginCommandError::failed(
                "empty, duplicate, or zero-version contract declaration",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Instance;
    impl PluginInstance for Instance {}
    fn metadata() -> PluginMetadata {
        PluginMetadata {
            plugin_id: "example.plugin".into(),
            version: "1".into(),
            roles: vec![ComponentRole::Backend],
            required_capabilities: vec![],
            contracts: vec![],
            manifest_toml: None,
        }
    }
    fn scope() -> PluginCreateContext {
        PluginCreateContext {
            cli_command: None,
            runtime_scope: RuntimeScopeId::from_uuid(uuid::Uuid::from_u128(1)),
            instance: LoadedInstanceId::from_uuid(uuid::Uuid::from_u128(2)),
            role: ComponentRole::Backend,
        }
    }
    #[test]
    fn discovery_is_inert_and_factory_is_scoped() {
        let created = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&created);
        let definition = PluginDefinition::new(metadata(), move |_context| {
            observed.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(Instance) as Arc<dyn PluginInstance>)
        });
        assert_eq!(created.load(Ordering::SeqCst), 0);
        let a = definition.create(scope()).unwrap();
        let b = definition.create(scope()).unwrap();
        assert!(!Arc::ptr_eq(&a, &b));
        assert_eq!(created.load(Ordering::SeqCst), 2);
    }
    #[test]
    fn unsupported_role_never_calls_factory() {
        let definition = PluginDefinition::new(
            metadata(),
            |_context| -> PluginResult<Arc<dyn PluginInstance>> {
                panic!("unsupported role must be rejected before factory")
            },
        );
        let mut context = scope();
        context.role = ComponentRole::Frontend;
        assert!(definition.create(context).is_err());
    }
    #[test]
    fn generated_contract_evidence_preserves_checked_v2_representation() {
        assert_eq!(
            ContractEvidence::from_generated(None).unwrap(),
            ContractEvidence::Unavailable
        );
        assert_eq!(
            ContractEvidence::from_generated(Some(&[])).unwrap(),
            ContractEvidence::Unavailable
        );
        let digest = [23; 32];
        assert_eq!(
            ContractEvidence::from_generated(Some(&digest)).unwrap(),
            ContractEvidence::CanonicalSha256V2(&digest)
        );
        assert!(ContractEvidence::from_generated(Some(&[1; 31])).is_err());
        assert!(ContractEvidence::from_generated(Some(&[1; 33])).is_err());
    }
    #[test]
    fn kernel_grants_are_explicit_bounded_and_identity_compared() {
        let services = PluginHostServices::new(|_| Ok(vec![]));
        assert!(matches!(
            services.execute_kernel_encoded(b"request"),
            Err(crate::PluginError::UnsupportedHostOperation {
                operation: "execute_kernel_encoded"
            })
        ));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = Arc::clone(&calls);
        let bound = services.clone().with_kernel(move |request| {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(request.to_vec())
        });
        assert_eq!(bound, bound.clone());
        assert_ne!(
            bound,
            services.clone().with_kernel(|request| Ok(request.to_vec()))
        );
        assert_ne!(bound, services);
        assert_eq!(
            bound.execute_kernel_encoded(b"request").unwrap(),
            b"request"
        );
        assert!(
            bound
                .execute_kernel_encoded(&vec![0; crate::abi::MAX_BUFFER_BYTES + 1])
                .is_err()
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let oversized = services.with_kernel(|_| Ok(vec![0; crate::abi::MAX_BUFFER_BYTES + 1]));
        assert!(oversized.execute_kernel_encoded(b"request").is_err());
    }
    #[test]
    fn command_error_is_an_owned_result() {
        let outcome = CommandOutcome::from(Err(PluginCommandError::invalid_arguments("bad input")));
        assert_eq!(outcome.exit_code, crate::EXIT_USAGE);
        assert_eq!(outcome.error.as_deref(), Some("bad input"));
        assert!(outcome.metadata.is_empty());
    }
    #[test]
    fn captured_command_metadata_is_part_of_the_returned_outcome() {
        let status = || serde_json::Value::String("ran".into());
        let outcome = CommandOutcome::capture(|| {
            crate::record_command_outcome_metadata(
                crate::COMMAND_OUTCOME_STATUS_MESSAGE_KEY,
                status(),
            );
            Err(PluginCommandError::failed("boom"))
        });
        assert_eq!(outcome.error.as_deref(), Some("boom"));
        assert_eq!(
            outcome
                .metadata
                .get(crate::COMMAND_OUTCOME_STATUS_MESSAGE_KEY),
            Some(&status())
        );
        // Travels as data: the serialized value carries the metadata.
        let decoded: CommandOutcome =
            serde_json::from_slice(&serde_json::to_vec(&outcome).unwrap()).unwrap();
        assert_eq!(decoded, outcome);
        // Nothing lingers for a later command on this thread.
        let next = CommandOutcome::capture(|| Ok(crate::EXIT_OK));
        assert!(next.metadata.is_empty());
        // An enclosing capture is restored and unaffected by the nested one.
        crate::begin_command_outcome_capture();
        let nested = CommandOutcome::capture(|| {
            crate::record_command_outcome_metadata("inner", status());
            Ok(crate::EXIT_OK)
        });
        crate::record_command_outcome_metadata("outer", status());
        let enclosing = crate::finish_command_outcome_capture();
        assert!(nested.metadata.contains_key("inner"));
        assert!(!nested.metadata.contains_key("outer"));
        assert!(enclosing.metadata.contains_key("outer"));
        assert!(!enclosing.metadata.contains_key("inner"));
    }
}
