//! Zstandard compression support.

use crate::Error;

use super::{CompressionAlgorithm, CompressionCodec, validate_decompressed_len};

/// Zstandard compression codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZstdCompression {
    /// Zstandard compression level.
    pub level: i32,
}

impl ZstdCompression {
    /// Create a Zstandard codec at a compression level.
    #[must_use]
    pub const fn new(level: i32) -> Self {
        Self { level }
    }
}

impl Default for ZstdCompression {
    fn default() -> Self {
        Self { level: 1 }
    }
}

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
