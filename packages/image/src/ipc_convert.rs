//! Conversions between `bmux_image` types and `bmux_ipc` transport types.
//!
//! Keeps the IPC crate independent of the image crate while allowing
//! the server and client to convert between domain types and wire types.

use crate::model::{ImagePayload, ImageProtocol, PaneImage};
use bmux_ipc::{AttachImageProtocol, AttachPaneImage, AttachPaneImageDelta};

// ---------------------------------------------------------------------------
// PaneImage -> AttachPaneImage (server sends to client)
// ---------------------------------------------------------------------------

impl From<&PaneImage> for AttachPaneImage {
    fn from(img: &PaneImage) -> Self {
        Self {
            id: img.id,
            protocol: protocol_to_ipc(img.protocol),
            raw_data: img.payload.raw.clone().unwrap_or_default(),
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
        Self {
            id: ipc.id,
            protocol: protocol_from_ipc(ipc.protocol),
            payload: ImagePayload {
                raw: if ipc.raw_data.is_empty() {
                    None
                } else {
                    Some(ipc.raw_data.clone())
                },
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
