use crate::compression::{self, CompressionCodec, CompressionHint};
use crate::{Envelope, ProtocolVersion, decode, encode};
use thiserror::Error;

const FRAME_LEN_BYTES: usize = 4;

/// Maximum accepted encoded envelope payload size (8 MiB).
///
/// This is the hard ceiling for a single IPC frame over the local Unix socket.
/// Server-side response assembly should enforce its own budget well below this
/// limit so that `PayloadTooLarge` is never hit in normal operation.
pub const MAX_FRAME_PAYLOAD_SIZE: usize = 8 * 1_048_576;

/// Maximum accepted compressed frame size on the wire (16 MiB).
///
/// Compressed payloads are smaller than their uncompressed form, but the wire
/// frame size limit must be larger than `MAX_FRAME_PAYLOAD_SIZE` to account
/// for the compression byte and rare incompressible payloads.
pub const MAX_COMPRESSED_FRAME_SIZE: usize = 16 * 1_048_576;

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
    Serialize(#[from] bmux_codec::Error),
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
    #[error("frame decompression failed: {0}")]
    Decompress(#[from] compression::CompressionError),
    #[error("unknown frame compression id: {0}")]
    UnknownCompression(u8),
    #[error("failed to deserialize frame payload: {0}")]
    Deserialize(bmux_codec::Error),
}

/// Encode an envelope into a length-prefixed frame.
///
/// Framing format:
/// - 4-byte little-endian payload length
/// - binary-encoded envelope payload
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

// ── Compressed frame encoding/decoding ───────────────────────────────────────
//
// Compressed frame format (used ONLY after capability negotiation):
//
//   [4-byte LE total_len][1-byte compression_id][payload_bytes]
//
// Where `total_len` includes the 1-byte compression_id + payload_bytes.
// When `compression_id == 0` the payload_bytes are uncompressed.
// Otherwise the payload_bytes must be decompressed with the indicated codec.
//
// The uncompressed payload is always validated against `MAX_FRAME_PAYLOAD_SIZE`
// to prevent decompression bombs.

/// Encode an envelope into a length-prefixed frame with optional compression.
///
/// If the codec is provided and the serialized payload is above the codec's
/// threshold, the payload is compressed and the compression byte reflects the
/// codec used.  Otherwise the compression byte is 0 (none) and the payload
/// is stored uncompressed.
///
/// The wire frame always includes the 1-byte compression indicator so that
/// the decoder can distinguish compressed from uncompressed frames.
///
/// # Errors
///
/// Returns an error if serialization fails or the payload exceeds the max.
pub fn encode_frame_compressed(
    envelope: &Envelope,
    codec: Option<&dyn CompressionCodec>,
) -> Result<Vec<u8>, FrameEncodeError> {
    let serialized = encode(envelope)?;
    if serialized.len() > MAX_FRAME_PAYLOAD_SIZE {
        return Err(FrameEncodeError::PayloadTooLarge {
            actual: serialized.len(),
            max: MAX_FRAME_PAYLOAD_SIZE,
        });
    }

    let (payload_bytes, comp_byte) = if let Some(codec) = codec {
        if serialized.len() >= codec.threshold(CompressionHint::Frame) {
            if let Some(compressed) = codec.compress(&serialized, CompressionHint::Frame) {
                if compressed.len() < serialized.len() {
                    (compressed, codec.id() as u8)
                } else {
                    (serialized, 0u8)
                }
            } else {
                (serialized, 0u8)
            }
        } else {
            (serialized, 0u8)
        }
    } else {
        (serialized, 0u8)
    };

    // total_len = 1 (compression byte) + payload_bytes.len()
    let total_len = 1 + payload_bytes.len();
    let total_len_u32 =
        u32::try_from(total_len).map_err(|_| FrameEncodeError::PayloadTooLarge {
            actual: total_len,
            max: MAX_COMPRESSED_FRAME_SIZE,
        })?;
    let mut frame = Vec::with_capacity(FRAME_LEN_BYTES + total_len);
    frame.extend_from_slice(&total_len_u32.to_le_bytes());
    frame.push(comp_byte);
    frame.extend_from_slice(&payload_bytes);
    Ok(frame)
}

/// Decode one exact compressed frame from bytes.
///
/// Reads the compression byte, decompresses if needed, then deserializes
/// the envelope.  Validates uncompressed size against `MAX_FRAME_PAYLOAD_SIZE`.
///
/// # Errors
///
/// Returns an error for truncated, malformed, oversized, unknown compression,
/// decompression failure, or deserialization failures.
pub fn decode_frame_compressed(frame: &[u8]) -> Result<Envelope, FrameDecodeError> {
    if frame.len() < FRAME_LEN_BYTES + 1 {
        return Err(FrameDecodeError::IncompleteFrame);
    }
    let total_len = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
    if total_len > MAX_COMPRESSED_FRAME_SIZE {
        return Err(FrameDecodeError::PayloadTooLarge {
            actual: total_len,
            max: MAX_COMPRESSED_FRAME_SIZE,
        });
    }
    let expected = FRAME_LEN_BYTES + total_len;
    if frame.len() < expected {
        return Err(FrameDecodeError::IncompleteFrame);
    }
    if frame.len() > expected {
        return Err(FrameDecodeError::TrailingBytes);
    }

    let comp_byte = frame[FRAME_LEN_BYTES];
    let payload_bytes = &frame[FRAME_LEN_BYTES + 1..expected];

    let decompressed = match compression::CompressionId::from_byte(comp_byte) {
        Some(compression::CompressionId::None) => payload_bytes.to_vec(),
        Some(id) => {
            let data = compression::decompress_by_id(payload_bytes, id)?;
            if data.len() > MAX_FRAME_PAYLOAD_SIZE {
                return Err(FrameDecodeError::PayloadTooLarge {
                    actual: data.len(),
                    max: MAX_FRAME_PAYLOAD_SIZE,
                });
            }
            data
        }
        None => return Err(FrameDecodeError::UnknownCompression(comp_byte)),
    };

    decode_envelope_payload(&decompressed)
}

/// Try to decode one compressed frame from a mutable input buffer.
///
/// Returns `Ok(None)` when the buffer does not yet contain a complete frame.
pub fn try_decode_frame_compressed(
    buffer: &mut Vec<u8>,
) -> Result<Option<Envelope>, FrameDecodeError> {
    if buffer.len() < FRAME_LEN_BYTES + 1 {
        return Ok(None);
    }
    let total_len = u32::from_le_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]) as usize;
    if total_len > MAX_COMPRESSED_FRAME_SIZE {
        return Err(FrameDecodeError::PayloadTooLarge {
            actual: total_len,
            max: MAX_COMPRESSED_FRAME_SIZE,
        });
    }
    let expected = FRAME_LEN_BYTES + total_len;
    if buffer.len() < expected {
        return Ok(None);
    }

    let comp_byte = buffer[FRAME_LEN_BYTES];
    let payload_bytes = buffer[FRAME_LEN_BYTES + 1..expected].to_vec();
    buffer.drain(..expected);

    let decompressed = match compression::CompressionId::from_byte(comp_byte) {
        Some(compression::CompressionId::None) => payload_bytes,
        Some(id) => {
            let data = compression::decompress_by_id(&payload_bytes, id)?;
            if data.len() > MAX_FRAME_PAYLOAD_SIZE {
                return Err(FrameDecodeError::PayloadTooLarge {
                    actual: data.len(),
                    max: MAX_FRAME_PAYLOAD_SIZE,
                });
            }
            data
        }
        None => return Err(FrameDecodeError::UnknownCompression(comp_byte)),
    };

    decode_envelope_payload(&decompressed).map(Some)
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

    #[test]
    fn compressed_frame_roundtrip_no_codec() {
        let payload = encode(&Request::Ping).expect("request should encode");
        let envelope = Envelope::new(42, EnvelopeKind::Request, payload);
        let frame = super::encode_frame_compressed(&envelope, None)
            .expect("compressed frame should encode");
        let decoded =
            super::decode_frame_compressed(&frame).expect("compressed frame should decode");
        assert_eq!(decoded, envelope);
    }

    #[cfg(feature = "compression-lz4")]
    #[test]
    fn compressed_frame_roundtrip_lz4() {
        use crate::compression::Lz4Codec;
        // Use a larger payload to exceed the compression threshold.
        let big_payload = vec![0x42u8; 1024];
        let envelope = Envelope::new(7, EnvelopeKind::Response, big_payload);
        let codec = Lz4Codec::new();
        let frame = super::encode_frame_compressed(&envelope, Some(&codec))
            .expect("compressed frame should encode");
        let decoded =
            super::decode_frame_compressed(&frame).expect("compressed frame should decode");
        assert_eq!(decoded, envelope);
    }

    #[cfg(feature = "compression-zstd")]
    #[test]
    fn compressed_frame_roundtrip_zstd() {
        use crate::compression::ZstdCodec;
        let big_payload = vec![0x42u8; 1024];
        let envelope = Envelope::new(99, EnvelopeKind::Event, big_payload);
        let codec = ZstdCodec::new();
        let frame = super::encode_frame_compressed(&envelope, Some(&codec))
            .expect("compressed frame should encode");
        let decoded =
            super::decode_frame_compressed(&frame).expect("compressed frame should decode");
        assert_eq!(decoded, envelope);
    }

    #[test]
    fn compressed_frame_rejects_unknown_compression_id() {
        // Build a valid-looking frame with an invalid compression byte.
        let payload = encode(&Request::Ping).expect("request should encode");
        let total_len = (1 + payload.len()) as u32;
        let mut frame = total_len.to_le_bytes().to_vec();
        frame.push(0xFF); // unknown compression id
        frame.extend_from_slice(&payload);
        let error =
            super::decode_frame_compressed(&frame).expect_err("should reject unknown compression");
        assert!(matches!(error, FrameDecodeError::UnknownCompression(0xFF)));
    }
}
