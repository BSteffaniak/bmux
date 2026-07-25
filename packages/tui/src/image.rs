//! Protocol-neutral terminal image contributions.

use crate::geometry::Rect;

/// Stable caller-owned identity for an image contribution.
///
/// A key identifies the same logical image across frames. Reusing a key with
/// different content or placement updates that image rather than adding a
/// second image.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ImageKey(String);

impl ImageKey {
    /// Create an image key from caller-owned text.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Return the key text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for ImageKey {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for ImageKey {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

/// Pixel encoding supplied by a TUI image producer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImagePixelFormat {
    /// Three bytes per pixel in red, green, blue order.
    Rgb8,
    /// Four bytes per pixel in red, green, blue, alpha order.
    Rgba8,
}

/// Protocol-neutral image payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImagePayload {
    /// PNG-encoded image bytes and their decoded dimensions.
    Png {
        /// Encoded PNG bytes.
        bytes: Vec<u8>,
        /// Decoded pixel width.
        width: u32,
        /// Decoded pixel height.
        height: u32,
    },
    /// Uncompressed pixels and their dimensions.
    Pixels {
        /// Pixel bytes in `format` order.
        bytes: Vec<u8>,
        /// Pixel width.
        width: u32,
        /// Pixel height.
        height: u32,
        /// Pixel byte format.
        format: ImagePixelFormat,
    },
}

/// Lifetime requested for a presented image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageLifecycle {
    /// Keep the image only while its key is contributed in each frame.
    Frame,
    /// Keep the image across frames until it is updated or explicitly removed.
    Persistent,
}

/// A protocol-neutral image and its cell-space placement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePlacement {
    /// Stable caller-owned identity.
    pub key: ImageKey,
    /// Encoded or decoded image content.
    pub payload: ImagePayload,
    /// Destination rectangle in terminal cell coordinates.
    pub destination: Rect,
    /// Rectangle to which image output must be clipped.
    pub clip: Rect,
    /// Requested frame-to-frame lifetime.
    pub lifecycle: ImageLifecycle,
}

/// One image lifecycle contribution emitted while rendering a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageContribution {
    /// Add or update an image and its placement.
    Present(ImagePlacement),
    /// Remove the image identified by this stable key.
    Remove(ImageKey),
}

#[cfg(test)]
mod tests {
    use super::{ImageContribution, ImageKey, ImageLifecycle, ImagePayload, ImagePlacement};
    use crate::geometry::Rect;

    #[test]
    fn contribution_preserves_protocol_neutral_placement() {
        let contribution = ImageContribution::Present(ImagePlacement {
            key: ImageKey::new("diagram:turn-1"),
            payload: ImagePayload::Png {
                bytes: vec![1, 2, 3],
                width: 320,
                height: 200,
            },
            destination: Rect::new(4, 6, 40, 10),
            clip: Rect::new(0, 2, 80, 20),
            lifecycle: ImageLifecycle::Frame,
        });

        let ImageContribution::Present(placement) = contribution else {
            panic!("expected a presented image");
        };
        assert_eq!(placement.key.as_str(), "diagram:turn-1");
        assert_eq!(placement.destination, Rect::new(4, 6, 40, 10));
        assert_eq!(placement.clip, Rect::new(0, 2, 80, 20));
        assert_eq!(placement.lifecycle, ImageLifecycle::Frame);
    }
}
