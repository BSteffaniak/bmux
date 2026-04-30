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
    /// potentially compressed according to `compression`.
    #[serde(with = "bmux_codec::serde_bytes_vec")]
    pub raw_data: Vec<u8>,
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
