//! Typed interface dispatch for plugin-to-plugin calls.
//!
//! BPDL schemas generate typed service traits (e.g. `WindowsStateService`)
//! that plugins implement and consume. This module provides the primitives
//! that bridge those typed traits to the untyped [`crate::ServiceRequest`]
//! / [`crate::ServiceResponse`] transport the plugin host already speaks.
//!
//! # Model
//!
//! - The **provider** plugin implements the BPDL-generated service trait
//!   (e.g. `impl WindowsStateService for MyPlugin`). During plugin init
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

use crate::{HostScope, PluginError, Result, ServiceKind};
use serde::{Serialize, de::DeserializeOwned};
use std::any::{Any, TypeId};
use std::collections::BTreeMap;
use std::sync::Arc;

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
    interface_id: String,
    kind: ServiceKind,
    provider: Arc<dyn Any + Send + Sync>,
    provider_type: TypeId,
    provider_type_name: &'static str,
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
        interface_id: impl Into<String>,
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
        interface_id: impl Into<String>,
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
        }
    }

    #[must_use]
    pub const fn capability(&self) -> &HostScope {
        &self.capability
    }

    #[must_use]
    pub fn interface_id(&self) -> &str {
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
pub type TypedServiceKey = (HostScope, ServiceKind, String);

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
        interface_id: impl Into<String>,
        provider: Arc<S>,
    ) {
        self.insert(TypedServiceHandle::new_typed(
            capability,
            interface_id,
            kind,
            provider,
        ));
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
        self.entries
            .get(&(capability.clone(), kind, interface_id.to_string()))
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
