//! Compression support for IPC payloads and frames.
//!
//! Provides a trait-based abstraction over compression codecs (zstd, lz4)
//! with three intended usage layers:
//!
//! - **Payload compression** (Layer 1): compress individual large fields like
//!   image `raw_data` before IPC serialization.
//! - **Frame compression** (Layer 2): compress entire serialized IPC frames.
//! - **Transport compression** (Layer 3): streaming compression of the byte
//!   stream for remote connections.
//!
//! Each layer is independently configurable and feature-gated.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Compression identifier (wire format)
// ---------------------------------------------------------------------------

/// Identifies which compression algorithm was used on a payload.
///
/// Serialized as a single `u8` on the wire.  New variants must be appended
/// at the end to preserve backward compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[repr(u8)]
pub enum CompressionId {
    #[default]
    None = 0,
    Zstd = 1,
    Lz4 = 2,
}

impl CompressionId {
    /// Decode a raw byte into a `CompressionId`, returning `None` for
    /// unrecognised values.
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::None),
            1 => Some(Self::Zstd),
            2 => Some(Self::Lz4),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Compression hint
// ---------------------------------------------------------------------------

/// Hint describing the kind of data being compressed so the codec can tune
/// its parameters (e.g. compression level, dictionary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionHint {
    /// Large binary blob (image payload).  Favour ratio over speed.
    BulkPayload,
    /// IPC frame (mixed content).  Favour speed over ratio.
    Frame,
}

// ---------------------------------------------------------------------------
// Codec trait
// ---------------------------------------------------------------------------

/// A compression codec that can compress and decompress byte buffers.
///
/// Implementations must be `Send + Sync` so they can be shared across
/// async tasks (e.g. stored in `Arc<dyn CompressionCodec>`).
pub trait CompressionCodec: Send + Sync {
    /// The identifier written to the wire when this codec is used.
    fn id(&self) -> CompressionId;

    /// Compress `input`.  Returns `Some(compressed)` on success, or `None`
    /// if compression is not beneficial (e.g. output would be larger than
    /// input or the data is incompressible).
    fn compress(&self, input: &[u8], hint: CompressionHint) -> Option<Vec<u8>>;

    /// Decompress `input`, returning the original bytes.
    fn decompress(&self, input: &[u8]) -> Result<Vec<u8>, CompressionError>;

    /// Minimum input size worth compressing for the given hint.
    /// Inputs smaller than this are returned uncompressed.
    fn threshold(&self, hint: CompressionHint) -> usize;
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur during compression or decompression.
#[derive(Debug, thiserror::Error)]
pub enum CompressionError {
    #[error("decompression failed: {0}")]
    DecompressFailed(String),
    #[error("unknown compression id: {0}")]
    UnknownCodec(u8),
}

// ---------------------------------------------------------------------------
// Zstd codec
// ---------------------------------------------------------------------------

#[cfg(feature = "compression-zstd")]
pub struct ZstdCodec {
    /// Compression level for bulk payloads (images).
    bulk_level: i32,
    /// Compression level for frames (latency-sensitive).
    frame_level: i32,
}

#[cfg(feature = "compression-zstd")]
impl ZstdCodec {
    /// Create a new zstd codec with sensible defaults.
    ///
    /// - `bulk_level`: 3 (good ratio for image data)
    /// - `frame_level`: 1 (fastest, still worthwhile)
    pub fn new() -> Self {
        Self {
            bulk_level: 3,
            frame_level: 1,
        }
    }

    /// Create with custom levels.
    pub fn with_levels(bulk_level: i32, frame_level: i32) -> Self {
        Self {
            bulk_level,
            frame_level,
        }
    }

    /// Create with a single level used for bulk payloads (images).
    /// Frame-level compression always uses level 1 for low latency.
    pub fn with_level(level: i32) -> Self {
        Self {
            bulk_level: level,
            frame_level: 1,
        }
    }
}

#[cfg(feature = "compression-zstd")]
impl Default for ZstdCodec {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "compression-zstd")]
impl CompressionCodec for ZstdCodec {
    fn id(&self) -> CompressionId {
        CompressionId::Zstd
    }

    fn compress(&self, input: &[u8], hint: CompressionHint) -> Option<Vec<u8>> {
        let level = match hint {
            CompressionHint::BulkPayload => self.bulk_level,
            CompressionHint::Frame => self.frame_level,
        };
        let compressed = zstd::bulk::compress(input, level).ok()?;
        // Only use compressed output if it actually saves space.
        if compressed.len() < input.len() {
            Some(compressed)
        } else {
            None
        }
    }

    fn decompress(&self, input: &[u8]) -> Result<Vec<u8>, CompressionError> {
        // Use a generous upper bound; the actual decompressed size is encoded
        // in the zstd frame header so the library handles allocation.
        zstd::bulk::decompress(input, 64 * 1024 * 1024)
            .map_err(|e| CompressionError::DecompressFailed(e.to_string()))
    }

    fn threshold(&self, hint: CompressionHint) -> usize {
        match hint {
            CompressionHint::BulkPayload => 4096,
            CompressionHint::Frame => 256,
        }
    }
}

// ---------------------------------------------------------------------------
// LZ4 codec
// ---------------------------------------------------------------------------

#[cfg(feature = "compression-lz4")]
pub struct Lz4Codec;

#[cfg(feature = "compression-lz4")]
impl Lz4Codec {
    pub fn new() -> Self {
        Self
    }
}

#[cfg(feature = "compression-lz4")]
impl Default for Lz4Codec {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "compression-lz4")]
impl CompressionCodec for Lz4Codec {
    fn id(&self) -> CompressionId {
        CompressionId::Lz4
    }

    fn compress(&self, input: &[u8], _hint: CompressionHint) -> Option<Vec<u8>> {
        let compressed = lz4_flex::compress_prepend_size(input);
        if compressed.len() < input.len() {
            Some(compressed)
        } else {
            None
        }
    }

    fn decompress(&self, input: &[u8]) -> Result<Vec<u8>, CompressionError> {
        lz4_flex::decompress_size_prepended(input)
            .map_err(|e| CompressionError::DecompressFailed(e.to_string()))
    }

    fn threshold(&self, hint: CompressionHint) -> usize {
        match hint {
            CompressionHint::BulkPayload => 4096,
            CompressionHint::Frame => 256,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compress `data` using the given codec if the data is above the threshold
/// and compression is beneficial.  Returns `(output_bytes, compression_id)`.
///
/// If the codec is `None`, data is below the threshold, or compression
/// doesn't reduce size, returns the original data with `CompressionId::None`.
pub fn compress_if_worthwhile(
    codec: Option<&dyn CompressionCodec>,
    data: &[u8],
    hint: CompressionHint,
) -> (Vec<u8>, CompressionId) {
    let Some(codec) = codec else {
        return (data.to_vec(), CompressionId::None);
    };
    if data.len() < codec.threshold(hint) {
        return (data.to_vec(), CompressionId::None);
    }
    match codec.compress(data, hint) {
        Some(compressed) => (compressed, codec.id()),
        None => (data.to_vec(), CompressionId::None),
    }
}

/// Decompress `data` based on the `CompressionId`.  Returns the original
/// bytes if `id` is `None`.
pub fn decompress_by_id(data: &[u8], id: CompressionId) -> Result<Vec<u8>, CompressionError> {
    match id {
        CompressionId::None => Ok(data.to_vec()),
        #[cfg(feature = "compression-zstd")]
        CompressionId::Zstd => ZstdCodec::new().decompress(data),
        #[cfg(feature = "compression-lz4")]
        CompressionId::Lz4 => Lz4Codec::new().decompress(data),
        #[allow(unreachable_patterns)]
        other => Err(CompressionError::UnknownCodec(other as u8)),
    }
}

/// Resolve a `CompressionId` into a boxed codec instance, or `None` if the
/// id is `None` or the required feature is not compiled in.
pub fn resolve_codec(id: CompressionId) -> Option<Box<dyn CompressionCodec>> {
    match id {
        CompressionId::None => None,
        #[cfg(feature = "compression-zstd")]
        CompressionId::Zstd => Some(Box::new(ZstdCodec::new())),
        #[cfg(feature = "compression-lz4")]
        CompressionId::Lz4 => Some(Box::new(Lz4Codec::new())),
        #[allow(unreachable_patterns)]
        _ => None,
    }
}

/// Return the default payload compression codec (best available).
///
/// Prefers zstd (better ratio) for bulk payloads, falls back to lz4.
pub fn default_payload_codec() -> Option<Box<dyn CompressionCodec>> {
    #[cfg(feature = "compression-zstd")]
    {
        return Some(Box::new(ZstdCodec::new()));
    }
    #[cfg(feature = "compression-lz4")]
    {
        return Some(Box::new(Lz4Codec::new()));
    }
    #[allow(unreachable_code)]
    None
}

/// Return the default frame compression codec (fastest available).
///
/// Prefers lz4 (lower latency) for per-frame compression, falls back to zstd.
pub fn default_frame_codec() -> Option<Box<dyn CompressionCodec>> {
    #[cfg(feature = "compression-lz4")]
    {
        return Some(Box::new(Lz4Codec::new()));
    }
    #[cfg(feature = "compression-zstd")]
    {
        return Some(Box::new(ZstdCodec::new()));
    }
    #[allow(unreachable_code)]
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "compression-zstd")]
    #[test]
    fn zstd_roundtrip() {
        let codec = ZstdCodec::new();
        // Compressible data (repeated pattern).
        let data = vec![42u8; 8192];
        let compressed = codec
            .compress(&data, CompressionHint::BulkPayload)
            .expect("should compress");
        assert!(compressed.len() < data.len());
        let decompressed = codec.decompress(&compressed).expect("should decompress");
        assert_eq!(decompressed, data);
    }

    #[cfg(feature = "compression-zstd")]
    #[test]
    fn zstd_below_threshold_skipped_by_helper() {
        let codec = ZstdCodec::new();
        let small = vec![1u8; 100]; // below 4096 threshold
        let (output, id) =
            compress_if_worthwhile(Some(&codec), &small, CompressionHint::BulkPayload);
        assert_eq!(id, CompressionId::None);
        assert_eq!(output, small);
    }

    #[cfg(feature = "compression-lz4")]
    #[test]
    fn lz4_roundtrip() {
        let codec = Lz4Codec::new();
        let data = vec![42u8; 8192];
        let compressed = codec
            .compress(&data, CompressionHint::Frame)
            .expect("should compress");
        assert!(compressed.len() < data.len());
        let decompressed = codec.decompress(&compressed).expect("should decompress");
        assert_eq!(decompressed, data);
    }

    #[cfg(feature = "compression-lz4")]
    #[test]
    fn lz4_below_threshold_skipped_by_helper() {
        let codec = Lz4Codec::new();
        let small = vec![1u8; 100];
        let (output, id) = compress_if_worthwhile(Some(&codec), &small, CompressionHint::Frame);
        assert_eq!(id, CompressionId::None);
        assert_eq!(output, small);
    }

    #[test]
    fn no_codec_returns_original() {
        let data = vec![42u8; 8192];
        let (output, id) = compress_if_worthwhile(None, &data, CompressionHint::BulkPayload);
        assert_eq!(id, CompressionId::None);
        assert_eq!(output, data);
    }

    #[test]
    fn decompress_none_returns_original() {
        let data = vec![1, 2, 3, 4];
        let result = decompress_by_id(&data, CompressionId::None).unwrap();
        assert_eq!(result, data);
    }

    #[cfg(feature = "compression-zstd")]
    #[test]
    fn decompress_by_id_zstd_roundtrip() {
        let data = vec![42u8; 8192];
        let compressed = ZstdCodec::new()
            .compress(&data, CompressionHint::BulkPayload)
            .unwrap();
        let decompressed = decompress_by_id(&compressed, CompressionId::Zstd).unwrap();
        assert_eq!(decompressed, data);
    }

    #[cfg(feature = "compression-lz4")]
    #[test]
    fn decompress_by_id_lz4_roundtrip() {
        let data = vec![42u8; 8192];
        let compressed = Lz4Codec::new()
            .compress(&data, CompressionHint::BulkPayload)
            .unwrap();
        let decompressed = decompress_by_id(&compressed, CompressionId::Lz4).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn compression_id_from_byte() {
        assert_eq!(CompressionId::from_byte(0), Some(CompressionId::None));
        assert_eq!(CompressionId::from_byte(1), Some(CompressionId::Zstd));
        assert_eq!(CompressionId::from_byte(2), Some(CompressionId::Lz4));
        assert_eq!(CompressionId::from_byte(255), None);
    }
}
