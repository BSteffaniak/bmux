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
    pub fn display_name(&self) -> &str {
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
    pub fn ok(payload: Vec<u8>) -> Self {
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

pub fn encode_service_message<T>(message: &T) -> Result<Vec<u8>>
where
    T: Serialize,
{
    postcard::to_allocvec(message).map_err(|error| PluginError::ServiceProtocol {
        details: error.to_string(),
    })
}

pub fn decode_service_message<T>(payload: &[u8]) -> Result<T>
where
    T: DeserializeOwned,
{
    postcard::from_bytes(payload).map_err(|error| PluginError::ServiceProtocol {
        details: error.to_string(),
    })
}

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
        ProviderId, RegisteredService, ServiceEnvelopeKind, ServiceKind, ServiceRequest,
        ServiceResponse, decode_service_envelope, decode_service_message, encode_service_envelope,
        encode_service_message,
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
}
