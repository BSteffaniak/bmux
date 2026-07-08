//! Optional byte compression helpers for encoded bmux payloads.
//!
//! Compression is intentionally layered after serialization. Existing bmux codec
//! encode/decode APIs remain uncompressed unless callers explicitly opt into
//! these helpers.

use crate::Error;

/// Stable wire id for LZ4 compression.
pub const LZ4_WIRE_ID: u8 = 1;
/// Stable wire id for Zstandard compression.
pub const ZSTD_WIRE_ID: u8 = 2;

/// Compression algorithms available in this build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionAlgorithm {
    /// LZ4 block compression.
    #[cfg(feature = "compression-lz4")]
    Lz4,
    /// Zstandard compression.
    #[cfg(feature = "compression-zstd")]
    Zstd,
}

impl CompressionAlgorithm {
    /// Return this algorithm's stable wire id.
    #[must_use]
    pub const fn wire_id(self) -> u8 {
        match self {
            #[cfg(feature = "compression-lz4")]
            Self::Lz4 => LZ4_WIRE_ID,
            #[cfg(feature = "compression-zstd")]
            Self::Zstd => ZSTD_WIRE_ID,
        }
    }

    /// Decode a stable wire id into an algorithm available in this build.
    ///
    /// # Errors
    ///
    /// Returns an error when the wire id is unknown or the algorithm is not enabled.
    pub const fn from_wire_id(id: u8) -> Result<Self, Error> {
        match id {
            #[cfg(feature = "compression-lz4")]
            LZ4_WIRE_ID => Ok(Self::Lz4),
            #[cfg(not(feature = "compression-lz4"))]
            LZ4_WIRE_ID => Err(Error::CompressionAlgorithmUnavailable("lz4")),
            #[cfg(feature = "compression-zstd")]
            ZSTD_WIRE_ID => Ok(Self::Zstd),
            #[cfg(not(feature = "compression-zstd"))]
            ZSTD_WIRE_ID => Err(Error::CompressionAlgorithmUnavailable("zstd")),
            _ => Err(Error::UnknownCompressionAlgorithm(id)),
        }
    }
}

/// Compression policy for optional compression decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompressionPolicy {
    /// Algorithm to use when compression is attempted.
    pub algorithm: CompressionAlgorithm,
    /// Minimum input size before compression is attempted.
    pub min_bytes: usize,
    /// Require compressed output to be smaller than input.
    pub require_smaller: bool,
    /// Compression level for algorithms that support levels. Ignored by LZ4.
    pub level: i32,
}

impl CompressionPolicy {
    /// Create a default fast compression policy for an algorithm.
    #[must_use]
    pub const fn new(algorithm: CompressionAlgorithm) -> Self {
        Self {
            algorithm,
            min_bytes: 256 * 1024,
            require_smaller: true,
            level: 1,
        }
    }

    /// Return this policy with a custom minimum byte threshold.
    #[must_use]
    pub const fn min_bytes(mut self, min_bytes: usize) -> Self {
        self.min_bytes = min_bytes;
        self
    }

    /// Return this policy with a custom compression level.
    #[must_use]
    pub const fn level(mut self, level: i32) -> Self {
        self.level = level;
        self
    }

    /// Return this policy with custom require-smaller behavior.
    #[must_use]
    pub const fn require_smaller(mut self, require_smaller: bool) -> Self {
        self.require_smaller = require_smaller;
        self
    }
}

/// Reason optional compression returned plain bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionSkipReason {
    /// Input was below the policy threshold.
    BelowThreshold,
    /// Compressed output was not smaller than the input.
    NotSmaller,
}

/// Result of applying an optional compression policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompressionDecision {
    /// Payload remains plain/uncompressed.
    Plain {
        /// Original bytes.
        bytes: Vec<u8>,
        /// Reason compression was not applied.
        reason: CompressionSkipReason,
    },
    /// Payload was compressed.
    Compressed {
        /// Compression algorithm used.
        algorithm: CompressionAlgorithm,
        /// Uncompressed byte length.
        uncompressed_len: usize,
        /// Compressed bytes.
        bytes: Vec<u8>,
    },
}

impl CompressionDecision {
    /// Return true if this decision is compressed.
    #[must_use]
    pub const fn is_compressed(&self) -> bool {
        matches!(self, Self::Compressed { .. })
    }

    /// Return the encoded bytes regardless of compression state.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        match self {
            Self::Plain { bytes, .. } | Self::Compressed { bytes, .. } => bytes,
        }
    }
}

/// Byte compression implementation trait.
pub trait CompressionCodec {
    /// Return the algorithm implemented by this codec.
    fn algorithm(&self) -> CompressionAlgorithm;
    /// Compress bytes.
    ///
    /// # Errors
    ///
    /// Returns an error when compression fails.
    fn compress(&self, input: &[u8]) -> Result<Vec<u8>, Error>;
    /// Decompress bytes.
    ///
    /// # Errors
    ///
    /// Returns an error when decompression fails or the decoded length does not match.
    fn decompress(&self, input: &[u8], expected_len: usize) -> Result<Vec<u8>, Error>;
}

/// LZ4 compression codec.
#[cfg(feature = "compression-lz4")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Lz4Compression;

#[cfg(feature = "compression-lz4")]
impl CompressionCodec for Lz4Compression {
    fn algorithm(&self) -> CompressionAlgorithm {
        CompressionAlgorithm::Lz4
    }

    fn compress(&self, input: &[u8]) -> Result<Vec<u8>, Error> {
        Ok(lz4_flex::block::compress(input))
    }

    fn decompress(&self, input: &[u8], expected_len: usize) -> Result<Vec<u8>, Error> {
        lz4_flex::block::decompress(input, expected_len)
            .map_err(|error| Error::DecompressionFailed(error.to_string()))
            .and_then(|bytes| validate_decompressed_len(bytes, expected_len))
    }
}

/// Zstandard compression codec.
#[cfg(feature = "compression-zstd")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZstdCompression {
    /// Zstandard compression level.
    pub level: i32,
}

#[cfg(feature = "compression-zstd")]
impl ZstdCompression {
    /// Create a Zstandard codec at a compression level.
    #[must_use]
    pub const fn new(level: i32) -> Self {
        Self { level }
    }
}

#[cfg(feature = "compression-zstd")]
impl Default for ZstdCompression {
    fn default() -> Self {
        Self { level: 1 }
    }
}

#[cfg(feature = "compression-zstd")]
impl CompressionCodec for ZstdCompression {
    fn algorithm(&self) -> CompressionAlgorithm {
        CompressionAlgorithm::Zstd
    }

    fn compress(&self, input: &[u8]) -> Result<Vec<u8>, Error> {
        zstd::bulk::compress(input, self.level)
            .map_err(|error| Error::CompressionFailed(error.to_string()))
    }

    fn decompress(&self, input: &[u8], expected_len: usize) -> Result<Vec<u8>, Error> {
        zstd::bulk::decompress(input, expected_len)
            .map_err(|error| Error::DecompressionFailed(error.to_string()))
            .and_then(|bytes| validate_decompressed_len(bytes, expected_len))
    }
}

/// Maybe compress bytes according to a policy.
///
/// # Errors
///
/// Returns an error when the selected algorithm fails to compress.
pub fn maybe_compress_bytes(
    bytes: Vec<u8>,
    policy: CompressionPolicy,
) -> Result<CompressionDecision, Error> {
    if bytes.len() < policy.min_bytes {
        return Ok(CompressionDecision::Plain {
            bytes,
            reason: CompressionSkipReason::BelowThreshold,
        });
    }
    let compressed = compress_bytes(policy.algorithm, &bytes, policy.level)?;
    if policy.require_smaller && compressed.len() >= bytes.len() {
        return Ok(CompressionDecision::Plain {
            bytes,
            reason: CompressionSkipReason::NotSmaller,
        });
    }
    Ok(CompressionDecision::Compressed {
        algorithm: policy.algorithm,
        uncompressed_len: bytes.len(),
        bytes: compressed,
    })
}

/// Compress bytes using an algorithm.
///
/// # Errors
///
/// Returns an error when the algorithm is unavailable or compression fails.
pub fn compress_bytes(
    algorithm: CompressionAlgorithm,
    bytes: &[u8],
    level: i32,
) -> Result<Vec<u8>, Error> {
    match algorithm {
        #[cfg(feature = "compression-lz4")]
        CompressionAlgorithm::Lz4 => Lz4Compression.compress(bytes),
        #[cfg(feature = "compression-zstd")]
        CompressionAlgorithm::Zstd => ZstdCompression::new(level).compress(bytes),
    }
}

/// Decompress bytes using an algorithm.
///
/// # Errors
///
/// Returns an error when the algorithm is unavailable, decompression fails, or the length mismatches.
pub fn decompress_bytes(
    algorithm: CompressionAlgorithm,
    bytes: &[u8],
    expected_len: usize,
) -> Result<Vec<u8>, Error> {
    match algorithm {
        #[cfg(feature = "compression-lz4")]
        CompressionAlgorithm::Lz4 => Lz4Compression.decompress(bytes, expected_len),
        #[cfg(feature = "compression-zstd")]
        CompressionAlgorithm::Zstd => ZstdCompression::default().decompress(bytes, expected_len),
    }
}

#[cfg(all(feature = "serde", feature = "typed-stable"))]
/// Encode a value with typed-stable serialization, then maybe compress it.
///
/// # Errors
///
/// Returns an error when serialization or compression fails.
pub fn to_typed_maybe_compressed_vec<T>(
    value: &T,
    policy: CompressionPolicy,
) -> Result<CompressionDecision, Error>
where
    T: serde::Serialize,
{
    maybe_compress_bytes(crate::to_typed_vec(value)?, policy)
}

#[cfg(all(feature = "serde", feature = "typed-stable"))]
/// Decompress typed-stable bytes, then decode a value.
///
/// # Errors
///
/// Returns an error when decompression or deserialization fails.
pub fn from_typed_compressed_bytes<T>(
    algorithm: CompressionAlgorithm,
    bytes: &[u8],
    expected_len: usize,
) -> Result<T, Error>
where
    T: serde::de::DeserializeOwned,
{
    crate::from_typed_bytes(&decompress_bytes(algorithm, bytes, expected_len)?)
}

#[cfg(all(feature = "serde", feature = "positional"))]
/// Encode a value with positional serialization, then maybe compress it.
///
/// # Errors
///
/// Returns an error when serialization or compression fails.
pub fn to_positional_maybe_compressed_vec<T>(
    value: &T,
    policy: CompressionPolicy,
) -> Result<CompressionDecision, Error>
where
    T: serde::Serialize,
{
    maybe_compress_bytes(crate::to_positional_vec(value)?, policy)
}

#[cfg(all(feature = "serde", feature = "positional"))]
/// Decompress positional bytes, then decode a value.
///
/// # Errors
///
/// Returns an error when decompression or deserialization fails.
pub fn from_positional_compressed_bytes<T>(
    algorithm: CompressionAlgorithm,
    bytes: &[u8],
    expected_len: usize,
) -> Result<T, Error>
where
    T: serde::de::DeserializeOwned,
{
    crate::from_positional_bytes(&decompress_bytes(algorithm, bytes, expected_len)?)
}

fn validate_decompressed_len(bytes: Vec<u8>, expected_len: usize) -> Result<Vec<u8>, Error> {
    if bytes.len() == expected_len {
        Ok(bytes)
    } else {
        Err(Error::DecompressedLengthMismatch {
            expected: expected_len,
            actual: bytes.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repeated_payload() -> Vec<u8> {
        "bmux-codec-compression-test-payload|"
            .repeat(4096)
            .into_bytes()
    }

    #[cfg(feature = "compression-lz4")]
    #[test]
    fn lz4_round_trips_bytes() {
        let input = repeated_payload();
        let compressed = compress_bytes(CompressionAlgorithm::Lz4, &input, 1).unwrap();
        assert!(compressed.len() < input.len());
        assert_eq!(
            decompress_bytes(CompressionAlgorithm::Lz4, &compressed, input.len()).unwrap(),
            input
        );
    }

    #[cfg(feature = "compression-zstd")]
    #[test]
    fn zstd_round_trips_bytes() {
        let input = repeated_payload();
        let compressed = compress_bytes(CompressionAlgorithm::Zstd, &input, 1).unwrap();
        assert!(compressed.len() < input.len());
        assert_eq!(
            decompress_bytes(CompressionAlgorithm::Zstd, &compressed, input.len()).unwrap(),
            input
        );
    }

    #[cfg(feature = "compression-lz4")]
    #[test]
    fn below_threshold_returns_plain() {
        let input = b"small".to_vec();
        let decision = maybe_compress_bytes(
            input.clone(),
            CompressionPolicy::new(CompressionAlgorithm::Lz4).min_bytes(1024),
        )
        .unwrap();
        assert_eq!(
            decision,
            CompressionDecision::Plain {
                bytes: input,
                reason: CompressionSkipReason::BelowThreshold,
            }
        );
    }

    #[cfg(feature = "compression-lz4")]
    #[test]
    fn not_smaller_returns_plain_when_required() {
        let input = (0_u8..=255).collect::<Vec<_>>();
        let decision = maybe_compress_bytes(
            input.clone(),
            CompressionPolicy::new(CompressionAlgorithm::Lz4).min_bytes(0),
        )
        .unwrap();
        if let CompressionDecision::Plain { bytes, reason } = decision {
            assert_eq!(bytes, input);
            assert_eq!(reason, CompressionSkipReason::NotSmaller);
        }
    }

    #[cfg(feature = "compression-lz4")]
    #[test]
    fn wire_id_round_trips_enabled_algorithm() {
        assert_eq!(
            CompressionAlgorithm::from_wire_id(CompressionAlgorithm::Lz4.wire_id()).unwrap(),
            CompressionAlgorithm::Lz4
        );
    }

    #[test]
    fn unknown_wire_id_errors() {
        assert!(matches!(
            CompressionAlgorithm::from_wire_id(255),
            Err(Error::UnknownCompressionAlgorithm(255))
        ));
    }

    #[cfg(feature = "compression-lz4")]
    #[test]
    fn length_mismatch_errors() {
        let input = repeated_payload();
        let compressed = compress_bytes(CompressionAlgorithm::Lz4, &input, 1).unwrap();
        assert!(matches!(
            decompress_bytes(CompressionAlgorithm::Lz4, &compressed, input.len() + 1),
            Err(Error::DecompressionFailed(_)) | Err(Error::DecompressedLengthMismatch { .. })
        ));
    }

    #[cfg(all(
        feature = "compression-lz4",
        feature = "serde",
        feature = "typed-stable"
    ))]
    #[test]
    fn typed_helper_round_trips_compressed_payload() {
        let value = vec!["hello compression".to_owned(); 1024];
        let decision = to_typed_maybe_compressed_vec(
            &value,
            CompressionPolicy::new(CompressionAlgorithm::Lz4).min_bytes(0),
        )
        .unwrap();
        let CompressionDecision::Compressed {
            algorithm,
            uncompressed_len,
            bytes,
        } = decision
        else {
            panic!("expected compressed payload");
        };
        let decoded: Vec<String> =
            from_typed_compressed_bytes(algorithm, &bytes, uncompressed_len).unwrap();
        assert_eq!(decoded, value);
    }

    #[cfg(all(
        feature = "compression-zstd",
        feature = "serde",
        feature = "positional"
    ))]
    #[test]
    fn positional_helper_round_trips_compressed_payload() {
        let value = vec!["hello compression".to_owned(); 1024];
        let decision = to_positional_maybe_compressed_vec(
            &value,
            CompressionPolicy::new(CompressionAlgorithm::Zstd).min_bytes(0),
        )
        .unwrap();
        let CompressionDecision::Compressed {
            algorithm,
            uncompressed_len,
            bytes,
        } = decision
        else {
            panic!("expected compressed payload");
        };
        let decoded: Vec<String> =
            from_positional_compressed_bytes(algorithm, &bytes, uncompressed_len).unwrap();
        assert_eq!(decoded, value);
    }
}
