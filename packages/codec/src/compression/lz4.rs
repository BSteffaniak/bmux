//! LZ4 compression support.

use crate::Error;

use super::{CompressionAlgorithm, CompressionCodec, validate_decompressed_len};

/// LZ4 compression codec.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Lz4Compression;

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
