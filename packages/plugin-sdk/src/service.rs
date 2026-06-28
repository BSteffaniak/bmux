use crate::{CapabilityId, HostScope, InterfaceId, PluginError, PluginInvocationId, Result};
use bmux_perf_telemetry::{PhaseChannel, PhasePayload, emit as emit_phase_timing};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub const SERVICE_STREAM_MAGIC_V1: &[u8] = b"BMUXSTR1";

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

/// BPDL-generated descriptor for an interface-level service endpoint.
///
/// Unlike [`PluginService`], this is cheap to expose from generated Rust
/// bindings because every field is a typed static identifier. Hosts convert it
/// into a [`PluginService`] when building a plugin declaration.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServiceInterfaceDescriptor {
    pub capability: CapabilityId,
    pub kind: ServiceKind,
    pub interface_id: InterfaceId,
}

/// Type-level plugin contract exported by BPDL-generated API crates.
///
/// Rust plugin implementations associate themselves with one contract type via
/// `RustPlugin::Contract`. Hosts then derive generated services from that type,
/// so providers do not manually list BPDL services in manifests or plugin impls.
pub trait PluginContract {
    /// Return service declarations generated from the plugin contract.
    ///
    /// # Errors
    ///
    /// Returns if any generated capability cannot be represented as a host scope.
    fn service_declarations() -> Result<Vec<PluginService>>;
}

/// Explicit marker for plugins that do not have a BPDL contract.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoPluginContract;

impl PluginContract for NoPluginContract {
    fn service_declarations() -> Result<Vec<PluginService>> {
        Ok(Vec::new())
    }
}

impl ServiceInterfaceDescriptor {
    /// Convert this generated descriptor into a manifest-compatible service
    /// declaration.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::InvalidCapabilityId`] if the generated capability
    /// cannot be represented as a host scope. This should only happen for
    /// invalid BPDL input or hand-written descriptors.
    pub fn to_plugin_service(&self) -> Result<PluginService> {
        Ok(PluginService {
            capability: HostScope::new(self.capability.as_str())?,
            kind: self.kind,
            interface_id: self.interface_id.as_str().to_string(),
        })
    }
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
    #[serde(default)]
    pub invocation_id: PluginInvocationId,
    pub request_id: u64,
    pub kind: ServiceEnvelopeKind,
    #[serde(with = "bmux_codec::serde_bytes_vec")]
    pub payload: Vec<u8>,
}

impl ServiceEnvelope {
    #[must_use]
    pub fn new(request_id: u64, kind: ServiceEnvelopeKind, payload: Vec<u8>) -> Self {
        Self::with_invocation_id(PluginInvocationId::new(), request_id, kind, payload)
    }

    #[must_use]
    pub const fn with_invocation_id(
        invocation_id: PluginInvocationId,
        request_id: u64,
        kind: ServiceEnvelopeKind,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            version: ServiceProtocolVersion::current(),
            invocation_id,
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
    #[serde(with = "bmux_codec::serde_bytes_vec")]
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceResponse {
    #[serde(with = "bmux_codec::serde_bytes_vec")]
    pub payload: Vec<u8>,
    pub error: Option<ServiceError>,
}

/// Typed-stable service event frame emitted during a streaming invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceEvent {
    pub invocation_id: PluginInvocationId,
    #[serde(with = "bmux_codec::serde_bytes_vec")]
    pub payload: Vec<u8>,
}

/// Sink for request-scoped service event frames.
pub trait ServiceEventSink: Send + Sync {
    /// Emit one event frame for the current service invocation.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::ServiceProtocol`] when the host cannot accept the event.
    fn emit_event(&self, event: ServiceEvent) -> Result<()>;
}

/// Shared service event sink handle.
#[derive(Clone)]
pub struct ServiceEventSinkHandle(Arc<dyn ServiceEventSink>);

impl ServiceEventSinkHandle {
    #[must_use]
    pub fn new<S: ServiceEventSink + 'static>(sink: S) -> Self {
        Self(Arc::new(sink))
    }

    #[must_use]
    pub fn from_arc(sink: Arc<dyn ServiceEventSink>) -> Self {
        Self(sink)
    }

    #[must_use]
    pub fn noop() -> Self {
        Self::new(NoopServiceEventSink)
    }

    /// Emit one event frame for the current service invocation.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::ServiceProtocol`] when the host cannot accept the event.
    pub fn emit_event(&self, event: ServiceEvent) -> Result<()> {
        self.0.emit_event(event)
    }
}

impl std::fmt::Debug for ServiceEventSinkHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServiceEventSinkHandle")
            .finish_non_exhaustive()
    }
}

/// No-op service event sink used by non-streaming fallback paths.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopServiceEventSink;

impl ServiceEventSink for NoopServiceEventSink {
    fn emit_event(&self, _event: ServiceEvent) -> Result<()> {
        Ok(())
    }
}

/// Bounded in-memory event sink for host streaming dispatch.
#[derive(Debug, Clone)]
pub struct BoundedServiceEventSink {
    capacity: usize,
    events: Arc<Mutex<Vec<ServiceEvent>>>,
}

impl BoundedServiceEventSink {
    /// Create a bounded sink that accepts at most `capacity` events.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Drain accepted events in emission order.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::ServiceProtocol`] if the internal event queue lock is poisoned.
    pub fn drain(&self) -> Result<Vec<ServiceEvent>> {
        let mut events = self
            .events
            .lock()
            .map_err(|_| PluginError::ServiceProtocol {
                details: "service event queue lock poisoned".to_string(),
            })?;
        Ok(std::mem::take(&mut *events))
    }
}

impl ServiceEventSink for BoundedServiceEventSink {
    fn emit_event(&self, event: ServiceEvent) -> Result<()> {
        let mut events = self
            .events
            .lock()
            .map_err(|_| PluginError::ServiceProtocol {
                details: "service event queue lock poisoned".to_string(),
            })?;
        if events.len() >= self.capacity {
            return Err(PluginError::ServiceProtocol {
                details: format!(
                    "service event queue overflow: capacity {} exceeded",
                    self.capacity
                ),
            });
        }
        events.push(event);
        drop(events);
        Ok(())
    }
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

/// Serialize a service message using canonical typed-stable `bmux_codec` bytes.
///
/// Legacy positional bytes are accepted by [`decode_service_message`] for
/// migration compatibility, but new service messages are always encoded with
/// the typed-stable codec.
///
/// # Errors
///
/// Returns [`PluginError::ServiceProtocol`] if serialization fails.
pub fn encode_service_message<T>(message: &T) -> Result<Vec<u8>>
where
    T: Serialize,
{
    let timing_enabled = PhaseChannel::Service.enabled();
    let started_at = timing_enabled.then(Instant::now);
    let result = bmux_codec::to_typed_vec(message).map_err(|error| PluginError::ServiceProtocol {
        details: error.to_string(),
    });
    if let Some(started_at) = started_at {
        emit_service_codec_timing::<T>("typed_service.message_encode", started_at, &result);
    }
    result
}

/// Deserialize a service message from canonical typed-stable `bmux_codec` bytes.
///
/// Legacy positional `bmux_codec` bytes are decoded as a migration
/// compatibility fallback so old in-repo fixtures and plugins can be migrated
/// without a second public transport API.
///
/// # Errors
///
/// Returns [`PluginError::ServiceProtocol`] if deserialization fails.
pub fn decode_service_message<T>(payload: &[u8]) -> Result<T>
where
    T: DeserializeOwned,
{
    let timing_enabled = PhaseChannel::Service.enabled();
    let started_at = timing_enabled.then(Instant::now);
    let result = match bmux_codec::from_typed_bytes(payload) {
        Ok(message) => Ok(message),
        Err(typed_error) => bmux_codec::from_positional_bytes(payload).map_err(|positional_error| {
            PluginError::ServiceProtocol {
                details: format!(
                    "typed-stable decode failed: {typed_error}; legacy positional decode failed: {positional_error}",
                ),
            }
        }),
    };
    if let Some(started_at) = started_at {
        let total_us = started_at.elapsed().as_micros();
        let payload = PhasePayload::new("typed_service.message_decode")
            .field("type_name", std::any::type_name::<T>())
            .field("input_bytes", payload.len())
            .field("ok", result.is_ok())
            .field("total_us", total_us)
            .finish();
        emit_phase_timing(PhaseChannel::Service, &payload);
    }
    result
}

fn emit_service_codec_timing<T>(phase: &str, started_at: Instant, result: &Result<Vec<u8>>)
where
    T: ?Sized,
{
    let total_us = started_at.elapsed().as_micros();
    let output_bytes = result.as_ref().map_or(0, Vec::len);
    let payload = PhasePayload::new(phase)
        .field("type_name", std::any::type_name::<T>())
        .field("output_bytes", output_bytes)
        .field("ok", result.is_ok())
        .field("total_us", total_us)
        .finish();
    emit_phase_timing(PhaseChannel::Service, &payload);
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
    encode_service_envelope_with_invocation_id(PluginInvocationId::new(), request_id, kind, message)
}

/// Encode a typed message into a service envelope with an explicit invocation ID.
///
/// # Errors
///
/// Returns [`PluginError::ServiceProtocol`] if serialization of the message
/// or the envelope fails.
pub fn encode_service_envelope_with_invocation_id<T>(
    invocation_id: PluginInvocationId,
    request_id: u64,
    kind: ServiceEnvelopeKind,
    message: &T,
) -> Result<Vec<u8>>
where
    T: Serialize,
{
    encode_service_message(&ServiceEnvelope::with_invocation_id(
        invocation_id,
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
    let (_invocation_id, request_id, message) =
        decode_service_envelope_with_invocation_id(payload, expected_kind)?;
    Ok((request_id, message))
}

/// Decode a service envelope and extract its invocation ID and typed payload.
///
/// # Errors
///
/// Returns [`PluginError::ServiceProtocol`] if the envelope cannot be
/// deserialized, the protocol version is unsupported, the envelope kind
/// does not match `expected_kind`, or the inner payload fails to deserialize.
pub fn decode_service_envelope_with_invocation_id<T>(
    payload: &[u8],
    expected_kind: ServiceEnvelopeKind,
) -> Result<(PluginInvocationId, u64, T)>
where
    T: DeserializeOwned,
{
    let envelope = match decode_service_message::<ServiceEnvelope>(payload) {
        Ok(envelope) => envelope,
        Err(current_error) => {
            #[derive(Deserialize)]
            struct LegacyServiceEnvelope {
                version: ServiceProtocolVersion,
                request_id: u64,
                kind: ServiceEnvelopeKind,
                #[serde(with = "bmux_codec::serde_bytes_vec")]
                payload: Vec<u8>,
            }

            let legacy: LegacyServiceEnvelope = bmux_codec::from_positional_bytes(payload)
                .map_err(|legacy_error| PluginError::ServiceProtocol {
                    details: format!(
                        "current envelope decode failed: {current_error}; legacy envelope decode failed: {legacy_error}",
                    ),
                })?;
            ServiceEnvelope {
                version: legacy.version,
                invocation_id: PluginInvocationId::new(),
                request_id: legacy.request_id,
                kind: legacy.kind,
                payload: legacy.payload,
            }
        }
    };
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
        envelope.invocation_id,
        envelope.request_id,
        decode_service_message::<T>(&envelope.payload)?,
    ))
}

/// Encode service stream frames as versioned length-prefixed typed-stable envelopes.
///
/// The stream format is `BMUXSTR1` followed by repeated `u32` big-endian frame
/// lengths and typed-stable [`ServiceEnvelope`] bytes. Each event frame uses
/// [`ServiceEnvelopeKind::Event`]; exactly one final frame should use
/// [`ServiceEnvelopeKind::Response`].
///
/// # Errors
///
/// Returns [`PluginError::ServiceProtocol`] if any envelope cannot be encoded or
/// a frame exceeds `u32::MAX` bytes.
pub fn encode_service_stream_envelopes(envelopes: &[ServiceEnvelope]) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    output.extend_from_slice(SERVICE_STREAM_MAGIC_V1);
    for envelope in envelopes {
        let frame = encode_service_message(envelope)?;
        let frame_len = u32::try_from(frame.len()).map_err(|_| PluginError::ServiceProtocol {
            details: "service stream frame too large".to_string(),
        })?;
        output.extend_from_slice(&frame_len.to_be_bytes());
        output.extend_from_slice(&frame);
    }
    Ok(output)
}

/// Maximum encoded streaming response frame size accepted by canonical helpers.
pub const SERVICE_STREAM_MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Decode versioned length-prefixed typed-stable service stream envelopes.
///
/// # Errors
///
/// Returns [`PluginError::ServiceProtocol`] when the stream prefix, frame length,
/// or envelope bytes are invalid.
pub fn decode_service_stream_envelopes(bytes: &[u8]) -> Result<Vec<ServiceEnvelope>> {
    if !bytes.starts_with(SERVICE_STREAM_MAGIC_V1) {
        return Err(PluginError::ServiceProtocol {
            details: "service stream output missing BMUXSTR1 frame prefix".to_string(),
        });
    }

    let mut offset = SERVICE_STREAM_MAGIC_V1.len();
    let mut envelopes = Vec::new();
    while offset < bytes.len() {
        let remaining = bytes.len() - offset;
        if remaining < 4 {
            return Err(PluginError::ServiceProtocol {
                details: "service stream output truncated frame length".to_string(),
            });
        }
        let mut len_buf = [0_u8; 4];
        len_buf.copy_from_slice(&bytes[offset..offset + 4]);
        offset += 4;
        let frame_len = usize::try_from(u32::from_be_bytes(len_buf)).map_err(|_| {
            PluginError::ServiceProtocol {
                details: "service stream frame length conversion failed".to_string(),
            }
        })?;
        if frame_len > SERVICE_STREAM_MAX_FRAME_BYTES {
            return Err(PluginError::ServiceProtocol {
                details: format!(
                    "service stream frame too large: {frame_len} bytes exceeds {SERVICE_STREAM_MAX_FRAME_BYTES}"
                ),
            });
        }
        if bytes.len() < offset + frame_len {
            return Err(PluginError::ServiceProtocol {
                details: "service stream output truncated frame payload".to_string(),
            });
        }
        envelopes.push(decode_service_message(&bytes[offset..offset + frame_len])?);
        offset += frame_len;
    }
    Ok(envelopes)
}

#[cfg(test)]
mod tests {
    use super::{
        BoundedServiceEventSink, ProviderId, RegisteredService, SERVICE_STREAM_MAGIC_V1,
        SERVICE_STREAM_MAX_FRAME_BYTES, ServiceEnvelope, ServiceEnvelopeKind, ServiceError,
        ServiceEvent, ServiceEventSink, ServiceKind, ServiceRequest, ServiceResponse,
        decode_service_envelope, decode_service_message, decode_service_stream_envelopes,
        encode_service_envelope, encode_service_message, encode_service_stream_envelopes,
    };
    use crate::HostScope;

    #[test]
    fn service_message_roundtrip() {
        let response = ServiceResponse::ok(vec![1, 2, 3]);
        let bytes = encode_service_message(&response).expect("service response should encode");
        bmux_codec::from_typed_bytes::<ServiceResponse>(&bytes)
            .expect("service response should use typed-stable encoding");
        let decoded: ServiceResponse =
            decode_service_message(&bytes).expect("service response should decode");
        assert_eq!(decoded, response);
    }

    #[test]
    fn legacy_positional_response_message_still_decodes() {
        let legacy_bytes = [3, 1, 2, 3, 0];
        let decoded: ServiceResponse = decode_service_message(&legacy_bytes)
            .expect("legacy positional service response should decode");
        assert_eq!(decoded, ServiceResponse::ok(vec![1, 2, 3]));
    }

    #[test]
    fn service_envelope_roundtrip() {
        let request = ServiceRequest {
            caller_plugin_id: "example.native".to_string(),
            service: RegisteredService {
                capability: HostScope::new("bmux.storage.read").expect("capability should parse"),
                kind: ServiceKind::Query,
                interface_id: "storage-state".to_string(),
                provider: ProviderId::Plugin("bmux.storage".to_string()),
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
    fn service_envelope_preserves_explicit_invocation_id() {
        let invocation_id = crate::PluginInvocationId::new();
        let response = ServiceResponse::ok(vec![7, 8, 9]);
        let bytes = crate::encode_service_envelope_with_invocation_id(
            invocation_id.clone(),
            99,
            ServiceEnvelopeKind::Response,
            &response,
        )
        .expect("response envelope should encode");
        let (decoded_invocation_id, request_id, decoded): (
            crate::PluginInvocationId,
            u64,
            ServiceResponse,
        ) = crate::decode_service_envelope_with_invocation_id(
            &bytes,
            ServiceEnvelopeKind::Response,
        )
        .expect("response envelope should decode");
        assert_eq!(decoded_invocation_id, invocation_id);
        assert_eq!(request_id, 99);
        assert_eq!(decoded, response);
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
    fn legacy_positional_response_envelope_still_decodes() {
        let legacy_bytes = [1, 99, 1, 5, 3, 7, 8, 9, 0];
        let (request_id, decoded): (u64, ServiceResponse) =
            decode_service_envelope(&legacy_bytes, ServiceEnvelopeKind::Response)
                .expect("legacy positional response envelope should decode");
        assert_eq!(request_id, 99);
        assert_eq!(decoded, ServiceResponse::ok(vec![7, 8, 9]));
    }

    #[test]
    fn service_stream_envelopes_are_length_prefixed_and_ordered() {
        let invocation_id = crate::PluginInvocationId::new();
        let event = ServiceEnvelope::with_invocation_id(
            invocation_id.clone(),
            7,
            ServiceEnvelopeKind::Event,
            encode_service_message(&ServiceEvent {
                invocation_id: invocation_id.clone(),
                payload: vec![1, 2, 3],
            })
            .expect("event should encode"),
        );
        let response = ServiceEnvelope::with_invocation_id(
            invocation_id.clone(),
            7,
            ServiceEnvelopeKind::Response,
            encode_service_message(&ServiceResponse::ok(vec![4, 5, 6]))
                .expect("response should encode"),
        );
        let stream = encode_service_stream_envelopes(&[event.clone(), response.clone()])
            .expect("stream should encode");
        let decoded = decode_service_stream_envelopes(&stream).expect("stream should decode");
        assert_eq!(decoded, vec![event, response]);
        assert!(matches!(decoded[0].kind, ServiceEnvelopeKind::Event));
        assert!(matches!(decoded[1].kind, ServiceEnvelopeKind::Response));
        assert_eq!(decoded[0].invocation_id, invocation_id);
        assert_eq!(decoded[1].invocation_id, invocation_id);
    }

    #[test]
    fn bounded_service_event_sink_preserves_order_and_rejects_overflow() {
        let invocation_id = crate::PluginInvocationId::new();
        let sink = BoundedServiceEventSink::new(2);
        sink.emit_event(ServiceEvent {
            invocation_id: invocation_id.clone(),
            payload: vec![1],
        })
        .expect("first event should fit");
        sink.emit_event(ServiceEvent {
            invocation_id: invocation_id.clone(),
            payload: vec![2],
        })
        .expect("second event should fit");
        let error = sink
            .emit_event(ServiceEvent {
                invocation_id,
                payload: vec![3],
            })
            .expect_err("third event should overflow");
        assert!(error.to_string().contains("service event queue overflow"));
        let drained = sink.drain().expect("events should drain");
        assert_eq!(drained[0].payload, vec![1]);
        assert_eq!(drained[1].payload, vec![2]);
    }

    #[test]
    fn service_stream_rejects_oversized_frame_length() {
        let mut stream = Vec::new();
        stream.extend_from_slice(SERVICE_STREAM_MAGIC_V1);
        stream.extend_from_slice(
            &u32::try_from(SERVICE_STREAM_MAX_FRAME_BYTES + 1)
                .expect("limit fits u32")
                .to_be_bytes(),
        );
        let error = decode_service_stream_envelopes(&stream).expect_err("oversized frame fails");
        assert!(error.to_string().contains("service stream frame too large"));
    }

    #[test]
    fn registered_service_with_host_provider_roundtrip() {
        let service = RegisteredService {
            capability: HostScope::new("bmux.storage").expect("capability should parse"),
            kind: ServiceKind::Command,
            interface_id: "storage-command/v1".to_string(),
            provider: ProviderId::Host,
        };
        let bytes = encode_service_message(&service).expect("registered service should encode");
        let decoded: RegisteredService =
            decode_service_message(&bytes).expect("registered service should decode");
        assert_eq!(decoded, service);
    }
}
