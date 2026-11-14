//! Typed interface dispatch for plugin-to-plugin calls.
//!
//! BPDL schemas generate typed service traits (e.g. `TabsStateService`)
//! that plugins implement and consume. This module provides the primitives
//! that bridge those typed traits to the untyped [`crate::ServiceRequest`]
//! / [`crate::ServiceResponse`] transport the plugin host already speaks.
//!
//! # Model
//!
//! - The **provider** plugin implements the BPDL-generated service trait
//!   (e.g. `impl TabsStateService for MyPlugin`). During plugin init
//!   the provider registers an `Arc<Self>` as a typed handle via
//!   [`TypedServiceRegistry`].
//! - The **consumer** plugin resolves a typed handle via
//!   [`crate::PluginHost::resolve_service`] and obtains a reference to
//!   the generated `<Iface>Client` wrapper, whose methods call into the
//!   provider's trait directly without serialization.
//!
//! # Serialization fallback
//!
//! When the provider and consumer are not in the same process (or the
//! consumer is a non-Rust SDK), the byte-encoded
//! [`crate::ServiceRequest`] transport is used instead. The typed
//! [`InProcessTypedDispatch`] helpers serialize and deserialize JSON
//! payloads so non-Rust SDKs can interoperate.

use crate::{HostScope, InterfaceId, PluginError, Result, ServiceInterfaceDescriptor, ServiceKind};
use serde::{Serialize, de::DeserializeOwned};
use std::any::{Any, TypeId};
use std::collections::BTreeMap;
use std::sync::Arc;

pub use crate::service::{ServiceStreamContext, TypedServiceEventSink};

/// Exact routing key for an invocation-bound selected-provider availability query.
///
/// The key carries no provider, caller identity or locality assertion. The host
/// supplies those from its bound invocation and selected authoritative registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceAvailabilityKey {
    pub capability: String,
    pub kind: ServiceKind,
    pub interface_id: String,
}

impl ServiceAvailabilityKey {
    /// Validate bounded routing metadata before consulting host state.
    ///
    /// # Errors
    /// Rejects invalid capability syntax or empty, oversized or padded interface IDs.
    pub fn validate(&self) -> Result<()> {
        if self.capability.len() > crate::service_contract::MAX_INTERFACE_ID_BYTES {
            return Err(dispatch_failure(
                "service availability capability exceeds bound",
            ));
        }
        HostScope::new(&self.capability)?;
        if self.interface_id.is_empty()
            || self.interface_id.len() > crate::service_contract::MAX_INTERFACE_ID_BYTES
            || self.interface_id.trim() != self.interface_id
        {
            return Err(dispatch_failure("invalid service availability interface"));
        }
        Ok(())
    }
}

/// Authoritative selected-provider presence, not an invocation authorization.
///
/// Returned only after bound invocation admission and caller access checks.
/// Unsupported negotiation, denied lookup, corrupt registry, unavailable selected
/// provider and failed queries are errors, never `Absent`. Consumers must not
/// cache this observation as authority for later invocation or infer locality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectedServiceAvailability {
    /// The exact tuple resolves to a currently usable selected provider.
    Present,
    /// The authoritative selected registry has no provider for this exact tuple.
    Absent,
}

/// Explicit native capability version for the availability contract.
///
/// Hosts must negotiate this separately from service invocation. Missing support
/// is `UnsupportedHostOperation`; it must never synthesize an `Absent` response.
pub const SELECTED_SERVICE_AVAILABILITY_VERSION: u16 = 1;

/// Declaration-owned generated evidence for one modeled service interface.
///
/// This is Rust metadata, not an ABI layout. Native exporters serialize these
/// fields through the negotiated table. Legacy routing aliases are deliberately
/// absent: an alias cannot manufacture a modeled contract identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeledServiceContractDescriptor {
    pub capability: &'static str,
    pub kind: ServiceKind,
    pub interface_id: &'static str,
    /// Existing canonical SHA-256 v2 evidence; empty means explicitly unavailable.
    pub contract: &'static [u8],
}

impl ModeledServiceContractDescriptor {
    /// Validate evidence before publication or negotiation.
    ///
    /// # Errors
    /// Rejects invalid identifiers and unavailable evidence.
    pub fn validate(&self) -> Result<()> {
        HostScope::new(self.capability)?;
        if self.interface_id.is_empty()
            || self.interface_id.len() > crate::service_contract::MAX_INTERFACE_ID_BYTES
            || self.interface_id.trim() != self.interface_id
            || self.contract.len() != crate::service_contract::MAX_SERVICE_CONTRACT_BYTES
        {
            return Err(dispatch_failure(
                "invalid or unavailable modeled service contract",
            ));
        }
        Ok(())
    }
}

/// Generated portable adapter over the same typed provider used by native clients.
///
/// Host admission/authorization precedes this boundary; payloads never
/// carry caller authority. Only checked v2 frames are admitted by this adapter.
pub trait PortableServiceProvider: Send + Sync {
    fn descriptor(&self) -> ServiceInterfaceDescriptor;

    /// Operation-level negotiation. Implementations must never claim support
    /// unless checked dispatch preserves events, cancellation and terminal failure.
    fn supports_streaming(&self, _operation: &str) -> bool {
        false
    }

    /// Decode, invoke once, then encode the terminal response. A missing stream
    /// context for an emitting operation is rejected before invoking the provider.
    fn dispatch_checked<'a>(
        &'a self,
        operation: &'a str,
        payload: &'a [u8],
        stream: Option<ServiceStreamContext>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>>> + Send + 'a>>;
}

/// Invoke a legacy body handler at an explicitly checked service boundary.
///
/// Generated portable providers already own checked framing and must not be
/// passed as the handler. Raw legacy routing is a separate explicit path; this
/// helper never accepts unchecked bytes or retries after provider effects.
/// Host authorization and live invocation admission precede this boundary.
///
/// # Errors
/// Rejects missing checked framing, mismatched routes, missing generated
/// evidence or incompatible contracts before calling the handler.
pub fn invoke_checked_service_body<C: crate::PluginContract>(
    mut context: crate::NativeServiceContext,
    body_handler: impl FnOnce(crate::NativeServiceContext) -> crate::ServiceResponse,
) -> Result<crate::ServiceResponse> {
    let interface = crate::service_contract::interface_id_for(
        &context.request.service.interface_id,
        &context.request.payload,
    )?;
    let evidence = C::service_contract_for(interface).ok_or_else(|| {
        dispatch_failure("provider has no generated contract for checked interface")
    })?;
    context.request.payload = crate::service_contract::decode_for(
        &context.request.service.interface_id,
        evidence,
        &context.request.payload,
    )?
    .to_vec();
    let mut response = body_handler(context);
    if response.error.is_none() {
        response.payload = crate::service_contract::encode_response(&response.payload);
    }
    Ok(response)
}

/// A negotiated provider target.
///
/// Hosts select this only after lifecycle admission, authoritative authorization
/// and caller/deadline inheritance. Static targets retain the native typed fast
/// path; portable targets never expose Rust layout.
#[derive(Clone, Copy)]
pub enum ServiceDispatchTarget<'a> {
    Static(&'a TypedServiceHandle),
    Portable(&'a dyn PortableServiceProvider),
}

/// Shared generated dispatch helper.
///
/// Both paths enforce identical descriptor and stream-shape preconditions before
/// invoking the implementation exactly once. No retry or fallback is performed
/// after effects begin.
///
/// # Errors
/// Rejects missing evidence, unsupported streams, type mismatch, cancellation,
/// delivery failure and checked response/codec failures.
pub async fn dispatch_service<E, S, F, Fut>(
    target: ServiceDispatchTarget<'_>,
    request: E::Request,
    stream: Option<ServiceStreamContext>,
    direct: F,
) -> Result<E::Response>
where
    E: crate::TypedServiceEndpoint,
    S: ?Sized + Send + Sync + 'static,
    F: FnOnce(Arc<S>, E::Request, Option<ServiceStreamContext>) -> Fut,
    Fut: std::future::Future<Output = Result<E::Response>>,
{
    let expected = E::service_descriptor();
    let descriptor = match target {
        ServiceDispatchTarget::Static(handle) => handle.descriptor_contract(),
        ServiceDispatchTarget::Portable(provider) => Some(provider.descriptor()),
    }
    .ok_or_else(|| dispatch_failure("provider has no checked contract evidence"))?;
    if descriptor != expected || descriptor.contract.len() != 32 {
        return Err(dispatch_failure(
            "provider descriptor or exact contract mismatch",
        ));
    }
    if E::STREAMING != stream.is_some() {
        return Err(dispatch_failure(
            "operation stream context does not match declaration",
        ));
    }
    if let ServiceDispatchTarget::Portable(provider) = target
        && E::STREAMING
        && !provider.supports_streaming(E::OPERATION.as_str())
    {
        return Err(dispatch_failure(
            "portable provider has not negotiated streaming",
        ));
    }
    let (stream, completion) = if let Some(stream) = stream {
        if stream.cancellation.is_cancelled() {
            return Err(dispatch_failure("service stream cancelled"));
        }
        let (stream, completion) = stream.scoped();
        (Some(stream), Some(completion))
    } else {
        (None, None)
    };
    let cancellation = stream.as_ref().map(|stream| stream.cancellation.clone());
    let response = match target {
        ServiceDispatchTarget::Static(handle) => {
            let provider = handle.provider_as_trait::<S>()?;
            direct(provider, request, stream).await?
        }
        ServiceDispatchTarget::Portable(provider) => {
            let payload = crate::encode_service_message(&request)?;
            let payload = crate::service_contract::encode(
                E::INTERFACE_ID.as_str(),
                E::SERVICE_CONTRACT,
                &payload,
            )?;
            let response =
                dispatch_portable_checked(provider, E::OPERATION.as_str(), &payload, stream)
                    .await?;
            crate::decode_service_message(crate::service_contract::decode_response(&response)?)?
        }
    };
    if let Some(completion) = completion {
        completion.finish()?;
    }
    if cancellation.is_some_and(|cancellation| cancellation.is_cancelled()) {
        return Err(dispatch_failure("service stream cancelled"));
    }
    Ok(response)
}

/// Dispatch an already checked byte request through a negotiated provider.
///
/// This is the common M1/M3 entry point for raw invocation paths; native typed
/// callers use `dispatch_service` instead. Checked evidence is validated before effects.
///
/// # Errors
/// Rejects incompatible evidence, stream negotiation, cancellation and delivery
/// failures. Never retries provider work or substitutes a unary invocation.
pub async fn dispatch_portable_checked(
    provider: &dyn PortableServiceProvider,
    operation: &str,
    payload: &[u8],
    stream: Option<ServiceStreamContext>,
) -> Result<Vec<u8>> {
    let descriptor = provider.descriptor();
    crate::service_contract::decode_for(
        descriptor.interface_id.as_str(),
        descriptor.contract,
        payload,
    )?;
    if provider.supports_streaming(operation) != stream.is_some() {
        return Err(dispatch_failure(
            "operation stream context does not match negotiation",
        ));
    }
    let (stream, completion) = if let Some(stream) = stream {
        if stream.cancellation.is_cancelled() {
            return Err(dispatch_failure("service stream cancelled"));
        }
        let (stream, completion) = stream.scoped();
        (Some(stream), Some(completion))
    } else {
        (None, None)
    };
    let cancellation = stream.as_ref().map(|stream| stream.cancellation.clone());
    let response = provider
        .dispatch_checked(operation, payload, stream)
        .await?;
    crate::service_contract::decode_response(&response)?;
    if let Some(completion) = completion {
        completion.finish()?;
    }
    if cancellation.is_some_and(|cancellation| cancellation.is_cancelled()) {
        return Err(dispatch_failure("service stream cancelled"));
    }
    Ok(response)
}

fn dispatch_failure(details: &str) -> PluginError {
    PluginError::ServiceProtocol {
        details: details.to_string(),
    }
}

/// Errors returned by typed-dispatch operations that are distinct from
/// the plugin error domain (which is focused on capability gating and
/// registry lookup).
#[derive(Debug, thiserror::Error)]
pub enum TypedDispatchError {
    /// Serialization of typed parameters failed.
    #[error("failed to serialize typed parameters: {0}")]
    Serialize(String),
    /// Deserialization of a typed response failed.
    #[error("failed to deserialize typed response: {0}")]
    Deserialize(String),
    /// The requested typed handle was not of the expected Rust type.
    /// This happens when a plugin registered a provider under a given
    /// interface id but the type doesn't match what the consumer
    /// requested.
    #[error("typed dispatch mismatch: requested {requested}, registered {registered}")]
    TypeMismatch {
        requested: &'static str,
        registered: &'static str,
    },
}

/// Concrete cell holding an `Arc<S>` where `S` may be an unsized trait object.
///
/// `Any` only works on sized types, so providers cannot be downcast directly
/// from `Arc<dyn Any>` to `Arc<dyn SomeTrait>`. The cell sidesteps that by
/// being a sized, concrete `Any` type that carries the `Arc<S>` inside.
///
/// Generated client wrappers resolve a typed handle by asking for a
/// `TypedProviderCell<dyn SomeService + Send + Sync>` and extracting
/// the inner `Arc` for their trait object.
pub struct TypedProviderCell<S: ?Sized + 'static> {
    provider: Arc<S>,
}

impl<S: ?Sized + 'static> TypedProviderCell<S> {
    /// Construct a new cell wrapping `provider`.
    #[must_use]
    pub const fn new(provider: Arc<S>) -> Self {
        Self { provider }
    }

    /// Borrow the inner `Arc<S>`.
    #[must_use]
    pub const fn inner(&self) -> &Arc<S> {
        &self.provider
    }

    /// Consume the cell and return the inner `Arc<S>`.
    #[must_use]
    pub fn into_inner(self) -> Arc<S> {
        self.provider
    }
}

/// A typed reference to a provider plugin's interface impl.
///
/// Internally the handle stores a type-erased `Arc<dyn Any + Send + Sync>`
/// whose concrete type is a [`TypedProviderCell<S>`]. Generated client
/// wrappers use [`Self::provider_as_trait`] to recover `Arc<S>`.
pub struct TypedServiceHandle {
    capability: HostScope,
    interface_id: InterfaceId,
    kind: ServiceKind,
    provider: Arc<dyn Any + Send + Sync>,
    provider_type: TypeId,
    provider_type_name: &'static str,
    portable: Option<Arc<dyn PortableServiceProvider>>,
}

impl std::fmt::Debug for TypedServiceHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TypedServiceHandle")
            .field("capability", &self.capability)
            .field("interface_id", &self.interface_id)
            .field("kind", &self.kind)
            .field("provider_type_name", &self.provider_type_name)
            .finish_non_exhaustive()
    }
}

impl TypedServiceHandle {
    /// Construct a typed handle from an `Arc` whose concrete type is
    /// already `Any`-compatible (i.e. `Sized`).
    ///
    /// Prefer [`Self::new_typed`] when the provider is addressed via a
    /// trait object.
    #[must_use]
    pub fn new(
        capability: HostScope,
        interface_id: impl Into<InterfaceId>,
        kind: ServiceKind,
        provider: Arc<dyn Any + Send + Sync>,
    ) -> Self {
        let provider_type = (*provider).type_id();
        Self {
            capability,
            interface_id: interface_id.into(),
            kind,
            provider,
            provider_type,
            provider_type_name: "<Arc<dyn Any>>",
            portable: None,
        }
    }

    /// Construct a typed handle from an `Arc<S>` where `S` may be a
    /// trait object (`dyn Trait + Send + Sync`).
    ///
    /// The handle stores the `Arc<S>` inside a [`TypedProviderCell<S>`]
    /// so callers can later retrieve it via
    /// [`Self::provider_as_trait::<S>()`].
    #[must_use]
    pub fn new_typed<S: ?Sized + Send + Sync + 'static>(
        capability: HostScope,
        interface_id: impl Into<InterfaceId>,
        kind: ServiceKind,
        provider: Arc<S>,
    ) -> Self {
        let cell: Arc<TypedProviderCell<S>> = Arc::new(TypedProviderCell::new(provider));
        let erased: Arc<dyn Any + Send + Sync> = cell;
        Self {
            capability,
            interface_id: interface_id.into(),
            kind,
            provider: erased,
            provider_type: TypeId::of::<TypedProviderCell<S>>(),
            provider_type_name: std::any::type_name::<TypedProviderCell<S>>(),
            portable: None,
        }
    }

    /// Checked adapter registered atomically with this typed provider, when
    /// generated by BPDL. Host-side admission is still required on either path.
    #[must_use]
    pub fn descriptor_contract(&self) -> Option<ServiceInterfaceDescriptor> {
        self.portable.as_ref().map(|provider| provider.descriptor())
    }

    #[must_use]
    pub fn portable_provider(&self) -> Option<Arc<dyn PortableServiceProvider>> {
        self.portable.clone()
    }

    #[must_use]
    pub const fn capability(&self) -> &HostScope {
        &self.capability
    }

    #[must_use]
    pub const fn interface_id(&self) -> &InterfaceId {
        &self.interface_id
    }

    #[must_use]
    pub const fn kind(&self) -> ServiceKind {
        self.kind
    }

    /// Downcast the provider to a concrete sized type. Callers that
    /// need a trait object should use [`Self::provider_as_trait`] instead.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::ServiceProtocol`] if the registered
    /// provider cannot be downcast to `T`.
    pub fn provider_as<T: Any + Send + Sync>(&self) -> Result<Arc<T>> {
        Arc::clone(&self.provider)
            .downcast::<T>()
            .map_err(|_| PluginError::ServiceProtocol {
                details: format!(
                    "typed dispatch type mismatch for interface '{}'",
                    self.interface_id
                ),
            })
    }

    /// Recover the inner `Arc<S>` when the provider was registered via
    /// [`Self::new_typed::<S>`]. `S` may be a trait object.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::ServiceProtocol`] if the stored cell's
    /// inner type does not match the requested `S`.
    pub fn provider_as_trait<S: ?Sized + Send + Sync + 'static>(&self) -> Result<Arc<S>> {
        if self.provider_type != TypeId::of::<TypedProviderCell<S>>() {
            return Err(PluginError::ServiceProtocol {
                details: format!(
                    "typed dispatch type mismatch for interface '{}': requested {}, registered {}",
                    self.interface_id,
                    std::any::type_name::<TypedProviderCell<S>>(),
                    self.provider_type_name,
                ),
            });
        }
        let cell: Arc<TypedProviderCell<S>> = Arc::clone(&self.provider)
            .downcast::<TypedProviderCell<S>>()
            .map_err(|_| PluginError::ServiceProtocol {
                details: format!(
                    "typed dispatch downcast failed for interface '{}'",
                    self.interface_id
                ),
            })?;
        Ok(Arc::clone(cell.inner()))
    }
}

/// Key used to uniquely identify a typed service entry.
pub type TypedServiceKey = (HostScope, ServiceKind, InterfaceId);

/// Collection of typed service handles a plugin provides.
///
/// Plugins populate the registry during `register_typed_services`; the
/// host merges all registries into a lookup map keyed by
/// `(capability, kind, interface_id)`.
#[derive(Default)]
pub struct TypedServiceRegistry {
    entries: BTreeMap<TypedServiceKey, TypedServiceHandle>,
}

impl TypedServiceRegistry {
    /// Construct an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a typed service handle. Replaces any existing entry under
    /// the same key.
    pub fn insert(&mut self, handle: TypedServiceHandle) {
        let key = (
            handle.capability.clone(),
            handle.kind,
            handle.interface_id.clone(),
        );
        self.entries.insert(key, handle);
    }

    /// Build a typed handle from an `Arc<S>` (where `S` may be a trait
    /// object) and insert it into the registry.
    pub fn insert_typed<S: ?Sized + Send + Sync + 'static>(
        &mut self,
        capability: HostScope,
        kind: ServiceKind,
        interface_id: impl Into<InterfaceId>,
        provider: Arc<S>,
    ) {
        self.insert(TypedServiceHandle::new_typed(
            capability,
            interface_id,
            kind,
            provider,
        ));
    }

    /// Insert a typed service handle from a BPDL-generated descriptor.
    ///
    /// # Errors
    ///
    /// Returns if the descriptor's capability cannot be represented as a host
    /// scope. Generated descriptors should always be valid.
    pub fn insert_typed_descriptor<S: ?Sized + Send + Sync + 'static>(
        &mut self,
        descriptor: ServiceInterfaceDescriptor,
        provider: Arc<S>,
    ) -> Result<()> {
        let service = descriptor.to_plugin_service()?;
        self.insert_typed(
            service.capability,
            service.kind,
            descriptor.interface_id,
            provider,
        );
        Ok(())
    }

    /// Atomically register both native and checked adapters under one descriptor.
    ///
    /// # Errors
    /// Rejects mismatched adapter evidence before changing the registry.
    pub fn insert_typed_provider<S: ?Sized + Send + Sync + 'static>(
        &mut self,
        descriptor: ServiceInterfaceDescriptor,
        provider: Arc<S>,
        portable: Arc<dyn PortableServiceProvider>,
    ) -> Result<()> {
        let actual = portable.descriptor();
        if actual.capability != descriptor.capability
            || actual.kind != descriptor.kind
            || actual.interface_id != descriptor.interface_id
            || actual.contract != descriptor.contract
        {
            return Err(PluginError::ServiceProtocol {
                details: "portable provider descriptor does not match typed provider".to_string(),
            });
        }
        let service = descriptor.to_plugin_service()?;
        let mut handle = TypedServiceHandle::new_typed(
            service.capability,
            descriptor.interface_id,
            service.kind,
            provider,
        );
        handle.portable = Some(portable);
        self.insert(handle);
        Ok(())
    }

    /// Generated portable adapters from the same registration snapshot as native
    /// handles. M1 hosts use this instead of requiring duplicate provider lists.
    #[must_use]
    pub fn portable_providers(&self) -> Vec<Arc<dyn PortableServiceProvider>> {
        self.entries
            .values()
            .filter_map(TypedServiceHandle::portable_provider)
            .collect()
    }

    /// Exact interface evidence from this admitted provider registry. Ambiguous
    /// interface identities fail closed rather than depending on map order.
    #[must_use]
    pub fn service_contract_for(&self, interface: &str) -> Option<&'static [u8]> {
        let mut contract = None;
        for handle in self
            .entries
            .values()
            .filter(|handle| handle.interface_id().as_str() == interface)
        {
            let descriptor = handle.descriptor_contract()?;
            if descriptor.contract.len() != 32 {
                return None;
            }
            if contract.is_some_and(|previous| previous != descriptor.contract) {
                return None;
            }
            contract = Some(descriptor.contract);
        }
        contract
    }

    /// Route a raw checked invocation using this exact registration snapshot.
    /// M1/M3 must perform host lifecycle/authorization admission before calling.
    /// The payload is still checked-v2 framed; the returned bytes retain provider
    /// acceptance, so callers must not add or remove framing a second time.
    ///
    /// # Errors
    /// Rejects missing providers/evidence, negotiation and invocation failures.
    pub async fn dispatch_checked(
        &self,
        request: &crate::ServiceRequest,
        stream: Option<ServiceStreamContext>,
    ) -> Result<Vec<u8>> {
        let provider = self
            .get(
                &request.service.capability,
                request.service.kind,
                &request.service.interface_id,
            )
            .and_then(TypedServiceHandle::portable_provider)
            .ok_or_else(|| dispatch_failure("checked provider is not registered"))?;
        dispatch_portable_checked(
            provider.as_ref(),
            &request.operation,
            &request.payload,
            stream,
        )
        .await
    }

    /// Return the number of registered handles.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the registry has any entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterate over every registered typed handle.
    pub fn iter(&self) -> impl Iterator<Item = (&TypedServiceKey, &TypedServiceHandle)> {
        self.entries.iter()
    }

    /// Look up a handle by key.
    #[must_use]
    pub fn get(
        &self,
        capability: &HostScope,
        kind: ServiceKind,
        interface_id: &str,
    ) -> Option<&TypedServiceHandle> {
        self.entries.get(&(
            capability.clone(),
            kind,
            InterfaceId::from_owned(interface_id.to_string()),
        ))
    }

    /// Consume the registry and return the underlying map.
    #[must_use]
    pub fn into_entries(self) -> BTreeMap<TypedServiceKey, TypedServiceHandle> {
        self.entries
    }
}

/// Helpers for round-tripping typed calls across serialized transport.
///
/// Used by generated client stubs when the provider is not in-process
/// (non-Rust SDK, separate process, etc.).
pub struct InProcessTypedDispatch;

impl InProcessTypedDispatch {
    /// Serialize typed arguments for cross-process transport.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::ServiceProtocol`] if the typed arguments
    /// cannot be JSON-encoded.
    pub fn encode_args<T: Serialize>(args: &T) -> Result<Vec<u8>> {
        serde_json::to_vec(args).map_err(|err| PluginError::ServiceProtocol {
            details: format!("typed dispatch serialize: {err}"),
        })
    }

    /// Deserialize a typed response from cross-process transport.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::ServiceProtocol`] if the bytes cannot be
    /// JSON-decoded into `T`.
    pub fn decode_response<T: DeserializeOwned>(payload: &[u8]) -> Result<T> {
        serde_json::from_slice(payload).map_err(|err| PluginError::ServiceProtocol {
            details: format!("typed dispatch deserialize: {err}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dummy {
        value: u32,
    }

    trait Greeter: Send + Sync {
        fn greet(&self) -> String;
    }

    struct Hello;
    impl Greeter for Hello {
        fn greet(&self) -> String {
            "hi".into()
        }
    }

    struct StreamingEndpoint;
    impl crate::TypedServiceEndpoint for StreamingEndpoint {
        type Request = u32;
        type Response = u32;
        const CAPABILITY: crate::CapabilityId = crate::CapabilityId::from_static("example.stream");
        const KIND: bmux_ipc::InvokeServiceKind = bmux_ipc::InvokeServiceKind::Command;
        const INTERFACE_ID: InterfaceId = InterfaceId::from_static("stream/v2");
        const OPERATION: crate::OperationId = crate::OperationId::from_static("run");
        const SERVICE_CONTRACT: &'static [u8] = &[5; 32];
        const STREAMING: bool = true;
    }

    struct StreamProvider;
    impl StreamProvider {
        fn run(value: u32, stream: &ServiceStreamContext) -> Result<u32> {
            stream.typed_sink::<u32>().emit(value)?;
            Ok(value + 1)
        }
    }
    impl PortableServiceProvider for StreamProvider {
        fn descriptor(&self) -> ServiceInterfaceDescriptor {
            <StreamingEndpoint as crate::TypedServiceEndpoint>::service_descriptor()
        }
        fn supports_streaming(&self, operation: &str) -> bool {
            operation == "run"
        }
        fn dispatch_checked<'a>(
            &'a self,
            _: &'a str,
            payload: &'a [u8],
            stream: Option<ServiceStreamContext>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>>> + Send + 'a>>
        {
            Box::pin(async move {
                let payload = crate::service_contract::decode_for("stream/v2", &[5; 32], payload)?;
                let value = crate::decode_service_message(payload)?;
                let response = Self::run(
                    value,
                    &stream.ok_or_else(|| dispatch_failure("missing stream"))?,
                )?;
                Ok(crate::service_contract::encode_response(
                    &crate::encode_service_message(&response)?,
                ))
            })
        }
    }

    #[tokio::test]
    async fn static_and_portable_selection_share_stream_and_terminal_semantics() {
        let mut registry = TypedServiceRegistry::new();
        let provider = Arc::new(StreamProvider);
        registry
            .insert_typed_provider(
                <StreamingEndpoint as crate::TypedServiceEndpoint>::service_descriptor(),
                provider.clone(),
                provider.clone(),
            )
            .unwrap();
        let capability = HostScope::new("example.stream").unwrap();
        let handle = registry
            .get(&capability, ServiceKind::Command, "stream/v2")
            .unwrap();
        assert_eq!(
            registry.service_contract_for("stream/v2"),
            Some(&[5; 32][..])
        );
        assert_eq!(registry.portable_providers().len(), 1);
        for target in [
            ServiceDispatchTarget::Static(handle),
            ServiceDispatchTarget::Portable(provider.as_ref()),
        ] {
            for capacity in [0, 1] {
                let events = crate::BoundedServiceEventSink::new(capacity);
                let stream = ServiceStreamContext {
                    invocation_id: crate::PluginInvocationId::new(),
                    cancellation: crate::ServiceCancellation::new(),
                    events: crate::ServiceEventSinkHandle::new(events.clone()),
                };
                let result = dispatch_service::<StreamingEndpoint, StreamProvider, _, _>(
                    target,
                    41,
                    Some(stream),
                    |_provider, request, stream| async move {
                        StreamProvider::run(request, &stream.unwrap())
                    },
                )
                .await;
                if capacity == 0 {
                    assert!(result.is_err());
                } else {
                    assert_eq!(result.unwrap(), 42);
                    let events = events.drain().unwrap();
                    assert_eq!(events.len(), 1);
                    assert_eq!(
                        crate::decode_service_message::<u32>(&events[0].payload).unwrap(),
                        41
                    );
                }
            }
            let cancellation = crate::ServiceCancellation::new();
            cancellation.cancel();
            let stream = ServiceStreamContext {
                invocation_id: crate::PluginInvocationId::new(),
                cancellation,
                events: crate::ServiceEventSinkHandle::new(crate::BoundedServiceEventSink::new(1)),
            };
            assert!(
                dispatch_service::<StreamingEndpoint, StreamProvider, _, _>(
                    target,
                    1,
                    Some(stream),
                    |_, _, _| async { panic!("cancelled direct call") },
                )
                .await
                .is_err()
            );
        }
    }

    struct BodyContract;
    impl crate::PluginContract for BodyContract {
        fn service_contract_for(interface: &str) -> Option<&'static [u8]> {
            (interface == "body/v1").then_some(&[3; 32][..])
        }
        fn service_declarations() -> Result<Vec<crate::PluginService>> {
            Ok(Vec::new())
        }
    }

    fn checked_body_context(payload: Vec<u8>) -> crate::NativeServiceContext {
        crate::NativeServiceContext {
            plugin_id: "example.body".into(),
            request: crate::ServiceRequest {
                caller_plugin_id: "example.caller".into(),
                service: crate::RegisteredService {
                    capability: HostScope::new("example.read").unwrap(),
                    kind: ServiceKind::Query,
                    interface_id: "body/v1".into(),
                    provider: crate::ProviderId::Plugin("example.body".into()),
                },
                operation: "read".into(),
                payload,
            },
            required_capabilities: vec![],
            provided_capabilities: vec![],
            services: vec![],
            available_capabilities: vec![],
            enabled_plugins: vec![],
            plugin_search_roots: vec![],
            host: crate::HostMetadata {
                product_name: "test".into(),
                product_version: "1".into(),
                plugin_api_version: crate::ApiVersion::new(1, 0),
                plugin_abi_version: crate::ApiVersion::new(2, 0),
            },
            connection: crate::HostConnectionInfo {
                config_dir: String::new(),
                config_dir_candidates: vec![],
                runtime_dir: String::new(),
                data_dir: String::new(),
                state_dir: String::new(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            caller_client_id: None,
            cancellation: crate::CancellationToken::new(),
            host_kernel_bridge: None,
            host_service_port: None,
        }
    }

    #[test]
    fn checked_body_boundary_validates_before_exactly_one_effect_and_one_response_frame() {
        let body = crate::encode_service_message(&41_u32).unwrap();
        let checked = crate::service_contract::encode("body/v1", &[3; 32], &body).unwrap();
        let calls = std::cell::Cell::new(0);
        let response =
            invoke_checked_service_body::<BodyContract>(checked_body_context(checked), |context| {
                calls.set(calls.get() + 1);
                let value: u32 = crate::decode_service_message(&context.request.payload).unwrap();
                crate::ServiceResponse::ok(crate::encode_service_message(&(value + 1)).unwrap())
            })
            .unwrap();
        assert_eq!(calls.get(), 1);
        let response = crate::service_contract::decode_response(&response.payload).unwrap();
        assert_eq!(crate::decode_service_message::<u32>(response).unwrap(), 42);
        assert!(crate::service_contract::decode_response(response).is_err());
        for invalid in [
            body,
            crate::service_contract::encode("body/v1", &[4; 32], b"bad").unwrap(),
            crate::service_contract::encode("other/v1", &[3; 32], b"bad").unwrap(),
        ] {
            assert!(
                invoke_checked_service_body::<BodyContract>(checked_body_context(invalid), |_| {
                    calls.set(calls.get() + 1);
                    crate::ServiceResponse::ok(vec![])
                })
                .is_err()
            );
        }
        assert_eq!(calls.get(), 1);
        let checked = crate::service_contract::encode("body/v1", &[3; 32], b"body").unwrap();
        let response =
            invoke_checked_service_body::<BodyContract>(checked_body_context(checked), |_| {
                crate::ServiceResponse::error("denied", "not authorized")
            })
            .unwrap();
        assert!(response.error.is_some());
        assert!(crate::service_contract::decode_response(&response.payload).is_err());
    }

    #[test]
    fn service_availability_metadata_is_exact_bounded_and_has_no_default_absence() {
        let key = ServiceAvailabilityKey {
            capability: "example.read".into(),
            kind: ServiceKind::Query,
            interface_id: "example-state/v2".into(),
        };
        key.validate().unwrap();
        let bytes = crate::encode_service_message(&key).unwrap();
        assert_eq!(
            crate::decode_service_message::<ServiceAvailabilityKey>(&bytes).unwrap(),
            key
        );
        for interface_id in [String::new(), " example-state/v2".into(), "x".repeat(1025)] {
            assert!(
                ServiceAvailabilityKey {
                    interface_id,
                    ..key.clone()
                }
                .validate()
                .is_err()
            );
        }
        assert!(
            ServiceAvailabilityKey {
                capability: "x".repeat(1025),
                ..key
            }
            .validate()
            .is_err()
        );
        for presence in [
            SelectedServiceAvailability::Present,
            SelectedServiceAvailability::Absent,
        ] {
            let bytes = crate::encode_service_message(&presence).unwrap();
            assert_eq!(
                crate::decode_service_message::<SelectedServiceAvailability>(&bytes).unwrap(),
                presence
            );
        }
        assert!(serde_json::from_str::<SelectedServiceAvailability>("\"unsupported\"").is_err());
        assert!(serde_json::from_str::<SelectedServiceAvailability>("null").is_err());
        assert!(serde_json::from_str::<ServiceAvailabilityKey>(r#"{"capability":"example.read","kind":"query","interface_id":"example-state/v2","local":true}"#).is_err());
    }

    #[test]
    fn modeled_evidence_validation_never_substitutes_missing_or_legacy_identity() {
        let valid = ModeledServiceContractDescriptor {
            capability: "example.read",
            kind: ServiceKind::Query,
            interface_id: "state/v2",
            contract: &[7; 32],
        };
        valid.validate().unwrap();
        for contract in [&[][..], &[7; 31][..], &[7; 33][..]] {
            assert!(
                ModeledServiceContractDescriptor { contract, ..valid }
                    .validate()
                    .is_err()
            );
        }
        assert!(
            ModeledServiceContractDescriptor {
                interface_id: "",
                ..valid
            }
            .validate()
            .is_err()
        );
        assert!(
            ModeledServiceContractDescriptor {
                interface_id: " state/v2",
                ..valid
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn typed_service_handle_downcasts_correctly() {
        let provider: Arc<dyn Any + Send + Sync> = Arc::new(Dummy { value: 7 });
        let handle = TypedServiceHandle::new(
            HostScope::new("bmux.example").expect("capability id"),
            "example-iface",
            ServiceKind::Query,
            provider,
        );
        let arc = handle.provider_as::<Dummy>().expect("downcast");
        assert_eq!(arc.value, 7);
    }

    #[test]
    fn typed_service_handle_rejects_wrong_type() {
        let provider: Arc<dyn Any + Send + Sync> = Arc::new(Dummy { value: 7 });
        let handle = TypedServiceHandle::new(
            HostScope::new("bmux.example").expect("capability id"),
            "example-iface",
            ServiceKind::Query,
            provider,
        );
        assert!(handle.provider_as::<u64>().is_err());
    }

    #[test]
    fn new_typed_round_trips_trait_object() {
        let hello: Arc<dyn Greeter + Send + Sync> = Arc::new(Hello);
        let handle = TypedServiceHandle::new_typed::<dyn Greeter + Send + Sync>(
            HostScope::new("bmux.example").expect("cap"),
            "greeter-iface",
            ServiceKind::Query,
            hello,
        );
        let recovered = handle
            .provider_as_trait::<dyn Greeter + Send + Sync>()
            .expect("round-trip");
        assert_eq!(recovered.greet(), "hi");
    }

    #[test]
    fn provider_as_trait_rejects_wrong_trait() {
        #[allow(dead_code)]
        trait Shouter: Send + Sync {
            fn shout(&self) -> String;
        }

        let hello: Arc<dyn Greeter + Send + Sync> = Arc::new(Hello);
        let handle = TypedServiceHandle::new_typed::<dyn Greeter + Send + Sync>(
            HostScope::new("bmux.example").expect("cap"),
            "greeter-iface",
            ServiceKind::Query,
            hello,
        );
        assert!(
            handle
                .provider_as_trait::<dyn Shouter + Send + Sync>()
                .is_err()
        );
    }

    #[test]
    fn registry_insert_and_lookup_by_key() {
        let hello: Arc<dyn Greeter + Send + Sync> = Arc::new(Hello);
        let cap = HostScope::new("bmux.example").expect("cap");
        let mut registry = TypedServiceRegistry::new();
        registry.insert_typed::<dyn Greeter + Send + Sync>(
            cap.clone(),
            ServiceKind::Query,
            "greeter-iface",
            hello,
        );
        assert_eq!(registry.len(), 1);
        let handle = registry
            .get(&cap, ServiceKind::Query, "greeter-iface")
            .expect("lookup");
        let greeter = handle
            .provider_as_trait::<dyn Greeter + Send + Sync>()
            .expect("trait recover");
        assert_eq!(greeter.greet(), "hi");
    }

    #[test]
    fn inprocess_encode_decode_round_trip() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Params {
            id: u64,
            label: String,
        }
        let p = Params {
            id: 42,
            label: "pane".into(),
        };
        let bytes = InProcessTypedDispatch::encode_args(&p).expect("encode");
        let round: Params = InProcessTypedDispatch::decode_response(&bytes).expect("decode");
        assert_eq!(round, p);
    }
}
