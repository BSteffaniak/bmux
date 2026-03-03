#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Cross-platform IPC protocol models for bmux.

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use uuid::Uuid;

pub mod frame;

/// Current IPC protocol version.
pub const CURRENT_PROTOCOL_VERSION: u16 = 1;

/// Protocol version used in IPC envelopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProtocolVersion(pub u16);

impl ProtocolVersion {
    /// The currently supported protocol version.
    #[must_use]
    pub const fn current() -> Self {
        Self(CURRENT_PROTOCOL_VERSION)
    }
}

impl Default for ProtocolVersion {
    fn default() -> Self {
        Self::current()
    }
}

/// Envelope discriminant for payload interpretation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvelopeKind {
    Request,
    Response,
    Event,
}

/// Versioned IPC envelope with request correlation support.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub version: ProtocolVersion,
    pub request_id: u64,
    pub kind: EnvelopeKind,
    pub payload: Vec<u8>,
}

impl Envelope {
    /// Build a new envelope.
    #[must_use]
    pub fn new(request_id: u64, kind: EnvelopeKind, payload: Vec<u8>) -> Self {
        Self {
            version: ProtocolVersion::current(),
            request_id,
            kind,
            payload,
        }
    }
}

/// Session selector accepted by commands and protocol requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionSelector {
    ById(Uuid),
    ByName(String),
}

/// Request payload variants for client/server IPC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Request {
    Hello {
        protocol_version: ProtocolVersion,
        client_name: String,
    },
    Ping,
    ServerStatus,
    ServerStop,
    NewSession {
        name: Option<String>,
    },
    ListSessions,
    KillSession {
        selector: SessionSelector,
    },
    Attach {
        selector: SessionSelector,
    },
    Detach,
}

/// Summary returned when listing sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: Uuid,
    pub name: Option<String>,
    pub window_count: usize,
    pub client_count: usize,
}

/// Successful response payload variants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponsePayload {
    Pong,
    ServerStatus { running: bool },
    SessionCreated { id: Uuid, name: Option<String> },
    SessionList { sessions: Vec<SessionSummary> },
    SessionKilled { id: Uuid },
    Attached { id: Uuid },
    Detached,
    ServerStopping,
}

/// Canonical error codes returned over IPC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    NotFound,
    AlreadyExists,
    InvalidRequest,
    VersionMismatch,
    Timeout,
    Internal,
}

/// Error details returned over IPC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub code: ErrorCode,
    pub message: String,
}

/// Top-level response message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Response {
    Ok(ResponsePayload),
    Err(ErrorResponse),
}

/// Event payload variants emitted by the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    ServerStarted,
    ServerStopping,
    SessionCreated { id: Uuid, name: Option<String> },
    SessionRemoved { id: Uuid },
    ClientAttached { id: Uuid },
    ClientDetached { id: Uuid },
}

/// Serialize any protocol message using postcard.
///
/// # Errors
///
/// Returns an error when serialization fails.
pub fn encode<T>(message: &T) -> Result<Vec<u8>, postcard::Error>
where
    T: Serialize,
{
    postcard::to_allocvec(message)
}

/// Deserialize any protocol message using postcard.
///
/// # Errors
///
/// Returns an error when deserialization fails.
pub fn decode<T>(bytes: &[u8]) -> Result<T, postcard::Error>
where
    T: DeserializeOwned,
{
    postcard::from_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::{
        decode, encode, Envelope, EnvelopeKind, ErrorCode, Event, ProtocolVersion, Request,
        Response, ResponsePayload, SessionSelector, SessionSummary,
    };
    use uuid::Uuid;

    #[test]
    fn serializes_request_roundtrip() {
        let request = Request::KillSession {
            selector: SessionSelector::ByName("dev-shell".to_string()),
        };
        let bytes = encode(&request).expect("request should encode");
        let decoded: Request = decode(&bytes).expect("request should decode");
        assert_eq!(decoded, request);
    }

    #[test]
    fn serializes_response_roundtrip() {
        let response = Response::Ok(ResponsePayload::SessionList {
            sessions: vec![SessionSummary {
                id: Uuid::new_v4(),
                name: Some("work".to_string()),
                window_count: 2,
                client_count: 1,
            }],
        });
        let bytes = encode(&response).expect("response should encode");
        let decoded: Response = decode(&bytes).expect("response should decode");
        assert_eq!(decoded, response);
    }

    #[test]
    fn serializes_event_roundtrip() {
        let event = Event::SessionCreated {
            id: Uuid::new_v4(),
            name: Some("ops".to_string()),
        };
        let bytes = encode(&event).expect("event should encode");
        let decoded: Event = decode(&bytes).expect("event should decode");
        assert_eq!(decoded, event);
    }

    #[test]
    fn serializes_envelope_roundtrip() {
        let payload = encode(&Request::Ping).expect("payload should encode");
        let envelope = Envelope {
            version: ProtocolVersion::current(),
            request_id: 7,
            kind: EnvelopeKind::Request,
            payload,
        };
        let bytes = encode(&envelope).expect("envelope should encode");
        let decoded: Envelope = decode(&bytes).expect("envelope should decode");
        assert_eq!(decoded, envelope);
    }

    #[test]
    fn serializes_session_selector_by_id_roundtrip() {
        let selector = SessionSelector::ById(Uuid::new_v4());
        let bytes = encode(&selector).expect("selector should encode");
        let decoded: SessionSelector = decode(&bytes).expect("selector should decode");
        assert_eq!(decoded, selector);
    }

    #[test]
    fn protocol_version_defaults_to_current() {
        assert_eq!(ProtocolVersion::default(), ProtocolVersion::current());
    }

    #[test]
    fn error_code_serializes_roundtrip() {
        let code = ErrorCode::VersionMismatch;
        let bytes = encode(&code).expect("error code should encode");
        let decoded: ErrorCode = decode(&bytes).expect("error code should decode");
        assert_eq!(decoded, code);
    }
}
