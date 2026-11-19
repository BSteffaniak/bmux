#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]

//! Neutral attach image transport DTOs.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Identifies which compression algorithm was used on a payload.
///
/// Serialized as a single `u8` on the wire. New variants must be appended
/// to preserve backwards-compatible decoding.
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
    #[must_use]
    pub const fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::None),
            1 => Some(Self::Zstd),
            2 => Some(Self::Lz4),
            _ => None,
        }
    }
}

/// Image protocol identifier for attach image transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachImageProtocol {
    Sixel,
    KittyGraphics,
    ITerm2,
}

/// A single image placed within a pane, for attach image transport.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachPaneImage {
    pub id: u64,
    pub protocol: AttachImageProtocol,
    /// Raw protocol bytes (sixel body, kitty payload, iTerm2 data),
    /// potentially compressed according to `compression`. Shared, not copied: an image's
    /// bytes never change, so whoever keeps the image (a frame, a decoded cache, a capture)
    /// shares one allocation, and the same allocation means the same image. Encoded as
    /// plain bytes.
    #[serde(with = "shared_bytes")]
    pub raw_data: std::sync::Arc<[u8]>,
    /// Compression algorithm applied to `raw_data`. `None` means the data
    /// is uncompressed. The receiver must decompress before use.
    pub compression: CompressionId,
    pub position_row: u16,
    pub position_col: u16,
    pub cell_rows: u16,
    pub cell_cols: u16,
    pub pixel_width: u32,
    pub pixel_height: u32,
}

/// Incremental image update for a single pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachPaneImageDelta {
    pub pane_id: Uuid,
    pub added: Vec<AttachPaneImage>,
    pub removed: Vec<u64>,
    pub sequence: u64,
}

/// Shared bytes encoded as plain codec bytes (the wire is the same as for a byte vector).
mod shared_bytes {
    use std::sync::Arc;

    pub fn serialize<S: serde::Serializer>(
        bytes: &Arc<[u8]>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        bmux_codec::serde_bytes_vec::serialize(bytes, serializer)
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Arc<[u8]>, D::Error> {
        bmux_codec::serde_bytes_vec::deserialize(deserializer).map(Arc::from)
    }
}

#[cfg(test)]
mod tests {
    use super::{AttachImageProtocol, AttachPaneImage, CompressionId};

    /// The image as it was encoded with a byte vector: shared bytes must encode the same.
    #[derive(serde::Serialize)]
    struct VecImage {
        id: u64,
        protocol: AttachImageProtocol,
        #[serde(with = "bmux_codec::serde_bytes_vec")]
        raw_data: Vec<u8>,
        compression: CompressionId,
        position_row: u16,
        position_col: u16,
        cell_rows: u16,
        cell_cols: u16,
        pixel_width: u32,
        pixel_height: u32,
    }

    #[test]
    fn shared_image_bytes_encode_as_a_byte_vector_did() {
        let bytes = vec![0x1b, b'P', b'q', 7, 0, 255];
        let image = AttachPaneImage {
            id: 7,
            protocol: AttachImageProtocol::Sixel,
            raw_data: bytes.clone().into(),
            compression: CompressionId::Zstd,
            position_row: 1,
            position_col: 2,
            cell_rows: 3,
            cell_cols: 4,
            pixel_width: 5,
            pixel_height: 6,
        };
        let old = VecImage {
            id: 7,
            protocol: AttachImageProtocol::Sixel,
            raw_data: bytes,
            compression: CompressionId::Zstd,
            position_row: 1,
            position_col: 2,
            cell_rows: 3,
            cell_cols: 4,
            pixel_width: 5,
            pixel_height: 6,
        };
        let stable = bmux_codec::to_vec(&image).unwrap();
        assert_eq!(stable, bmux_codec::to_vec(&old).unwrap());
        assert_eq!(
            bmux_codec::to_positional_vec(&image).unwrap(),
            bmux_codec::to_positional_vec(&old).unwrap()
        );
        assert_eq!(
            bmux_codec::from_bytes::<AttachPaneImage>(&stable).unwrap(),
            image
        );
    }
}
