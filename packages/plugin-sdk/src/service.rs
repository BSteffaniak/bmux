use crate::{HostScope, PluginError, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceKind {
    Query,
    Command,
    Event,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PluginService {
    pub capability: HostScope,
    pub kind: ServiceKind,
    pub interface_id: String,
}

impl PluginService {
    /// Validate that this service definition has a non-empty interface ID.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::InvalidServiceInterfaceId`] if the interface ID
    /// is empty or contains only whitespace.
    pub fn validate(&self, plugin_id: &str) -> Result<()> {
        if self.interface_id.trim().is_empty() {
            return Err(PluginError::InvalidServiceInterfaceId {
                plugin_id: plugin_id.to_string(),
                capability: self.capability.as_str().to_string(),
                kind: self.kind,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderId {
    Plugin(String),
    Host,
}

impl ProviderId {
    #[must_use]
    pub const fn display_name(&self) -> &str {
        match self {
            Self::Plugin(plugin_id) => plugin_id.as_str(),
            Self::Host => "host",
        }
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredService {
    pub capability: HostScope,
    pub kind: ServiceKind,
    pub interface_id: String,
    pub provider: ProviderId,
}

impl RegisteredService {
    #[must_use]
    pub fn key(&self) -> (HostScope, ServiceKind, String) {
        (
            self.capability.clone(),
            self.kind,
            self.interface_id.clone(),
        )
    }
}

pub const CURRENT_SERVICE_PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ServiceProtocolVersion(pub u16);

impl ServiceProtocolVersion {
    #[must_use]
    pub const fn current() -> Self {
        Self(CURRENT_SERVICE_PROTOCOL_VERSION)
    }
}

impl Default for ServiceProtocolVersion {
    fn default() -> Self {
        Self::current()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceEnvelopeKind {
    Request,
    Response,
    Event,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceEnvelope {
    pub version: ServiceProtocolVersion,
    pub request_id: u64,
    pub kind: ServiceEnvelopeKind,
    pub payload: Vec<u8>,
}

impl ServiceEnvelope {
    #[must_use]
    pub const fn new(request_id: u64, kind: ServiceEnvelopeKind, payload: Vec<u8>) -> Self {
        Self {
            version: ServiceProtocolVersion::current(),
            request_id,
            kind,
            payload,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceRequest {
    pub caller_plugin_id: String,
    pub service: RegisteredService,
    pub operation: String,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceResponse {
    pub payload: Vec<u8>,
    pub error: Option<ServiceError>,
}

impl ServiceResponse {
    #[must_use]
    pub const fn ok(payload: Vec<u8>) -> Self {
        Self {
            payload,
            error: None,
        }
    }

    #[must_use]
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            payload: Vec::new(),
            error: Some(ServiceError {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
}

/// Serialize a service message using the bmux binary codec.
///
/// # Errors
///
/// Returns [`PluginError::ServiceProtocol`] if serialization fails.
pub fn encode_service_message<T>(message: &T) -> Result<Vec<u8>>
where
    T: Serialize,
{
    bmux_codec::to_vec(message).map_err(|error| PluginError::ServiceProtocol {
        details: error.to_string(),
    })
}

/// Deserialize a service message from binary codec bytes.
///
/// # Errors
///
/// Returns [`PluginError::ServiceProtocol`] if deserialization fails.
pub fn decode_service_message<T>(payload: &[u8]) -> Result<T>
where
    T: DeserializeOwned,
{
    bmux_codec::from_bytes(payload).map_err(|error| PluginError::ServiceProtocol {
        details: error.to_string(),
    })
}

/// Encode a typed message into a service envelope with the given request ID and kind.
///
/// Serializes both the inner message and the outer envelope using the binary codec.
///
/// # Errors
///
/// Returns [`PluginError::ServiceProtocol`] if serialization of the message
/// or the envelope fails.
pub fn encode_service_envelope<T>(
    request_id: u64,
    kind: ServiceEnvelopeKind,
    message: &T,
) -> Result<Vec<u8>>
where
    T: Serialize,
{
    encode_service_message(&ServiceEnvelope::new(
        request_id,
        kind,
        encode_service_message(message)?,
    ))
}

/// Decode a service envelope and extract the typed payload.
///
/// Validates the protocol version and envelope kind before deserializing
/// the inner payload.
///
/// # Errors
///
/// Returns [`PluginError::ServiceProtocol`] if the envelope cannot be
/// deserialized, the protocol version is unsupported, the envelope kind
/// does not match `expected_kind`, or the inner payload fails to deserialize.
pub fn decode_service_envelope<T>(
    payload: &[u8],
    expected_kind: ServiceEnvelopeKind,
) -> Result<(u64, T)>
where
    T: DeserializeOwned,
{
    let envelope: ServiceEnvelope = decode_service_message(payload)?;
    if envelope.version != ServiceProtocolVersion::current() {
        return Err(PluginError::ServiceProtocol {
            details: format!(
                "unsupported service protocol version {}; expected {}",
                envelope.version.0,
                ServiceProtocolVersion::current().0,
            ),
        });
    }
    if envelope.kind != expected_kind {
        return Err(PluginError::ServiceProtocol {
            details: format!(
                "unexpected service envelope kind {:?}; expected {:?}",
                envelope.kind, expected_kind,
            ),
        });
    }
    Ok((
        envelope.request_id,
        decode_service_message::<T>(&envelope.payload)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        ProviderId, RegisteredService, ServiceEnvelopeKind, ServiceError, ServiceKind,
        ServiceRequest, ServiceResponse, decode_service_envelope, decode_service_message,
        encode_service_envelope, encode_service_message,
    };
    use crate::HostScope;

    #[test]
    fn service_message_roundtrip() {
        let response = ServiceResponse::ok(vec![1, 2, 3]);
        let bytes = encode_service_message(&response).expect("service response should encode");
        let decoded: ServiceResponse =
            decode_service_message(&bytes).expect("service response should decode");
        assert_eq!(decoded, response);
    }

    #[test]
    fn service_envelope_roundtrip() {
        let request = ServiceRequest {
            caller_plugin_id: "example.native".to_string(),
            service: RegisteredService {
                capability: HostScope::new("bmux.permissions.read")
                    .expect("capability should parse"),
                kind: ServiceKind::Query,
                interface_id: "permission-query/v1".to_string(),
                provider: ProviderId::Plugin("bmux.permissions".to_string()),
            },
            operation: "list".to_string(),
            payload: vec![4, 5, 6],
        };

        let bytes = encode_service_envelope(41, ServiceEnvelopeKind::Request, &request)
            .expect("service envelope should encode");
        let (request_id, decoded): (u64, ServiceRequest) =
            decode_service_envelope(&bytes, ServiceEnvelopeKind::Request)
                .expect("service envelope should decode");
        assert_eq!(request_id, 41);
        assert_eq!(decoded, request);
    }

    // ── Level 1F: Extended plugin service protocol round-trips ───────────────

    #[test]
    fn service_response_error_roundtrip() {
        let response = ServiceResponse::error("NOT_FOUND", "resource not found");
        let bytes = encode_service_message(&response).expect("error response should encode");
        let decoded: ServiceResponse =
            decode_service_message(&bytes).expect("error response should decode");
        assert_eq!(decoded, response);
        assert!(decoded.error.is_some());
        let err = decoded.error.unwrap();
        assert_eq!(err.code, "NOT_FOUND");
        assert_eq!(err.message, "resource not found");
    }

    #[test]
    fn service_error_standalone_roundtrip() {
        let error = ServiceError {
            code: "INTERNAL".to_string(),
            message: "something went wrong".to_string(),
        };
        let bytes = encode_service_message(&error).expect("service error should encode");
        let decoded: ServiceError =
            decode_service_message(&bytes).expect("service error should decode");
        assert_eq!(decoded, error);
    }

    #[test]
    fn provider_id_host_roundtrip() {
        let provider = ProviderId::Host;
        let bytes = encode_service_message(&provider).expect("host provider should encode");
        let decoded: ProviderId =
            decode_service_message(&bytes).expect("host provider should decode");
        assert_eq!(decoded, provider);
    }

    #[test]
    fn provider_id_plugin_roundtrip() {
        let provider = ProviderId::Plugin("my-plugin".to_string());
        let bytes = encode_service_message(&provider).expect("plugin provider should encode");
        let decoded: ProviderId =
            decode_service_message(&bytes).expect("plugin provider should decode");
        assert_eq!(decoded, provider);
    }

    #[test]
    fn service_kind_all_variants_roundtrip() {
        for kind in &[ServiceKind::Query, ServiceKind::Command, ServiceKind::Event] {
            let bytes = encode_service_message(kind).expect("service kind should encode");
            let decoded: ServiceKind =
                decode_service_message(&bytes).expect("service kind should decode");
            assert_eq!(&decoded, kind);
        }
    }

    #[test]
    fn service_envelope_kind_all_variants_roundtrip() {
        for kind in &[
            ServiceEnvelopeKind::Request,
            ServiceEnvelopeKind::Response,
            ServiceEnvelopeKind::Event,
        ] {
            let bytes = encode_service_message(kind).expect("envelope kind should encode");
            let decoded: ServiceEnvelopeKind =
                decode_service_message(&bytes).expect("envelope kind should decode");
            assert_eq!(&decoded, kind);
        }
    }

    #[test]
    fn service_envelope_response_kind_roundtrip() {
        let response = ServiceResponse::ok(vec![7, 8, 9]);
        let bytes = encode_service_envelope(99, ServiceEnvelopeKind::Response, &response)
            .expect("response envelope should encode");
        let (request_id, decoded): (u64, ServiceResponse) =
            decode_service_envelope(&bytes, ServiceEnvelopeKind::Response)
                .expect("response envelope should decode");
        assert_eq!(request_id, 99);
        assert_eq!(decoded, response);
    }

    #[test]
    fn registered_service_with_host_provider_roundtrip() {
        let service = RegisteredService {
            capability: HostScope::new("bmux.sessions.read").expect("capability should parse"),
            kind: ServiceKind::Command,
            interface_id: "session-command/v1".to_string(),
            provider: ProviderId::Host,
        };
        let bytes = encode_service_message(&service).expect("registered service should encode");
        let decoded: RegisteredService =
            decode_service_message(&bytes).expect("registered service should decode");
        assert_eq!(decoded, service);
    }
}
