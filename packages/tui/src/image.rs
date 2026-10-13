//! Protocol-neutral terminal image contributions.

use crate::geometry::Rect;

/// Stable caller-owned identity for an image contribution.
///
/// A key identifies the same logical image across frames. Reusing a key with
/// different content or placement updates that image rather than adding a
/// second image.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ImagePixelFormat {
    /// Three bytes per pixel in red, green, blue order.
    Rgb8,
    /// Four bytes per pixel in red, green, blue, alpha order.
    Rgba8,
}

/// Protocol-neutral image payload.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ImageLifecycle {
    /// Keep the image only while its key is contributed in each frame.
    Frame,
    /// Keep the image across frames until it is updated or explicitly removed.
    Persistent,
}

/// How an image uses its allocated rectangle.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ImageFit {
    /// Fill the allocation, permitting a change in aspect ratio.
    #[default]
    Stretch,
    /// Preserve aspect ratio and center inside the allocation.
    Contain,
}

/// Unclipped cell-space allocation. Signed origins retain offscreen geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ImageBounds {
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
}

impl ImageBounds {
    /// Check that retained allocation is nonempty and intersects its visible destination.
    #[must_use]
    pub fn covers(&self, visible: Rect) -> bool {
        self.width != 0
            && self.height != 0
            && i64::from(self.x) <= i64::from(visible.x)
            && i64::from(self.y) <= i64::from(visible.y)
            && i64::from(self.x) + i64::from(self.width) >= i64::from(visible.right())
            && i64::from(self.y) + i64::from(self.height) >= i64::from(visible.bottom())
    }
}

impl From<Rect> for ImageBounds {
    fn from(rect: Rect) -> Self {
        Self {
            x: i32::from(rect.x),
            y: i32::from(rect.y),
            width: rect.width,
            height: rect.height,
        }
    }
}

/// A pixel-space rectangle fitted before applying visibility clipping.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImagePixelRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl ImagePlacement {
    /// Fit using this output's cell metrics, never the visible clip's dimensions.
    #[must_use]
    pub fn pixel_rect(&self, cell_width: f64, cell_height: f64) -> ImagePixelRect {
        let bounds = self.allocation.unwrap_or_else(|| self.destination.into());
        let mut rect = ImagePixelRect {
            x: f64::from(bounds.x) * cell_width,
            y: f64::from(bounds.y) * cell_height,
            width: f64::from(bounds.width) * cell_width,
            height: f64::from(bounds.height) * cell_height,
        };
        if self.fit == ImageFit::Contain {
            let (width, height) = match self.payload {
                ImagePayload::Png { width, height, .. }
                | ImagePayload::Pixels { width, height, .. } => (width, height),
            };
            if width == 0 || height == 0 {
                return ImagePixelRect {
                    width: 0.0,
                    height: 0.0,
                    ..rect
                };
            }
            let scale = (rect.width / f64::from(width)).min(rect.height / f64::from(height));
            let fitted_width = f64::from(width) * scale;
            let fitted_height = f64::from(height) * scale;
            rect.x += (rect.width - fitted_width) / 2.0;
            rect.y += (rect.height - fitted_height) / 2.0;
            rect.width = fitted_width;
            rect.height = fitted_height;
        }
        rect
    }
}

/// A protocol-neutral image and its cell-space placement.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ImagePlacement {
    /// Stable caller-owned identity.
    pub key: ImageKey,
    /// Encoded or decoded image content.
    pub payload: ImagePayload,
    /// Destination rectangle in terminal cell coordinates.
    pub destination: Rect,
    /// Output-specific fitting policy. Missing wire fields retain legacy stretch.
    #[cfg_attr(feature = "serde", serde(default))]
    pub fit: ImageFit,
    /// Original allocation before paint clipping; absent for legacy placements.
    #[cfg_attr(feature = "serde", serde(default))]
    pub allocation: Option<ImageBounds>,
    /// Rectangle to which image output must be clipped.
    pub clip: Rect,
    /// Requested frame-to-frame lifetime.
    pub lifecycle: ImageLifecycle,
}

/// One image lifecycle contribution emitted while rendering a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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
    fn contain_uses_pixel_metrics_and_ignores_clip() {
        let mut placement = ImagePlacement {
            fit: super::ImageFit::Contain,
            allocation: None,
            key: ImageKey::new("fit"),
            payload: ImagePayload::Pixels {
                bytes: vec![],
                width: 100,
                height: 100,
                format: super::ImagePixelFormat::Rgba8,
            },
            destination: Rect::new(0, 0, 40, 12),
            clip: Rect::new(0, 0, 40, 12),
            lifecycle: ImageLifecycle::Frame,
        };
        for (width, height) in [(100, 100), (1100, 350), (100, 800)] {
            placement.payload = ImagePayload::Png {
                bytes: vec![],
                width,
                height,
            };
            for (cw, ch) in [(8.0, 16.0), (10.0, 22.0), (16.0, 16.0)] {
                let fit = placement.pixel_rect(cw, ch);
                assert!(
                    (fit.width / fit.height - f64::from(width) / f64::from(height)).abs() < 1e-10
                );
                assert!(fit.width <= 40.0 * cw && fit.height <= 12.0 * ch);
                placement.clip = Rect::new(0, 4, 40, 3);
                assert_eq!(fit, placement.pixel_rect(cw, ch));
            }
        }
        placement.payload = ImagePayload::Png {
            bytes: vec![],
            width: 100,
            height: 100,
        };
        let before = placement.pixel_rect(8.0, 16.0);
        placement.allocation = Some(super::ImageBounds {
            x: 0,
            y: -4,
            width: 40,
            height: 12,
        });
        placement.destination = Rect::new(0, 0, 40, 8);
        let scrolled = placement.pixel_rect(8.0, 16.0);
        assert!((before.width - scrolled.width).abs() < 1e-10);
        assert!((before.height - scrolled.height).abs() < 1e-10);
        assert!((before.y - 64.0 - scrolled.y).abs() < 1e-10);
    }

    #[test]
    fn contribution_preserves_protocol_neutral_placement() {
        let contribution = ImageContribution::Present(ImagePlacement {
            fit: crate::image::ImageFit::Stretch,
            allocation: None,
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
