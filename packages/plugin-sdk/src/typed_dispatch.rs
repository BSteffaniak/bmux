//! Typed interface dispatch for plugin-to-plugin calls.
//!
//! BPDL schemas generate typed consumer traits (e.g.,
//! `WindowsState`) that plugins implement and consume. This module
//! provides the primitives that bridge those typed traits to the
//! untyped [`crate::ServiceRequest`] / [`crate::ServiceResponse`]
//! transport the plugin host already speaks.
//!
//! # Model
//!
//! - The **provider** plugin implements the BPDL-generated trait (e.g.
//!   `impl WindowsState for MyWindowsPlugin`).
//! - The **consumer** plugin resolves a handle via
//!   [`crate::PluginHost::resolve_service`] and receives a
//!   [`TypedServiceHandle`], which wraps the untyped service transport
//!   in a way that's ergonomic to call from generated client stubs.
//!
//! # Serialization
//!
//! Calls across the plugin boundary serialize parameters as
//! `serde_json::Value` inside [`crate::ServiceRequest::payload`]. The
//! typed client stub serializes typed args into JSON, sends the request,
//! and deserializes the typed response. When both provider and consumer
//! are native Rust plugins loaded into the same process, an optimized
//! "in-process" fast path skips serialization and passes typed values
//! directly via [`InProcessTypedDispatch`].
//!
//! # Extensibility
//!
//! Non-Rust SDKs (TypeScript, Python, …) implement the same wire format
//! — serialized JSON parameters inside `ServiceRequest::payload` — and
//! gain the same plugin-to-plugin capability automatically.

use crate::{HostScope, PluginError, Result, ServiceKind};
use serde::{Serialize, de::DeserializeOwned};
use std::any::Any;
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

/// A typed reference to a provider plugin's interface impl.
///
/// In the current revision this is a narrow wrapper around a
/// type-erased `Arc<dyn Any + Send + Sync>`. Generated client stubs
/// downcast it to the specific BPDL-generated trait object at each call
/// site. As the plugin host grows full runtime-dispatch support,
/// [`TypedServiceHandle`] will expand with routing metadata
/// (interface id, provider id, protocol version).
pub struct TypedServiceHandle {
    capability: HostScope,
    interface_id: String,
    kind: ServiceKind,
    provider: Arc<dyn Any + Send + Sync>,
}

impl TypedServiceHandle {
    /// Construct a typed handle. Intended to be called by the plugin
    /// host when a provider registers its typed impl.
    #[must_use]
    pub fn new(
        capability: HostScope,
        interface_id: impl Into<String>,
        kind: ServiceKind,
        provider: Arc<dyn Any + Send + Sync>,
    ) -> Self {
        Self {
            capability,
            interface_id: interface_id.into(),
            kind,
            provider,
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

    /// Downcast the provider to a concrete type. Generated client stubs
    /// use this to obtain a reference to the trait impl they were
    /// compiled against.
    ///
    /// # Errors
    ///
    /// Returns [`TypedDispatchError::TypeMismatch`] if the registered
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
}

/// In-process typed dispatch for calls between two native Rust plugins
/// in the same process. Serialization-free and synchronous.
///
/// Non-native plugin consumers use the serialized `ServiceRequest`
/// transport in [`crate::service`] instead; the plugin host decides
/// which path to take based on where the provider lives.
pub struct InProcessTypedDispatch;

impl InProcessTypedDispatch {
    /// Serialize typed arguments for cross-process transport.
    ///
    /// # Errors
    ///
    /// Returns [`TypedDispatchError::Serialize`] if the typed arguments
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
    /// Returns [`TypedDispatchError::Deserialize`] if the bytes cannot
    /// be JSON-decoded into `T`.
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
