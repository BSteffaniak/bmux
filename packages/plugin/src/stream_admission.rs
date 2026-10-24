//! Neutral, host-trusted admission for opaque streams.
//!
//! An authority must atomically consume single-use tickets, fail closed, and
//! validate current lease validity. Ticket issuance and policy belong to its
//! provider. Never construct bindings from wire metadata. Providers may return
//! the same `Arc<StreamAdmissionBinding>` for distinct tickets on one lease.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// Immutable identity supplied only by an in-process authority, not the wire.
#[derive(Debug, PartialEq, Eq)]
pub struct StreamAdmissionBinding {
    subject_id: Uuid,
    lease_id: Uuid,
}

impl StreamAdmissionBinding {
    /// Construct a binding after authenticating the ticket's issuer and scope.
    #[must_use]
    pub const fn new(subject_id: Uuid, lease_id: Uuid) -> Self {
        Self {
            subject_id,
            lease_id,
        }
    }

    #[must_use]
    pub const fn subject_id(&self) -> Uuid {
        self.subject_id
    }

    #[must_use]
    pub const fn lease_id(&self) -> Uuid {
        self.lease_id
    }
}

/// Pure wire adapter for a provider's generated service contract. Implement this
/// with generated BPDL payloads; no runtime state or policy crosses this boundary.
pub trait StreamAdmissionProviderCodec: Send + Sync {
    /// Encode the provider instance probe.
    ///
    /// # Errors
    /// Rejects malformed or unsupported provider representations.
    fn encode_probe(&self) -> Result<Vec<u8>, StreamAdmissionError>;
    /// Decode a fresh non-nil authority instance UUID.
    ///
    /// # Errors
    /// Rejects malformed or unsupported provider representations.
    fn decode_probe(&self, bytes: &[u8]) -> Result<Uuid, StreamAdmissionError>;
    ///
    /// # Errors
    /// Rejects malformed or unsupported provider representations.
    fn encode_consume(&self, ticket: &[u8]) -> Result<Vec<u8>, StreamAdmissionError>;
    ///
    /// # Errors
    /// Rejects malformed or unsupported provider representations.
    fn decode_consume(&self, bytes: &[u8]) -> Result<StreamAdmissionBinding, StreamAdmissionError>;
    ///
    /// # Errors
    /// Rejects malformed or unsupported provider representations.
    fn encode_validate(
        &self,
        binding: &StreamAdmissionBinding,
    ) -> Result<Vec<u8>, StreamAdmissionError>;
    ///
    /// # Errors
    /// Rejects malformed or unsupported provider representations.
    fn decode_validate(&self, bytes: &[u8]) -> Result<(), StreamAdmissionError>;
}

/// Exact route approvals contributed by providers after side-effect migration.
pub const STREAM_ADMITTED_ROUTES_EXTENSION: &str = "bmux.stream-admitted-routes.v1";

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct StreamAdmittedRoute {
    pub capability: String,
    pub kind: bmux_plugin_sdk::ServiceKind,
    pub interface_id: String,
    pub operation: String,
}

/// Serialized activation extension point for explicitly selected admission.
pub const STREAM_ADMISSION_PROVIDER_EXTENSION: &str = "bmux.stream-admission-provider.v1";

/// Version-one neutral generated-service representation.
///
/// Each operation uses a checked BPDL frame. Providers must smoke-test their generated schema against
/// this codec; incompatible representations fail closed rather than guessing.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct StreamAdmissionProviderDeclaration {
    pub probe: StreamAdmissionProviderOperation,
    pub consume: StreamAdmissionProviderOperation,
    pub validate: StreamAdmissionProviderOperation,
    pub deadline_ms: u64,
    pub max_in_flight: usize,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct StreamAdmissionProviderOperation {
    pub capability: String,
    pub interface_id: String,
    pub operation: String,
    pub contract: [u8; 32],
}

impl StreamAdmissionProviderDeclaration {
    fn encode<T: serde::Serialize>(
        operation: &StreamAdmissionProviderOperation,
        value: &T,
    ) -> Result<Vec<u8>, StreamAdmissionError> {
        let bytes =
            bmux_plugin_sdk::encode_service_message(value).map_err(|_| StreamAdmissionError)?;
        bmux_plugin_sdk::service_contract::encode(
            &operation.interface_id,
            &operation.contract,
            &bytes,
        )
        .map_err(|_| StreamAdmissionError)
    }
    fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, StreamAdmissionError> {
        let bytes = bmux_plugin_sdk::service_contract::decode_response(bytes)
            .map_err(|_| StreamAdmissionError)?;
        bmux_plugin_sdk::decode_service_message(bytes).map_err(|_| StreamAdmissionError)
    }
}

impl StreamAdmissionProviderCodec for StreamAdmissionProviderDeclaration {
    fn encode_probe(&self) -> Result<Vec<u8>, StreamAdmissionError> {
        Self::encode(&self.probe, &())
    }
    fn decode_probe(&self, bytes: &[u8]) -> Result<Uuid, StreamAdmissionError> {
        Self::decode(bytes)
    }
    fn encode_consume(&self, ticket: &[u8]) -> Result<Vec<u8>, StreamAdmissionError> {
        #[derive(serde::Serialize)]
        struct Ticket {
            #[serde(with = "bmux_codec::serde_bytes_vec")]
            ticket: Vec<u8>,
        }
        Self::encode(
            &self.consume,
            &Ticket {
                ticket: ticket.to_vec(),
            },
        )
    }
    fn decode_consume(&self, bytes: &[u8]) -> Result<StreamAdmissionBinding, StreamAdmissionError> {
        #[derive(serde::Deserialize)]
        struct Binding {
            subject_id: Uuid,
            lease_id: Uuid,
        }
        let binding: Binding = Self::decode(bytes)?;
        Ok(StreamAdmissionBinding::new(
            binding.subject_id,
            binding.lease_id,
        ))
    }
    fn encode_validate(
        &self,
        binding: &StreamAdmissionBinding,
    ) -> Result<Vec<u8>, StreamAdmissionError> {
        #[derive(serde::Serialize)]
        struct Binding {
            subject_id: Uuid,
            lease_id: Uuid,
        }
        Self::encode(
            &self.validate,
            &Binding {
                subject_id: binding.subject_id(),
                lease_id: binding.lease_id(),
            },
        )
    }
    fn decode_validate(&self, bytes: &[u8]) -> Result<(), StreamAdmissionError> {
        if Self::decode::<bool>(bytes)? {
            Ok(())
        } else {
            Err(StreamAdmissionError)
        }
    }
}

/// Admission failures intentionally expose no ticket or provider details.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("stream admission denied")]
pub struct StreamAdmissionError;

/// Host-side provider of authenticated bindings and current lease validity.
pub trait StreamAdmissionAuthority: Send + Sync {
    /// Atomically consume an opaque, single-use ticket. Replays must fail.
    ///
    /// # Errors
    /// Returns an error for invalid, expired, revoked, or already used tickets.
    fn consume(&self, ticket: &[u8]) -> Result<Arc<StreamAdmissionBinding>, StreamAdmissionError>;

    /// Check current validity without trusting any caller-supplied metadata.
    ///
    /// # Errors
    /// Returns an error when the binding is no longer authorized or unavailable.
    fn validate(&self, binding: &StreamAdmissionBinding) -> Result<(), StreamAdmissionError>;
}

/// Register this neutral handle in `PluginStateRegistry` during initialization.
///
/// In-process consumers resolve a host-generated stream UUID with `binding`;
/// neither a Hello field nor service payload can install a trusted binding.
#[derive(Clone)]
pub struct StreamAdmissionAuthorityHandle {
    authority: Arc<dyn StreamAdmissionAuthority>,
    bindings: Arc<Mutex<BTreeMap<Uuid, Arc<StreamAdmissionBinding>>>>,
}

impl StreamAdmissionAuthorityHandle {
    #[must_use]
    pub fn new(authority: Arc<dyn StreamAdmissionAuthority>) -> Self {
        Self {
            authority,
            bindings: Arc::default(),
        }
    }

    /// Consume a ticket and retain its trusted binding until the guard drops.
    /// The host must supply its own fresh stream UUID, never a wire UUID.
    ///
    /// # Errors
    /// Fails closed on provider failure, invalid binding, or duplicate stream ID.
    pub fn admit(
        &self,
        stream_id: Uuid,
        ticket: &[u8],
    ) -> Result<AdmittedStream, StreamAdmissionError> {
        let binding = self.authority.consume(ticket)?;
        if binding.subject_id().is_nil() || binding.lease_id().is_nil() {
            return Err(StreamAdmissionError);
        }
        self.authority.validate(&binding)?;
        let mut bindings = self.bindings.lock().map_err(|_| StreamAdmissionError)?;
        if bindings.contains_key(&stream_id) {
            return Err(StreamAdmissionError);
        }
        bindings.insert(stream_id, Arc::clone(&binding));
        drop(bindings);
        Ok(AdmittedStream {
            handle: self.clone(),
            stream_id,
            binding,
        })
    }

    /// Resolve and revalidate a host-retained immutable binding.
    /// Absence means unadmitted, not authenticated local authority.
    #[must_use]
    pub fn binding(&self, stream_id: Uuid) -> Option<Arc<StreamAdmissionBinding>> {
        let binding = self.bindings.lock().ok()?.get(&stream_id).cloned()?;
        self.authority.validate(&binding).ok()?;
        Some(binding)
    }
}

/// Connection-scoped trusted state; dropping it removes the host lookup.
pub struct AdmittedStream {
    handle: StreamAdmissionAuthorityHandle,
    stream_id: Uuid,
    binding: Arc<StreamAdmissionBinding>,
}

impl AdmittedStream {
    #[must_use]
    pub fn binding(&self) -> &StreamAdmissionBinding {
        &self.binding
    }

    /// Recheck immediately before dispatch. Providers remain responsible for
    /// authorization at side effects and for cancelling ongoing work on revoke.
    ///
    /// # Errors
    /// Fails closed if the authority no longer validates this binding.
    pub fn validate(&self) -> Result<(), StreamAdmissionError> {
        self.handle.authority.validate(&self.binding)
    }
}

impl Drop for AdmittedStream {
    fn drop(&mut self) {
        if let Ok(mut bindings) = self.handle.bindings.lock() {
            bindings.remove(&self.stream_id);
        }
    }
}

/// Positively recorded host provenance. Unknown and revoked streams are invalid.
pub enum StreamProvenance {
    Local,
    Admitted(Arc<StreamAdmissionBinding>),
    Invalid,
}

enum RecordedOrigin {
    Local,
    Admitted(StreamAdmissionAuthorityHandle, Arc<StreamAdmissionBinding>),
}

/// Neutral host-only origin bookkeeping. Never populate from request metadata.
#[derive(Clone, Default)]
pub struct StreamProvenanceRegistry {
    origins: Arc<Mutex<BTreeMap<Uuid, RecordedOrigin>>>,
}

impl StreamProvenanceRegistry {
    /// Resolve only from the actual host's explicitly supplied registry.
    /// A library-local global registry is NOT a substitute across cdylib or
    /// process boundaries. Those runtimes need an explicit generic host-context
    /// bridge; until supplied, consumers must deny rather than infer locality.
    #[must_use]
    pub fn from_host_registry(registry: &crate::PluginStateRegistry) -> Option<Self> {
        registry
            .get::<Self>()?
            .read()
            .ok()
            .map(|handle| handle.clone())
    }

    /// Resolve a host-generated stream UUID, checking current admission validity.
    #[must_use]
    pub fn lookup(&self, stream_id: Uuid) -> StreamProvenance {
        let Ok(origins) = self.origins.lock() else {
            return StreamProvenance::Invalid;
        };
        let recorded = match origins.get(&stream_id) {
            Some(RecordedOrigin::Local) => return StreamProvenance::Local,
            Some(RecordedOrigin::Admitted(handle, binding)) => {
                (handle.clone(), Arc::clone(binding))
            }
            None => return StreamProvenance::Invalid,
        };
        drop(origins);
        if recorded.0.authority.validate(&recorded.1).is_ok() {
            StreamProvenance::Admitted(recorded.1)
        } else {
            StreamProvenance::Invalid
        }
    }

    /// Host-only: record the result of the serial handshake, not a wire claim.
    /// `None` is permitted only for an ordinary direct local Hello.
    ///
    /// # Errors
    /// Rejects duplicate identities, poisoned bookkeeping, or revoked admission.
    pub fn record(
        &self,
        stream_id: Uuid,
        admitted: Option<&AdmittedStream>,
    ) -> Result<StreamProvenanceGuard, StreamAdmissionError> {
        let origin = if let Some(admitted) = admitted {
            if admitted.stream_id != stream_id {
                return Err(StreamAdmissionError);
            }
            admitted.validate()?;
            RecordedOrigin::Admitted(admitted.handle.clone(), Arc::clone(&admitted.binding))
        } else {
            RecordedOrigin::Local
        };
        let mut origins = self.origins.lock().map_err(|_| StreamAdmissionError)?;
        if origins.contains_key(&stream_id) {
            return Err(StreamAdmissionError);
        }
        origins.insert(stream_id, origin);
        drop(origins);
        Ok(StreamProvenanceGuard {
            registry: self.clone(),
            stream_id,
        })
    }
}

/// Host connection lifetime; removal always makes provenance invalid, not local.
pub struct StreamProvenanceGuard {
    registry: StreamProvenanceRegistry,
    stream_id: Uuid,
}

impl Drop for StreamProvenanceGuard {
    fn drop(&mut self) {
        if let Ok(mut origins) = self.registry.origins.lock() {
            origins.remove(&self.stream_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU8, Ordering};

    #[test]
    fn provider_declaration_requires_checked_response_acceptance() {
        #[derive(serde::Serialize)]
        struct Binding {
            subject_id: Uuid,
            lease_id: Uuid,
        }
        let operation = StreamAdmissionProviderOperation {
            capability: "test.admission".into(),
            interface_id: "test-admission/v1".into(),
            operation: "probe".into(),
            contract: [7; 32],
        };
        let codec = StreamAdmissionProviderDeclaration {
            probe: operation.clone(),
            consume: operation.clone(),
            validate: operation.clone(),
            deadline_ms: 100,
            max_in_flight: 1,
        };
        let subject = Uuid::new_v4();
        let lease = Uuid::new_v4();
        let raw = bmux_plugin_sdk::encode_service_message(&subject).unwrap();
        assert!(codec.decode_probe(&raw).is_err());
        let request_frame = bmux_plugin_sdk::service_contract::encode(
            &operation.interface_id,
            &operation.contract,
            &raw,
        )
        .unwrap();
        assert!(codec.decode_probe(&request_frame).is_err());
        assert_eq!(
            codec
                .decode_probe(&bmux_plugin_sdk::service_contract::encode_response(&raw))
                .unwrap(),
            subject
        );
        let binding = bmux_plugin_sdk::encode_service_message(&Binding {
            subject_id: subject,
            lease_id: lease,
        })
        .unwrap();
        let binding = codec
            .decode_consume(&bmux_plugin_sdk::service_contract::encode_response(
                &binding,
            ))
            .unwrap();
        assert_eq!((binding.subject_id(), binding.lease_id()), (subject, lease));
        for valid in [true, false] {
            let raw = bmux_plugin_sdk::encode_service_message(&valid).unwrap();
            assert!(codec.decode_validate(&raw).is_err());
            assert_eq!(
                codec
                    .decode_validate(&bmux_plugin_sdk::service_contract::encode_response(&raw))
                    .is_ok(),
                valid
            );
        }
    }

    struct Authority {
        binding: Arc<StreamAdmissionBinding>,
        used: AtomicU8,
    }

    impl StreamAdmissionAuthority for Authority {
        fn consume(
            &self,
            ticket: &[u8],
        ) -> Result<Arc<StreamAdmissionBinding>, StreamAdmissionError> {
            let bit = match ticket {
                [1] => 1,
                [2] => 2,
                _ => return Err(StreamAdmissionError),
            };
            if self.used.fetch_or(bit, Ordering::SeqCst) & bit != 0 {
                return Err(StreamAdmissionError);
            }
            Ok(Arc::clone(&self.binding))
        }

        fn validate(&self, _: &StreamAdmissionBinding) -> Result<(), StreamAdmissionError> {
            Ok(())
        }
    }

    #[test]
    fn distinct_tickets_share_one_immutable_binding_and_drop_independently() {
        let authority = Arc::new(Authority {
            binding: Arc::new(StreamAdmissionBinding::new(
                Uuid::from_u128(1),
                Uuid::from_u128(2),
            )),
            used: AtomicU8::new(0),
        });
        let handle = StreamAdmissionAuthorityHandle::new(authority.clone());
        let first_id = Uuid::from_u128(3);
        let second_id = Uuid::from_u128(4);
        let first = handle.admit(first_id, &[1]).unwrap();
        let second = handle.admit(second_id, &[2]).unwrap();
        assert!(Arc::ptr_eq(
            &handle.binding(first_id).unwrap(),
            &handle.binding(second_id).unwrap()
        ));
        assert!(Arc::ptr_eq(
            &handle.binding(first_id).unwrap(),
            &authority.binding
        ));
        drop(first);
        assert!(handle.binding(first_id).is_none());
        assert!(handle.binding(second_id).is_some());
        drop(second);
        assert!(handle.binding(second_id).is_none());
    }
}
