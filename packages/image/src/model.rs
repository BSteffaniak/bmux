//! Core data model for terminal image protocol support.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Protocol identification
// ---------------------------------------------------------------------------

/// Which terminal image protocol produced (or should consume) an image.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ImageProtocol {
    Sixel,
    KittyGraphics,
    ITerm2,
}

// ---------------------------------------------------------------------------
// Pixel buffer (decoded image data, protocol-agnostic)
// ---------------------------------------------------------------------------

/// Raw pixel format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PixelFormat {
    /// 3 bytes per pixel (R, G, B).
    Rgb8,
    /// 4 bytes per pixel (R, G, B, A).
    Rgba8,
    /// Compressed PNG bytes.
    Png,
}

/// A decoded pixel buffer, independent of any terminal protocol.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PixelBuffer {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    /// Raw pixel data, or PNG-compressed bytes when `format == Png`.
    pub data: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Image payload (may carry raw protocol bytes, decoded pixels, or both)
// ---------------------------------------------------------------------------

/// How the image data is represented in transit / storage.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ImagePayload {
    /// Original protocol bytes (sixel DCS body, kitty APC body, iTerm2 OSC
    /// body).  Present in client-decode and passthrough modes.
    pub raw: Option<Vec<u8>>,
    /// Decoded pixel buffer.  Present in server-decode mode.
    pub pixels: Option<PixelBuffer>,
}

// ---------------------------------------------------------------------------
// Image position & size (pane-local coordinates)
// ---------------------------------------------------------------------------

/// Position within a pane's inner content area (0-indexed cell coordinates).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImagePosition {
    pub row: u16,
    pub col: u16,
}

/// How many cells an image occupies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageCellSize {
    pub rows: u16,
    pub cols: u16,
}

/// Original pixel dimensions of the image source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImagePixelSize {
    pub width: u32,
    pub height: u32,
}

// ---------------------------------------------------------------------------
// PaneImage — a single image placed within a pane
// ---------------------------------------------------------------------------

/// A fully-described image placed within a pane's content area.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaneImage {
    /// Monotonically increasing per-pane identifier.
    pub id: u64,
    /// Which protocol produced this image.
    pub protocol: ImageProtocol,
    /// The image data (raw bytes, decoded pixels, or both).
    pub payload: ImagePayload,
    /// Where the image is placed (pane-local cell coordinates).
    pub position: ImagePosition,
    /// How many cells the image occupies.
    pub cell_size: ImageCellSize,
    /// Original pixel dimensions.
    pub pixel_size: ImagePixelSize,
}

// ---------------------------------------------------------------------------
// Delta transport (incremental IPC updates)
// ---------------------------------------------------------------------------

/// Incremental update sent from server to client.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ImageDelta {
    /// Images added since the last delta.
    pub added: Vec<PaneImage>,
    /// Image IDs removed since the last delta.
    pub removed: Vec<u64>,
    /// Monotonic sequence number for tracking.
    pub sequence: u64,
}

// ---------------------------------------------------------------------------
// Kitty-specific stateful types
// ---------------------------------------------------------------------------

/// Kitty image data format.
#[cfg(feature = "kitty")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KittyFormat {
    Rgb,
    Rgba,
    Png,
}

/// A transmitted (uploaded) image in the kitty protocol.
#[cfg(feature = "kitty")]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KittyTransmittedImage {
    pub image_id: u32,
    pub format: KittyFormat,
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// A kitty image placement.
#[cfg(feature = "kitty")]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KittyPlacement {
    pub image_id: u32,
    pub placement_id: u32,
    pub position: ImagePosition,
    pub source_rect: Option<KittySourceRect>,
    pub z_index: i32,
}

/// Sub-region of a kitty image to display.
#[cfg(feature = "kitty")]
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct KittySourceRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

// ---------------------------------------------------------------------------
// Image events (emitted by the interceptor)
// ---------------------------------------------------------------------------

/// Events produced by [`crate::intercept::ImageInterceptor`] when it detects
/// an image sequence in the PTY output stream.
#[derive(Clone, Debug)]
pub enum ImageEvent {
    /// A complete sixel image was extracted.
    #[cfg(feature = "sixel")]
    SixelImage {
        data: Vec<u8>,
        position: ImagePosition,
        pixel_size: ImagePixelSize,
        /// Byte offset in the filtered output where this image's ESC started.
        /// Used by the caller to feed preceding bytes to the cursor tracker
        /// before capturing the cursor position for image placement.
        filtered_byte_offset: usize,
    },

    /// A kitty graphics command was extracted.
    #[cfg(feature = "kitty")]
    KittyCommand {
        command: KittyCommand,
        /// Byte offset in the filtered output where this command's ESC started.
        filtered_byte_offset: usize,
    },

    /// An iTerm2 inline image was extracted.
    #[cfg(feature = "iterm2")]
    ITerm2Image {
        data: Vec<u8>,
        position: ImagePosition,
        /// Byte offset in the filtered output where this image's ESC started.
        filtered_byte_offset: usize,
    },
}

/// Parsed kitty graphics command.
#[cfg(feature = "kitty")]
#[derive(Clone, Debug)]
pub enum KittyCommand {
    /// Transmit image data (action=t or action=T).
    Transmit {
        image_id: u32,
        format: KittyFormat,
        data: Vec<u8>,
        width: u32,
        height: u32,
        more_chunks: bool,
    },
    /// Place a previously transmitted image (action=p).
    Place(KittyPlacement),
    /// Delete image(s) (action=d).
    Delete {
        /// What to delete (image id, placement id, or all).
        specifier: KittyDeleteSpecifier,
    },
    /// Query (action=q) — not stored, only forwarded.
    Query { image_id: u32 },
}

/// What a kitty delete command targets.
#[cfg(feature = "kitty")]
#[derive(Clone, Debug)]
pub enum KittyDeleteSpecifier {
    All,
    ByImageId(u32),
    ByPlacementId { image_id: u32, placement_id: u32 },
}

impl ImageEvent {
    /// Get the byte offset in the filtered output where this image started.
    pub fn filtered_byte_offset(&self) -> usize {
        match self {
            #[cfg(feature = "sixel")]
            Self::SixelImage {
                filtered_byte_offset,
                ..
            } => *filtered_byte_offset,
            #[cfg(feature = "kitty")]
            Self::KittyCommand {
                filtered_byte_offset,
                ..
            } => *filtered_byte_offset,
            #[cfg(feature = "iterm2")]
            Self::ITerm2Image {
                filtered_byte_offset,
                ..
            } => *filtered_byte_offset,
        }
    }

    /// Set the image position (used by the caller after resolving cursor pos).
    pub fn set_position(&mut self, pos: ImagePosition) {
        match self {
            #[cfg(feature = "sixel")]
            Self::SixelImage { position, .. } => *position = pos,
            #[cfg(feature = "kitty")]
            Self::KittyCommand { .. } => {}
            #[cfg(feature = "iterm2")]
            Self::ITerm2Image { position, .. } => *position = pos,
        }
    }
}
