#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::large_enum_variant)]
#![allow(clippy::struct_excessive_bools)]

//! Cross-platform IPC protocol models for bmux.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub mod compressed_stream;
pub mod compression;
pub mod frame;
pub mod transport;

/// Cross-platform local IPC endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "transport", content = "address", rename_all = "snake_case")]
pub enum IpcEndpoint {
    UnixSocket(PathBuf),
    WindowsNamedPipe(String),
}

impl IpcEndpoint {
    /// Construct a Unix domain socket endpoint.
    #[must_use]
    pub fn unix_socket(path: impl Into<PathBuf>) -> Self {
        Self::UnixSocket(path.into())
    }

    /// Construct a Windows named pipe endpoint.
    #[must_use]
    pub fn windows_named_pipe(name: impl Into<String>) -> Self {
        Self::WindowsNamedPipe(name.into())
    }

    /// Return the Unix socket path when this endpoint uses Unix sockets.
    #[must_use]
    pub fn as_unix_socket(&self) -> Option<&Path> {
        match self {
            Self::UnixSocket(path) => Some(path.as_path()),
            Self::WindowsNamedPipe(_) => None,
        }
    }

    /// Return the Windows named pipe when this endpoint uses named pipes.
    #[must_use]
    pub const fn as_windows_named_pipe(&self) -> Option<&str> {
        match self {
            Self::UnixSocket(_) => None,
            Self::WindowsNamedPipe(name) => Some(name.as_str()),
        }
    }
}

/// Current IPC protocol version.
pub const CURRENT_PROTOCOL_VERSION: u16 = 3;

/// Current wire-compatibility epoch for IPC framing.
pub const CURRENT_WIRE_EPOCH: u16 = CURRENT_PROTOCOL_VERSION;

/// Current negotiated protocol revision.
pub const CURRENT_PROTOCOL_REVISION: u32 = 1;

/// Minimum protocol revision this build can negotiate.
pub const MIN_SUPPORTED_PROTOCOL_REVISION: u32 = 1;

pub const CORE_CAPABILITY_SESSION: &str = "core.session";
pub const CORE_CAPABILITY_ATTACH: &str = "core.attach";
pub const CORE_CAPABILITY_PANE_IO: &str = "core.pane_io";
pub const CORE_CAPABILITY_DETACH: &str = "core.detach";

/// Core protocol capabilities required for baseline bmux operation.
pub const CORE_PROTOCOL_CAPABILITIES: &[&str] = &[
    CORE_CAPABILITY_SESSION,
    CORE_CAPABILITY_ATTACH,
    CORE_CAPABILITY_PANE_IO,
    CORE_CAPABILITY_DETACH,
];

// Compression capability strings (non-core, optional).
pub const CAPABILITY_COMPRESSION_PAYLOAD_ZSTD: &str = "compression.payload.zstd";
pub const CAPABILITY_COMPRESSION_PAYLOAD_LZ4: &str = "compression.payload.lz4";
pub const CAPABILITY_COMPRESSION_FRAME_ZSTD: &str = "compression.frame.zstd";
pub const CAPABILITY_COMPRESSION_FRAME_LZ4: &str = "compression.frame.lz4";
pub const CAPABILITY_COMPRESSION_TRANSPORT_ZSTD: &str = "compression.transport.zstd";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolRevisionRange {
    pub min: u32,
    pub max: u32,
}

impl ProtocolRevisionRange {
    #[must_use]
    pub const fn new(min: u32, max: u32) -> Self {
        Self { min, max }
    }

    #[must_use]
    pub const fn current() -> Self {
        Self {
            min: MIN_SUPPORTED_PROTOCOL_REVISION,
            max: CURRENT_PROTOCOL_REVISION,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolContract {
    pub wire_epoch: u16,
    pub revisions: ProtocolRevisionRange,
    pub capabilities: Vec<String>,
}

impl ProtocolContract {
    #[must_use]
    pub const fn current(capabilities: Vec<String>) -> Self {
        Self {
            wire_epoch: CURRENT_WIRE_EPOCH,
            revisions: ProtocolRevisionRange::current(),
            capabilities,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NegotiatedProtocol {
    pub wire_epoch: u16,
    pub revision: u32,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncompatibilityReason {
    WireEpochMismatch {
        client: u16,
        server: u16,
    },
    NoCommonRevision {
        client_min: u32,
        client_max: u32,
        server_min: u32,
        server_max: u32,
    },
    MissingCoreCapabilities {
        missing: Vec<String>,
    },
}

#[must_use]
pub fn default_supported_capabilities() -> Vec<String> {
    #[allow(unused_mut)]
    let mut caps = vec![
        CORE_CAPABILITY_SESSION.to_string(),
        CORE_CAPABILITY_ATTACH.to_string(),
        CORE_CAPABILITY_PANE_IO.to_string(),
        CORE_CAPABILITY_DETACH.to_string(),
    ];
    // Advertise compression capabilities when compiled in.
    // Note: Frame compression capabilities are NOT advertised by default
    // because frame compression requires both the server reader and the
    // client writer to switch to the compressed format simultaneously.
    // The non-streaming BmuxClient path (used by playbooks and the CLI)
    // does not support frame compression.  Frame compression is internal
    // infrastructure that may be exposed via an advanced config in the future.
    #[cfg(feature = "compression-zstd")]
    {
        caps.push(CAPABILITY_COMPRESSION_PAYLOAD_ZSTD.to_string());
        caps.push(CAPABILITY_COMPRESSION_TRANSPORT_ZSTD.to_string());
    }
    #[cfg(feature = "compression-lz4")]
    {
        caps.push(CAPABILITY_COMPRESSION_PAYLOAD_LZ4.to_string());
    }
    caps
}

/// Negotiate a protocol agreement between a client and server contract.
///
/// Compares wire epochs, finds the highest common protocol revision, intersects
/// capability sets, and verifies that all `core_required` capabilities are
/// present in the intersection.
///
/// # Errors
///
/// Returns [`IncompatibilityReason::WireEpochMismatch`] if the wire epochs
/// differ, [`IncompatibilityReason::NoCommonRevision`] if the revision ranges
/// do not overlap, or [`IncompatibilityReason::MissingCoreCapabilities`] if
/// any required core capability is absent from the negotiated set.
pub fn negotiate_protocol(
    client: &ProtocolContract,
    server: &ProtocolContract,
    core_required: &[&str],
) -> Result<NegotiatedProtocol, IncompatibilityReason> {
    if client.wire_epoch != server.wire_epoch {
        return Err(IncompatibilityReason::WireEpochMismatch {
            client: client.wire_epoch,
            server: server.wire_epoch,
        });
    }

    let overlap_min = client.revisions.min.max(server.revisions.min);
    let overlap_max = client.revisions.max.min(server.revisions.max);
    if overlap_min > overlap_max {
        return Err(IncompatibilityReason::NoCommonRevision {
            client_min: client.revisions.min,
            client_max: client.revisions.max,
            server_min: server.revisions.min,
            server_max: server.revisions.max,
        });
    }

    let server_caps: BTreeSet<&str> = server.capabilities.iter().map(String::as_str).collect();
    let client_caps: BTreeSet<&str> = client.capabilities.iter().map(String::as_str).collect();

    let negotiated_caps: Vec<String> = server_caps
        .intersection(&client_caps)
        .map(|cap| (*cap).to_string())
        .collect();

    let negotiated_set: BTreeSet<&str> = negotiated_caps.iter().map(String::as_str).collect();
    let missing: Vec<String> = core_required
        .iter()
        .copied()
        .filter(|required| !negotiated_set.contains(required))
        .map(std::string::ToString::to_string)
        .collect();
    if !missing.is_empty() {
        return Err(IncompatibilityReason::MissingCoreCapabilities { missing });
    }

    Ok(NegotiatedProtocol {
        wire_epoch: server.wire_epoch,
        revision: overlap_max,
        capabilities: negotiated_caps,
    })
}

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
    #[serde(with = "bmux_codec::serde_bytes_vec")]
    pub payload: Vec<u8>,
}

impl Envelope {
    /// Build a new envelope.
    #[must_use]
    pub const fn new(request_id: u64, kind: EnvelopeKind, payload: Vec<u8>) -> Self {
        Self {
            version: ProtocolVersion::current(),
            request_id,
            kind,
            payload,
        }
    }
}

/// Generic service invocation kind for plugin-dispatched RPC calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvokeServiceKind {
    Query,
    Command,
}

/// Request payload variants for client/server IPC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Request {
    Hello {
        protocol_version: ProtocolVersion,
        client_name: String,
        principal_id: Uuid,
    },
    Ping,
    WhoAmIPrincipal,
    ServerStatus,
    ServerStop,
    InvokeService {
        capability: String,
        kind: InvokeServiceKind,
        interface_id: String,
        operation: String,
        #[serde(with = "bmux_codec::serde_bytes_vec")]
        payload: Vec<u8>,
    },
    InvokeServicePipeline {
        pipeline: ServicePipelineRequest,
    },
    /// Emit a wire-encoded payload onto the server's plugin event bus
    /// under `kind`. The server looks up the registered channel and
    /// invokes its decoder to deserialise the bytes into the channel's
    /// typed value, then publishes on the same bus. Used by the
    /// attach runtime to relay state-channel payloads (e.g. attach
    /// layout snapshots) from client-process plugins to their
    /// server-process counterparts so every plugin can subscribe to
    /// the same wire vocabulary regardless of which process owns it.
    EmitOnPluginBus {
        kind: String,
        #[serde(with = "bmux_codec::serde_bytes_vec")]
        payload: Vec<u8>,
    },
    SubscribeEvents,
    PollEvents {
        max_events: usize,
    },
    /// Enable server-push event delivery on this connection.
    ///
    /// After the server responds with `EventPushEnabled`, it will write
    /// `EnvelopeKind::Event` frames asynchronously. Only streaming-capable
    /// clients (which split the socket into read/write halves and demux
    /// incoming frames) should send this request.
    EnableEventPush,
    HelloV2 {
        contract: ProtocolContract,
        client_name: String,
        principal_id: Uuid,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServicePipelineRequest {
    #[serde(default, with = "json_value_map")]
    pub inputs: BTreeMap<String, serde_json::Value>,
    pub steps: Vec<ServicePipelineStep>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServicePipelineStep {
    pub capability: String,
    pub kind: InvokeServiceKind,
    pub interface_id: String,
    pub operation: String,
    pub payload: ServicePipelinePayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServicePipelinePayload {
    Encoded {
        #[serde(with = "bmux_codec::serde_bytes_vec")]
        payload: Vec<u8>,
    },
    JsonTemplate {
        #[serde(with = "json_value")]
        value: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        field_order: Option<Vec<String>>,
    },
}

/// Successful response payload variants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponsePayload {
    Pong,
    PrincipalIdentity {
        principal_id: Uuid,
        server_control_principal_id: Uuid,
        force_local_permitted: bool,
    },
    ServerStatus {
        running: bool,
        principal_id: Uuid,
        server_control_principal_id: Uuid,
    },
    EventsSubscribed,
    EventBatch {
        events: Vec<Event>,
    },
    /// Acknowledgement that server-push event delivery has been enabled.
    EventPushEnabled,
    ServerStopping,
    /// Acknowledgement that an [`Request::EmitOnPluginBus`] payload
    /// was delivered to the server's event bus. `emitted` is `true`
    /// when a subscriber-ready channel received the bytes; `false`
    /// when no channel was registered (dropped silently so early
    /// wire traffic before plugin activation doesn't surface an
    /// error).
    PluginBusEmitted {
        emitted: bool,
    },
    ServiceInvoked {
        #[serde(with = "bmux_codec::serde_bytes_vec")]
        payload: Vec<u8>,
    },
    ServicePipelineInvoked {
        results: Vec<ServicePipelineStepResult>,
    },
    HelloNegotiated {
        negotiated: NegotiatedProtocol,
    },
    HelloIncompatible {
        reason: IncompatibilityReason,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServicePipelineStepResult {
    #[serde(with = "bmux_codec::serde_bytes_vec")]
    pub payload: Vec<u8>,
    #[serde(default, with = "json_value_map")]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

mod json_value {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S>(value: &serde_json::Value, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let encoded = serde_json::to_string(value).map_err(serde::ser::Error::custom)?;
        encoded.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<serde_json::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        serde_json::from_str(&encoded).map_err(serde::de::Error::custom)
    }
}

mod json_value_map {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S>(
        value: &BTreeMap<String, serde_json::Value>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let encoded = value
            .iter()
            .map(|(key, value)| {
                serde_json::to_string(value)
                    .map(|value| (key.clone(), value))
                    .map_err(serde::ser::Error::custom)
            })
            .collect::<Result<BTreeMap<_, _>, S::Error>>()?;
        encoded.serialize(serializer)
    }

    pub fn deserialize<'de, D>(
        deserializer: D,
    ) -> Result<BTreeMap<String, serde_json::Value>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = BTreeMap::<String, String>::deserialize(deserializer)?;
        encoded
            .into_iter()
            .map(|(key, value)| {
                serde_json::from_str(&value)
                    .map(|value| (key, value))
                    .map_err(serde::de::Error::custom)
            })
            .collect()
    }
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
    /// Plugin-bus emission forwarded from the server for client-side
    /// consumption. Forwarded kinds are declared in each plugin's
    /// manifest (`[[event_publications]] forward_to_streaming_clients
    /// = true`). The payload is `bmux_codec`-encoded and carries the
    /// plugin's typed event struct; consumers decode based on `kind`.
    PluginBusEvent {
        /// Canonical event kind (e.g. `"bmux.scene/scene-protocol"`).
        kind: String,
        /// `bmux_codec`-encoded typed payload.
        #[serde(with = "bmux_codec::serde_bytes_vec")]
        payload: Vec<u8>,
    },
}

/// Serialize any protocol message using the bmux binary codec.
///
/// # Errors
///
/// Returns an error when serialization fails.
pub fn encode<T>(message: &T) -> Result<Vec<u8>, bmux_codec::Error>
where
    T: Serialize,
{
    bmux_codec::to_vec(message)
}

/// Deserialize any protocol message using the bmux binary codec.
///
/// # Errors
///
/// Returns an error when deserialization fails.
pub fn decode<T>(bytes: &[u8]) -> Result<T, bmux_codec::Error>
where
    T: DeserializeOwned,
{
    bmux_codec::from_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_attach_image_protocol::{AttachPaneImage, CompressionId};
    use bmux_performance_state::{PerformanceRecordingLevel, PerformanceRuntimeSettings};
    use bmux_recording_protocol::{
        DisplayActivityKind, DisplayCursorShape, DisplayTrackEnvelope, DisplayTrackEvent,
        RECORDING_FORMAT_VERSION, RecordingEventEnvelope as ProtocolRecordingEventEnvelope,
        RecordingEventKind, RecordingPayload as ProtocolRecordingPayload, RecordingProfile,
        RecordingSummary, read_frames, write_frame,
    };
    use std::path::Path;

    type RecordingPayload = ProtocolRecordingPayload<Event, ErrorCode>;
    type RecordingEventEnvelope = ProtocolRecordingEventEnvelope<Event, ErrorCode>;

    #[test]
    fn serializes_request_roundtrip() {
        let request = Request::Ping;
        let bytes = encode(&request).expect("request should encode");
        let decoded: Request = decode(&bytes).expect("request should decode");
        assert_eq!(decoded, request);
    }

    #[test]
    fn serializes_response_roundtrip() {
        let response = Response::Ok(ResponsePayload::Pong);
        let bytes = encode(&response).expect("response should encode");
        let decoded: Response = decode(&bytes).expect("response should decode");
        assert_eq!(decoded, response);
    }

    #[test]
    fn serializes_event_roundtrip() {
        let event = Event::ServerStarted;
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

    #[test]
    fn negotiate_protocol_selects_highest_common_revision() {
        let client = ProtocolContract {
            wire_epoch: CURRENT_WIRE_EPOCH,
            revisions: ProtocolRevisionRange::new(1, 4),
            capabilities: vec![
                CORE_CAPABILITY_SESSION.to_string(),
                CORE_CAPABILITY_ATTACH.to_string(),
                CORE_CAPABILITY_PANE_IO.to_string(),
                CORE_CAPABILITY_DETACH.to_string(),
            ],
        };
        let server = ProtocolContract {
            wire_epoch: CURRENT_WIRE_EPOCH,
            revisions: ProtocolRevisionRange::new(2, 3),
            capabilities: default_supported_capabilities(),
        };

        let negotiated = negotiate_protocol(&client, &server, CORE_PROTOCOL_CAPABILITIES)
            .expect("negotiation should succeed");
        assert_eq!(negotiated.revision, 3);
        assert_eq!(
            negotiated.capabilities.len(),
            CORE_PROTOCOL_CAPABILITIES.len()
        );
    }

    #[test]
    fn negotiate_protocol_rejects_wire_epoch_mismatch() {
        let client = ProtocolContract {
            wire_epoch: 10,
            revisions: ProtocolRevisionRange::new(1, 1),
            capabilities: default_supported_capabilities(),
        };
        let server = ProtocolContract {
            wire_epoch: 11,
            revisions: ProtocolRevisionRange::new(1, 1),
            capabilities: default_supported_capabilities(),
        };

        let error = negotiate_protocol(&client, &server, CORE_PROTOCOL_CAPABILITIES)
            .expect_err("wire mismatch should fail");
        assert!(matches!(
            error,
            IncompatibilityReason::WireEpochMismatch {
                client: 10,
                server: 11,
            }
        ));
    }

    #[test]
    fn negotiate_protocol_rejects_missing_core_capability() {
        let client = ProtocolContract {
            wire_epoch: CURRENT_WIRE_EPOCH,
            revisions: ProtocolRevisionRange::new(1, 1),
            capabilities: vec![CORE_CAPABILITY_SESSION.to_string()],
        };
        let server = ProtocolContract {
            wire_epoch: CURRENT_WIRE_EPOCH,
            revisions: ProtocolRevisionRange::new(1, 1),
            capabilities: vec![CORE_CAPABILITY_SESSION.to_string()],
        };

        let error = negotiate_protocol(&client, &server, CORE_PROTOCOL_CAPABILITIES)
            .expect_err("missing core capabilities should fail");
        assert!(matches!(
            error,
            IncompatibilityReason::MissingCoreCapabilities { missing }
                if missing.contains(&CORE_CAPABILITY_ATTACH.to_string())
        ));
    }

    #[test]
    fn endpoint_helpers_report_correct_transport() {
        let unix_endpoint = IpcEndpoint::unix_socket("/tmp/bmux.sock");
        assert_eq!(
            unix_endpoint.as_unix_socket(),
            Some(Path::new("/tmp/bmux.sock"))
        );
        assert_eq!(unix_endpoint.as_windows_named_pipe(), None);

        let pipe_endpoint = IpcEndpoint::windows_named_pipe(r"\\.\pipe\bmux-test");
        assert_eq!(pipe_endpoint.as_unix_socket(), None);
        assert_eq!(
            pipe_endpoint.as_windows_named_pipe(),
            Some(r"\\.\pipe\bmux-test")
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_endpoint_exposes_socket_path() {
        let endpoint = IpcEndpoint::unix_socket("/tmp/bmux.sock");
        assert_eq!(endpoint.as_unix_socket(), Some(Path::new("/tmp/bmux.sock")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_endpoint_exposes_pipe_name() {
        let endpoint = IpcEndpoint::windows_named_pipe(r"\\.\pipe\bmux-test");
        assert_eq!(
            endpoint.as_windows_named_pipe(),
            Some(r"\\.\pipe\bmux-test")
        );
    }

    // ── Helper: assert encode/decode roundtrip ───────────────────────────────

    fn assert_roundtrip<T>(value: &T)
    where
        T: std::fmt::Debug + PartialEq + serde::Serialize + serde::de::DeserializeOwned,
    {
        let bytes = encode(value).unwrap_or_else(|e| panic!("encode failed: {e}"));
        let decoded: T = decode(&bytes).unwrap_or_else(|e| panic!("decode failed: {e}"));
        assert_eq!(&decoded, value);
    }

    // ── Level 1A: Exhaustive Request variant round-trips ─────────────────────

    #[test]
    #[allow(clippy::too_many_lines)]
    fn request_all_variants_roundtrip() {
        let id = Uuid::from_u128(1);

        let variants: Vec<Request> = vec![
            Request::Hello {
                protocol_version: ProtocolVersion::current(),
                client_name: "test-client".into(),
                principal_id: id,
            },
            Request::HelloV2 {
                contract: ProtocolContract::current(default_supported_capabilities()),
                client_name: "test-client-v2".into(),
                principal_id: id,
            },
            Request::Ping,
            Request::WhoAmIPrincipal,
            Request::ServerStatus,
            Request::ServerStop,
            Request::InvokeService {
                capability: "bmux.storage".into(),
                kind: InvokeServiceKind::Query,
                interface_id: "storage-query/v1".into(),
                operation: "get".into(),
                payload: vec![1, 2, 3],
            },
            Request::SubscribeEvents,
            Request::PollEvents { max_events: 100 },
        ];

        for (i, variant) in variants.iter().enumerate() {
            let bytes = encode(variant)
                .unwrap_or_else(|e| panic!("Request variant {i} encode failed: {e}"));
            let decoded: Request =
                decode(&bytes).unwrap_or_else(|e| panic!("Request variant {i} decode failed: {e}"));
            assert_eq!(&decoded, variant, "Request variant {i} roundtrip mismatch");
        }
    }

    // ── Level 1B: Exhaustive ResponsePayload variant round-trips ─────────────

    fn sample_recording_summary() -> RecordingSummary {
        RecordingSummary {
            id: Uuid::from_u128(100),
            name: Some("demo-recording".into()),
            format_version: RECORDING_FORMAT_VERSION,
            session_id: Some(Uuid::from_u128(1)),
            capture_input: true,
            profile: RecordingProfile::Full,
            event_kinds: vec![
                RecordingEventKind::PaneInputRaw,
                RecordingEventKind::PaneOutputRaw,
                RecordingEventKind::ServerEvent,
            ],
            started_epoch_ms: 1_700_000_000_000,
            ended_epoch_ms: Some(1_700_000_060_000),
            event_count: 42,
            payload_bytes: 123_456,
            path: "/tmp/recordings/test.bmux".into(),
            segments: vec!["events_0.bin".to_string()],
            total_segment_bytes: 123_456,
        }
    }

    const fn sample_performance_runtime_settings() -> PerformanceRuntimeSettings {
        PerformanceRuntimeSettings {
            recording_level: PerformanceRecordingLevel::Detailed,
            window_ms: 1_000,
            max_events_per_sec: 64,
            max_payload_bytes_per_sec: 131_072,
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn response_payload_all_variants_roundtrip() {
        let id = Uuid::from_u128(1);
        let id2 = Uuid::from_u128(2);

        let variants: Vec<ResponsePayload> = vec![
            ResponsePayload::Pong,
            ResponsePayload::PrincipalIdentity {
                principal_id: id,
                server_control_principal_id: id2,
                force_local_permitted: true,
            },
            ResponsePayload::ServerStatus {
                running: true,
                principal_id: id,
                server_control_principal_id: id2,
            },
            ResponsePayload::EventsSubscribed,
            ResponsePayload::EventBatch {
                events: vec![Event::ServerStarted],
            },
            ResponsePayload::ServerStopping,
            ResponsePayload::ServiceInvoked {
                payload: vec![9, 8, 7],
            },
        ];

        for (i, variant) in variants.iter().enumerate() {
            let response = Response::Ok(variant.clone());
            let bytes = encode(&response)
                .unwrap_or_else(|e| panic!("ResponsePayload variant {i} encode failed: {e}"));
            let decoded: Response = decode(&bytes)
                .unwrap_or_else(|e| panic!("ResponsePayload variant {i} decode failed: {e}"));
            assert_eq!(
                decoded, response,
                "ResponsePayload variant {i} roundtrip mismatch"
            );
        }
    }

    // ── Level 1C: Response::Err, all Event variants, all ErrorCode variants ──

    #[test]
    fn response_err_roundtrip() {
        let response = Response::Err(ErrorResponse {
            code: ErrorCode::NotFound,
            message: "session not found".into(),
        });
        assert_roundtrip(&response);
    }

    #[test]
    fn error_code_all_variants_roundtrip() {
        let codes = [
            ErrorCode::NotFound,
            ErrorCode::AlreadyExists,
            ErrorCode::InvalidRequest,
            ErrorCode::VersionMismatch,
            ErrorCode::Timeout,
            ErrorCode::Internal,
        ];
        for code in &codes {
            assert_roundtrip(code);
        }
    }

    #[test]
    fn event_all_variants_roundtrip() {
        let variants: Vec<Event> = vec![Event::ServerStarted, Event::ServerStopping];

        for (i, variant) in variants.iter().enumerate() {
            let bytes =
                encode(variant).unwrap_or_else(|e| panic!("Event variant {i} encode failed: {e}"));
            let decoded: Event =
                decode(&bytes).unwrap_or_else(|e| panic!("Event variant {i} decode failed: {e}"));
            assert_eq!(&decoded, variant, "Event variant {i} roundtrip mismatch");
        }
    }

    // ── Level 1D: Recording types round-trips ────────────────────────────────

    #[test]
    fn recording_profile_all_variants_roundtrip() {
        for profile in &[
            RecordingProfile::Full,
            RecordingProfile::Functional,
            RecordingProfile::Visual,
        ] {
            assert_roundtrip(profile);
        }
    }

    #[test]
    fn performance_recording_level_all_variants_roundtrip() {
        for level in &[
            PerformanceRecordingLevel::Off,
            PerformanceRecordingLevel::Basic,
            PerformanceRecordingLevel::Detailed,
            PerformanceRecordingLevel::Trace,
        ] {
            assert_roundtrip(level);
        }
    }

    #[test]
    fn performance_runtime_settings_roundtrip() {
        assert_roundtrip(&sample_performance_runtime_settings());
    }

    #[test]
    fn recording_event_kind_all_variants_roundtrip() {
        let kinds = [
            RecordingEventKind::PaneInputRaw,
            RecordingEventKind::PaneOutputRaw,
            RecordingEventKind::ProtocolReplyRaw,
            RecordingEventKind::ServerEvent,
            RecordingEventKind::RequestStart,
            RecordingEventKind::RequestDone,
            RecordingEventKind::RequestError,
            RecordingEventKind::Custom,
        ];
        for kind in &kinds {
            assert_roundtrip(kind);
        }
    }

    #[test]
    fn recording_summary_roundtrip() {
        assert_roundtrip(&sample_recording_summary());
    }

    #[test]
    fn recording_payload_all_variants_roundtrip() {
        let payloads: Vec<RecordingPayload> = vec![
            RecordingPayload::Bytes {
                data: vec![1, 2, 3, 4, 5],
            },
            RecordingPayload::Bytes { data: vec![] },
            RecordingPayload::ServerEvent {
                event: Event::ServerStarted,
            },
            RecordingPayload::RequestStart {
                request_id: 42,
                request_kind: "ping".into(),
                exclusive: false,
                request_data: vec![0, 1],
            },
            RecordingPayload::RequestDone {
                request_id: 42,
                request_kind: "ping".into(),
                response_kind: "pong".into(),
                elapsed_ms: 5,
                request_data: vec![0, 1],
                response_data: vec![2, 3],
            },
            RecordingPayload::RequestError {
                request_id: 43,
                request_kind: "kill_session".into(),
                error_code: ErrorCode::NotFound,
                message: "session not found".into(),
                elapsed_ms: 2,
            },
            RecordingPayload::Custom {
                source: "test-plugin".into(),
                name: "custom-event".into(),
                payload: b"{\"ok\":true}".to_vec(),
            },
        ];

        for (i, payload) in payloads.iter().enumerate() {
            let bytes = encode(payload)
                .unwrap_or_else(|e| panic!("RecordingPayload variant {i} encode failed: {e}"));
            let decoded: RecordingPayload = decode(&bytes)
                .unwrap_or_else(|e| panic!("RecordingPayload variant {i} decode failed: {e}"));
            assert_eq!(&decoded, payload, "RecordingPayload variant {i} mismatch");
        }
    }

    #[test]
    fn recording_event_envelope_roundtrip() {
        let envelope = RecordingEventEnvelope {
            seq: 1,
            mono_ns: 1_000_000,
            wall_epoch_ms: 1_700_000_000_000,
            session_id: Some(Uuid::from_u128(1)),
            pane_id: Some(Uuid::from_u128(2)),
            client_id: Some(Uuid::from_u128(3)),
            kind: RecordingEventKind::RequestDone,
            payload: RecordingPayload::RequestDone {
                request_id: 7,
                request_kind: "attach".into(),
                response_kind: "attached".into(),
                elapsed_ms: 12,
                request_data: vec![1, 2, 3],
                response_data: vec![4, 5, 6],
            },
        };
        assert_roundtrip(&envelope);
    }

    #[test]
    fn recording_event_envelope_with_none_ids_roundtrip() {
        let envelope = RecordingEventEnvelope {
            seq: 0,
            mono_ns: 0,
            wall_epoch_ms: 0,
            session_id: None,
            pane_id: None,
            client_id: None,
            kind: RecordingEventKind::Custom,
            payload: RecordingPayload::Bytes { data: vec![255] },
        };
        assert_roundtrip(&envelope);
    }

    #[test]
    fn recording_event_envelope_write_frame_read_frames_roundtrip() {
        let envelopes = vec![
            RecordingEventEnvelope {
                seq: 0,
                mono_ns: 1000,
                wall_epoch_ms: 1_700_000_000_000,
                session_id: Some(Uuid::from_u128(1)),
                pane_id: None,
                client_id: None,
                kind: RecordingEventKind::PaneOutputRaw,
                payload: RecordingPayload::Bytes {
                    data: vec![65, 66, 67],
                },
            },
            RecordingEventEnvelope {
                seq: 1,
                mono_ns: 2000,
                wall_epoch_ms: 1_700_000_000_001,
                session_id: Some(Uuid::from_u128(1)),
                pane_id: Some(Uuid::from_u128(2)),
                client_id: None,
                kind: RecordingEventKind::ServerEvent,
                payload: RecordingPayload::ServerEvent {
                    event: Event::ServerStarted,
                },
            },
        ];

        let mut buf = Vec::new();
        for env in &envelopes {
            write_frame(&mut buf, env).expect("write_frame should succeed");
        }

        let result =
            read_frames::<RecordingEventEnvelope>(&buf).expect("read_frames should succeed");
        assert_eq!(result.frames, envelopes);
        assert_eq!(result.bytes_remaining, 0);
    }

    // ── Level 1E: DisplayTrack types round-trips ─────────────────────────────

    #[test]
    fn display_track_event_all_variants_roundtrip() {
        let variants: Vec<DisplayTrackEvent> = vec![
            DisplayTrackEvent::StreamOpened {
                client_id: Uuid::from_u128(1),
                recording_id: Uuid::from_u128(2),
                cell_width_px: Some(8),
                cell_height_px: Some(16),
                window_width_px: Some(1920),
                window_height_px: Some(1080),
                terminal_profile: Some(vec![10, 20, 30]),
            },
            DisplayTrackEvent::StreamOpened {
                client_id: Uuid::from_u128(1),
                recording_id: Uuid::from_u128(2),
                cell_width_px: None,
                cell_height_px: None,
                window_width_px: None,
                window_height_px: None,
                terminal_profile: None,
            },
            DisplayTrackEvent::Resize {
                cols: 120,
                rows: 40,
            },
            DisplayTrackEvent::FrameBytes {
                data: vec![27, 91, 72],
            },
            DisplayTrackEvent::CursorSnapshot {
                x: 5,
                y: 7,
                visible: true,
                shape: DisplayCursorShape::Bar,
                blink_enabled: false,
            },
            DisplayTrackEvent::Activity {
                kind: DisplayActivityKind::Input,
            },
            DisplayTrackEvent::FrameBytes { data: vec![] },
            DisplayTrackEvent::ImageUpdate {
                images: vec![AttachPaneImage {
                    id: 42,
                    protocol: bmux_attach_image_protocol::AttachImageProtocol::Sixel,
                    raw_data: vec![0x1b, 0x50, 0x71],
                    compression: CompressionId::None,
                    position_row: 3,
                    position_col: 5,
                    cell_rows: 10,
                    cell_cols: 20,
                    pixel_width: 160,
                    pixel_height: 80,
                }],
            },
            DisplayTrackEvent::ImageUpdate { images: vec![] },
            DisplayTrackEvent::StreamClosed,
        ];

        for (i, variant) in variants.iter().enumerate() {
            let envelope = DisplayTrackEnvelope {
                mono_ns: (i as u64) * 1000,
                event: variant.clone(),
            };
            assert_roundtrip(&envelope);
        }
    }

    #[test]
    fn display_track_write_frame_read_frames_roundtrip() {
        let envelopes = vec![
            DisplayTrackEnvelope {
                mono_ns: 0,
                event: DisplayTrackEvent::StreamOpened {
                    client_id: Uuid::from_u128(1),
                    recording_id: Uuid::from_u128(2),
                    cell_width_px: Some(8),
                    cell_height_px: Some(16),
                    window_width_px: None,
                    window_height_px: None,
                    terminal_profile: None,
                },
            },
            DisplayTrackEnvelope {
                mono_ns: 1000,
                event: DisplayTrackEvent::Resize { cols: 80, rows: 24 },
            },
            DisplayTrackEnvelope {
                mono_ns: 2000,
                event: DisplayTrackEvent::FrameBytes {
                    data: vec![65; 100],
                },
            },
            DisplayTrackEnvelope {
                mono_ns: 3000,
                event: DisplayTrackEvent::CursorSnapshot {
                    x: 10,
                    y: 11,
                    visible: true,
                    shape: DisplayCursorShape::Block,
                    blink_enabled: true,
                },
            },
            DisplayTrackEnvelope {
                mono_ns: 3500,
                event: DisplayTrackEvent::Activity {
                    kind: DisplayActivityKind::Output,
                },
            },
            DisplayTrackEnvelope {
                mono_ns: 3600,
                event: DisplayTrackEvent::StreamClosed,
            },
        ];

        let mut buf = Vec::new();
        for env in &envelopes {
            write_frame(&mut buf, env).expect("write_frame should succeed");
        }

        let result = read_frames::<DisplayTrackEnvelope>(&buf).expect("read_frames should succeed");
        assert_eq!(result.frames, envelopes);
        assert_eq!(result.bytes_remaining, 0);
    }
}
