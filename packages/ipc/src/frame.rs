use crate::{Envelope, ProtocolVersion, decode, encode};
use thiserror::Error;

const FRAME_LEN_BYTES: usize = 4;

/// Maximum accepted encoded envelope payload size.
pub const MAX_FRAME_PAYLOAD_SIZE: usize = 1_048_576;

/// Errors returned by frame encoding.
#[derive(Debug, Error)]
pub enum FrameEncodeError {
    #[error(
        "frame payload exceeds max size ({actual} bytes > {max} bytes)",
        actual = .actual,
        max = .max
    )]
    PayloadTooLarge { actual: usize, max: usize },
    #[error("failed to serialize frame payload: {0}")]
    Serialize(#[from] postcard::Error),
}

/// Errors returned by frame decoding.
#[derive(Debug, Error)]
pub enum FrameDecodeError {
    #[error(
        "frame payload exceeds max size ({actual} bytes > {max} bytes)",
        actual = .actual,
        max = .max
    )]
    PayloadTooLarge { actual: usize, max: usize },
    #[error("incomplete frame data")]
    IncompleteFrame,
    #[error("frame contains trailing bytes")]
    TrailingBytes,
    #[error("unsupported protocol version {actual}; expected {expected}")]
    UnsupportedVersion { actual: u16, expected: u16 },
    #[error("failed to deserialize frame payload: {0}")]
    Deserialize(postcard::Error),
}

/// Encode an envelope into a length-prefixed frame.
///
/// Framing format:
/// - 4-byte little-endian payload length
/// - postcard-encoded envelope payload
///
/// # Errors
///
/// Returns an error if payload serialization fails or exceeds max size.
pub fn encode_frame(envelope: &Envelope) -> Result<Vec<u8>, FrameEncodeError> {
    let payload = encode(envelope)?;
    if payload.len() > MAX_FRAME_PAYLOAD_SIZE {
        return Err(FrameEncodeError::PayloadTooLarge {
            actual: payload.len(),
            max: MAX_FRAME_PAYLOAD_SIZE,
        });
    }

    let payload_len =
        u32::try_from(payload.len()).map_err(|_| FrameEncodeError::PayloadTooLarge {
            actual: payload.len(),
            max: MAX_FRAME_PAYLOAD_SIZE,
        })?;
    let mut frame = Vec::with_capacity(FRAME_LEN_BYTES + payload.len());
    frame.extend_from_slice(&payload_len.to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Decode one exact frame from bytes.
///
/// # Errors
///
/// Returns an error for truncated, malformed, oversized, trailing-byte,
/// unsupported-version, or deserialization failures.
pub fn decode_frame_exact(frame: &[u8]) -> Result<Envelope, FrameDecodeError> {
    let Some(payload_len) = frame_payload_len(frame)? else {
        return Err(FrameDecodeError::IncompleteFrame);
    };
    let total_len = FRAME_LEN_BYTES + payload_len;
    if frame.len() < total_len {
        return Err(FrameDecodeError::IncompleteFrame);
    }
    if frame.len() > total_len {
        return Err(FrameDecodeError::TrailingBytes);
    }

    decode_envelope_payload(&frame[FRAME_LEN_BYTES..total_len])
}

/// Try to decode one frame from a mutable input buffer.
///
/// Returns `Ok(None)` when the buffer does not yet contain a complete frame.
///
/// # Errors
///
/// Returns an error for malformed, oversized, unsupported-version, or
/// deserialization failures.
pub fn try_decode_frame(buffer: &mut Vec<u8>) -> Result<Option<Envelope>, FrameDecodeError> {
    let Some(payload_len) = frame_payload_len(buffer)? else {
        return Ok(None);
    };
    let total_len = FRAME_LEN_BYTES + payload_len;
    if buffer.len() < total_len {
        return Ok(None);
    }

    let payload = buffer[FRAME_LEN_BYTES..total_len].to_vec();
    buffer.drain(..total_len);
    decode_envelope_payload(&payload).map(Some)
}

fn frame_payload_len(bytes: &[u8]) -> Result<Option<usize>, FrameDecodeError> {
    if bytes.len() < FRAME_LEN_BYTES {
        return Ok(None);
    }

    let payload_len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    if payload_len > MAX_FRAME_PAYLOAD_SIZE {
        return Err(FrameDecodeError::PayloadTooLarge {
            actual: payload_len,
            max: MAX_FRAME_PAYLOAD_SIZE,
        });
    }
    Ok(Some(payload_len))
}

fn decode_envelope_payload(payload: &[u8]) -> Result<Envelope, FrameDecodeError> {
    let envelope: Envelope = decode(payload).map_err(FrameDecodeError::Deserialize)?;
    if envelope.version != ProtocolVersion::current() {
        return Err(FrameDecodeError::UnsupportedVersion {
            actual: envelope.version.0,
            expected: ProtocolVersion::current().0,
        });
    }
    Ok(envelope)
}

#[cfg(test)]
mod tests {
    use super::{
        FrameDecodeError, MAX_FRAME_PAYLOAD_SIZE, decode_frame_exact, encode_frame,
        try_decode_frame,
    };
    use crate::{Envelope, EnvelopeKind, ProtocolVersion, Request, encode};

    #[test]
    fn frame_roundtrip_exact() {
        let payload = encode(&Request::Ping).expect("request should encode");
        let envelope = Envelope::new(42, EnvelopeKind::Request, payload);
        let frame = encode_frame(&envelope).expect("frame should encode");
        let decoded = decode_frame_exact(&frame).expect("frame should decode");
        assert_eq!(decoded, envelope);
    }

    #[test]
    fn try_decode_returns_none_for_truncated_prefix() {
        let mut buffer = vec![0x02, 0x00, 0x00];
        let decoded = try_decode_frame(&mut buffer).expect("decode should not fail");
        assert!(decoded.is_none());
        assert_eq!(buffer.len(), 3);
    }

    #[test]
    fn decode_exact_rejects_truncated_payload() {
        let payload = encode(&Request::Ping).expect("request should encode");
        let envelope = Envelope::new(7, EnvelopeKind::Request, payload);
        let frame = encode_frame(&envelope).expect("frame should encode");
        let truncated = &frame[..frame.len() - 1];

        let error = decode_frame_exact(truncated).expect_err("expected incomplete frame error");
        assert!(matches!(error, FrameDecodeError::IncompleteFrame));
    }

    #[test]
    fn decode_rejects_oversized_payload_len() {
        let oversized = (MAX_FRAME_PAYLOAD_SIZE + 1) as u32;
        let mut buffer = oversized.to_le_bytes().to_vec();
        buffer.extend_from_slice(&[1, 2, 3]);

        let error = try_decode_frame(&mut buffer).expect_err("expected oversize error");
        assert!(matches!(error, FrameDecodeError::PayloadTooLarge { .. }));
    }

    #[test]
    fn decode_rejects_unknown_protocol_version() {
        let payload = encode(&Request::Ping).expect("request should encode");
        let mut envelope = Envelope::new(9, EnvelopeKind::Request, payload);
        envelope.version = ProtocolVersion(9999);
        let frame = encode_frame(&envelope).expect("frame should encode");

        let error = decode_frame_exact(&frame).expect_err("expected version mismatch");
        assert!(matches!(
            error,
            FrameDecodeError::UnsupportedVersion { actual: 9999, expected }
                if expected == crate::CURRENT_PROTOCOL_VERSION
        ));
    }
}
