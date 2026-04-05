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
        pane_image_to_ipc(img, None)
    }
}

/// Convert a `PaneImage` to an `AttachPaneImage`, optionally compressing
/// the raw payload with the provided codec.  When `codec` is `None`,
/// the default auto-detected codec is used.
pub fn pane_image_to_ipc(
    img: &PaneImage,
    codec: Option<&dyn bmux_ipc::compression::CompressionCodec>,
) -> AttachPaneImage {
    let raw = img.payload.raw.clone().unwrap_or_default();
    let (compressed_data, compression_id) =
        compress_image_payload(&raw, protocol_to_ipc(img.protocol), codec);
    AttachPaneImage {
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
    ///
    /// When `payload_codec` is provided, image payloads are compressed with
    /// the given codec.  When `None`, uses the default auto-detected codec.
    pub fn to_ipc(
        &self,
        pane_id: uuid::Uuid,
        payload_codec: Option<&dyn bmux_ipc::compression::CompressionCodec>,
    ) -> AttachPaneImageDelta {
        AttachPaneImageDelta {
            pane_id,
            added: self
                .added
                .iter()
                .map(|img| pane_image_to_ipc(img, payload_codec))
                .collect(),
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
    explicit_codec: Option<&dyn bmux_ipc::compression::CompressionCodec>,
) -> (Vec<u8>, bmux_ipc::compression::CompressionId) {
    use bmux_ipc::compression::{CompressionHint, CompressionId};

    // Skip compression for pre-compressed formats.
    if is_precompressed(data, protocol) {
        return (data.to_vec(), CompressionId::None);
    }

    // Use the explicit codec if provided, otherwise auto-detect.
    if let Some(codec) = explicit_codec {
        return bmux_ipc::compression::compress_if_worthwhile(
            Some(codec),
            data,
            CompressionHint::BulkPayload,
        );
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ImageCellSize, ImagePayload, ImagePixelSize, ImagePosition};

    fn make_pane_image(raw: Vec<u8>, protocol: ImageProtocol) -> PaneImage {
        PaneImage {
            id: 1,
            protocol,
            payload: ImagePayload {
                raw: Some(raw),
                pixels: None,
            },
            position: ImagePosition { row: 0, col: 0 },
            cell_size: ImageCellSize { rows: 5, cols: 10 },
            pixel_size: ImagePixelSize {
                width: 80,
                height: 40,
            },
        }
    }

    #[test]
    fn ipc_roundtrip_preserves_payload_with_compression() {
        // Create a large enough sixel payload to trigger compression.
        let raw = vec![b'#'; 8192]; // >4KB, highly compressible
        let img = make_pane_image(raw.clone(), ImageProtocol::Sixel);

        // Convert to IPC (triggers compression).
        let ipc = AttachPaneImage::from(&img);

        // Verify compression was applied (when features are available).
        #[cfg(feature = "compression-zstd")]
        assert_ne!(
            ipc.compression,
            bmux_ipc::compression::CompressionId::None,
            "large repetitive data should be compressed"
        );

        // Verify compressed data is smaller.
        #[cfg(any(feature = "compression-zstd", feature = "compression-lz4"))]
        assert!(
            ipc.raw_data.len() < raw.len(),
            "compressed size {} should be less than original {}",
            ipc.raw_data.len(),
            raw.len()
        );

        // Convert back (triggers decompression).
        let roundtripped = PaneImage::from(&ipc);
        assert_eq!(
            roundtripped.payload.raw.as_deref(),
            Some(raw.as_slice()),
            "roundtripped payload should match original"
        );
    }

    #[test]
    fn small_payload_not_compressed() {
        let raw = vec![b'#'; 100]; // <4KB, below threshold
        let img = make_pane_image(raw.clone(), ImageProtocol::Sixel);
        let ipc = AttachPaneImage::from(&img);
        assert_eq!(
            ipc.compression,
            bmux_ipc::compression::CompressionId::None,
            "small payload should not be compressed"
        );
        assert_eq!(ipc.raw_data, raw);
    }

    #[test]
    fn precompressed_kitty_png_not_double_compressed() {
        // PNG magic + some bytes
        let mut raw = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        raw.extend(vec![0u8; 8192]); // pad to be above threshold
        let img = make_pane_image(raw.clone(), ImageProtocol::KittyGraphics);
        let ipc = AttachPaneImage::from(&img);
        assert_eq!(
            ipc.compression,
            bmux_ipc::compression::CompressionId::None,
            "pre-compressed PNG should not be compressed again"
        );
        assert_eq!(ipc.raw_data, raw);
    }
}
