//! Pixel-buffer operations shared by terminal presentation and image interchange.
//! No resource, surface, clipboard or compositor policy belongs here.

#[cfg(feature = "formats")]
mod formats;
mod png;
pub mod transforms;

pub use png::{encode_png, validate_png};

/// A tightly packed, top-to-bottom, straight-alpha RGBA8 raster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Raster {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Decode an encoded raster using its signature, subject to caller-specified limits.
/// PNG is always supported; other interchange formats require the `formats` feature.
///
/// # Errors
/// Returns an error for unsupported formats, invalid data or exceeded limits.
pub fn decode(
    bytes: &[u8],
    max_width: u32,
    max_height: u32,
    max_alloc: u64,
) -> Result<Raster, String> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return png::decode_png(bytes, max_width, max_height, max_alloc);
    }
    #[cfg(feature = "formats")]
    {
        formats::decode_other(bytes, max_width, max_height, max_alloc)
    }
    #[cfg(not(feature = "formats"))]
    {
        Err("unsupported image format".to_owned())
    }
}

pub use png::png_dimensions;

/// Decode and normalize a source image to PNG, preserving native PNG sample precision.
/// # Errors
/// Rejects invalid images and dimensions or allocations outside the supplied budget.
pub fn transcode_png(
    bytes: &[u8],
    max_width: u32,
    max_height: u32,
    max_alloc: u64,
) -> Result<Vec<u8>, String> {
    transcode_png_staged(bytes, max_width, max_height, max_alloc).map_err(|e| match e {
        TranscodeError::Decode(message) | TranscodeError::Encode(message) => message,
    })
}

/// Distinguishes unsupported input from failure writing the replacement representation.
#[derive(Debug)]
pub enum TranscodeError {
    Decode(String),
    Encode(String),
}
impl From<String> for TranscodeError {
    fn from(message: String) -> Self {
        Self::Decode(message)
    }
}

/// Normalize while preserving the stage at which a failure occurred.
/// # Errors
/// Returns a decode or encode error as appropriate.
pub fn transcode_png_staged(
    bytes: &[u8],
    max_width: u32,
    max_height: u32,
    max_alloc: u64,
) -> Result<Vec<u8>, TranscodeError> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return png::transcode_native_png(bytes, max_width, max_height, max_alloc);
    }
    #[cfg(feature = "formats")]
    {
        formats::transcode_other(bytes, max_width, max_height, max_alloc)
    }
    #[cfg(not(feature = "formats"))]
    {
        Err(TranscodeError::Decode("unsupported image format".into()))
    }
}
