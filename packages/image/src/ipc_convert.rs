//! Conversions between `bmux_image` types and `bmux_ipc` transport types.
//!
//! Keeps the IPC crate independent of the image crate while allowing
//! the server and client to convert between domain types and wire types.
//!
//! When IPC compression features are compiled in, image payloads are
//! transparently compressed during `PaneImage → AttachPaneImage` conversion
//! and decompressed during the reverse conversion.

use crate::model::{ImagePayload, ImageProtocol, PaneImage};
use bmux_ipc::{AttachImageProtocol, AttachPaneImage, AttachPaneImageDelta};

// ---------------------------------------------------------------------------
// PaneImage -> AttachPaneImage (server sends to client)
// ---------------------------------------------------------------------------

impl From<&PaneImage> for AttachPaneImage {
    fn from(img: &PaneImage) -> Self {
        let raw = img.payload.raw.clone().unwrap_or_default();
        let (compressed_data, compression_id) =
            compress_image_payload(&raw, protocol_to_ipc(img.protocol));
        Self {
            id: img.id,
            protocol: protocol_to_ipc(img.protocol),
            raw_data: compressed_data,
            compression: compression_id,
            position_row: img.position.row,
            position_col: img.position.col,
            cell_rows: img.cell_size.rows,
            cell_cols: img.cell_size.cols,
            pixel_width: img.pixel_size.width,
            pixel_height: img.pixel_size.height,
        }
    }
}

// ---------------------------------------------------------------------------
// AttachPaneImage -> PaneImage (client receives from server)
// ---------------------------------------------------------------------------

impl From<&AttachPaneImage> for PaneImage {
    fn from(ipc: &AttachPaneImage) -> Self {
        let raw = decompress_image_payload(&ipc.raw_data, ipc.compression);
        Self {
            id: ipc.id,
            protocol: protocol_from_ipc(ipc.protocol),
            payload: ImagePayload {
                raw: if raw.is_empty() { None } else { Some(raw) },
                pixels: None,
            },
            position: crate::model::ImagePosition {
                row: ipc.position_row,
                col: ipc.position_col,
            },
            cell_size: crate::model::ImageCellSize {
                rows: ipc.cell_rows,
                cols: ipc.cell_cols,
            },
            pixel_size: crate::model::ImagePixelSize {
                width: ipc.pixel_width,
                height: ipc.pixel_height,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// ImageDelta -> AttachPaneImageDelta
// ---------------------------------------------------------------------------

impl crate::model::ImageDelta {
    /// Convert to an IPC delta for a specific pane.
    pub fn to_ipc(&self, pane_id: uuid::Uuid) -> AttachPaneImageDelta {
        AttachPaneImageDelta {
            pane_id,
            added: self.added.iter().map(AttachPaneImage::from).collect(),
            removed: self.removed.clone(),
            sequence: self.sequence,
        }
    }
}

// ---------------------------------------------------------------------------
// Protocol enum conversions
// ---------------------------------------------------------------------------

fn protocol_to_ipc(p: ImageProtocol) -> AttachImageProtocol {
    match p {
        ImageProtocol::Sixel => AttachImageProtocol::Sixel,
        ImageProtocol::KittyGraphics => AttachImageProtocol::KittyGraphics,
        ImageProtocol::ITerm2 => AttachImageProtocol::ITerm2,
    }
}

fn protocol_from_ipc(p: AttachImageProtocol) -> ImageProtocol {
    match p {
        AttachImageProtocol::Sixel => ImageProtocol::Sixel,
        AttachImageProtocol::KittyGraphics => ImageProtocol::KittyGraphics,
        AttachImageProtocol::ITerm2 => ImageProtocol::ITerm2,
    }
}

// ---------------------------------------------------------------------------
// Payload compression helpers
// ---------------------------------------------------------------------------

/// Compress image `raw_data` before IPC serialization.
///
/// Uses the best available codec (zstd preferred for image data).  Skips
/// compression for small payloads and data that is already compressed
/// (e.g. kitty PNG payloads).
fn compress_image_payload(
    data: &[u8],
    protocol: AttachImageProtocol,
) -> (Vec<u8>, bmux_ipc::compression::CompressionId) {
    use bmux_ipc::compression::{CompressionHint, CompressionId};

    // Skip compression for pre-compressed formats.
    if is_precompressed(data, protocol) {
        return (data.to_vec(), CompressionId::None);
    }

    // Use the best available payload codec.
    let codec = bmux_ipc::compression::default_payload_codec();
    bmux_ipc::compression::compress_if_worthwhile(
        codec.as_deref(),
        data,
        CompressionHint::BulkPayload,
    )
}

/// Decompress image `raw_data` received from IPC.
fn decompress_image_payload(data: &[u8], id: bmux_ipc::compression::CompressionId) -> Vec<u8> {
    bmux_ipc::compression::decompress_by_id(data, id).unwrap_or_else(|_| data.to_vec())
}

/// Check if the raw data is already in a compressed format that would not
/// benefit from additional compression.
fn is_precompressed(data: &[u8], protocol: AttachImageProtocol) -> bool {
    match protocol {
        AttachImageProtocol::KittyGraphics => {
            // Kitty PNG payloads start with PNG magic bytes.
            data.starts_with(&[0x89, 0x50, 0x4E, 0x47])
        }
        // Sixel is ASCII text — highly compressible.
        // iTerm2 body is base64-encoded — compressible (~1.3x).
        AttachImageProtocol::Sixel | AttachImageProtocol::ITerm2 => false,
    }
}
