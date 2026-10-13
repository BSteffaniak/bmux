//! Native Unix clipboard transports. No helper process or live display is used by tests.
//!
//! XCB ABI reviewed against libxcb 1.17 xcb.h/xproto.h (cookies are one u32,
//! replies/events use the generated fixed-width X protocol layout). Wayland ABI
//! references and protocol descriptors are in `wayland`.
use super::NativeImage;
use crate::ClipboardError;
use std::io::Cursor;

mod wayland;
mod x11;

pub const LIMIT: usize = 256 * 1024 * 1024;
pub fn failure(backend: &str, message: String) -> ClipboardError {
    ClipboardError::BackendFailed {
        backend: backend.into(),
        message,
    }
}

fn write(bytes: Vec<u8>, image: bool) -> Result<(), ClipboardError> {
    if bytes.len() > LIMIT {
        return Err(failure(
            "native-unix",
            "clipboard exceeds 256 MiB transfer limit".to_string(),
        ));
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        match wayland::write(bytes.clone(), image) {
            Ok(()) => return Ok(()),
            Err(wayland_error) => {
                return x11::write(bytes, image).map_err(|x11_error| {
                    failure("native-unix", format!("{wayland_error}; {x11_error}"))
                });
            }
        }
    }
    x11::write(bytes, image)
}
fn read(image: bool) -> Result<Option<Vec<u8>>, ClipboardError> {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        match wayland::read(image) {
            Ok(result) => return Ok(result),
            Err(wayland_error) => {
                return x11::read(image).map_err(|x11_error| {
                    failure("native-unix", format!("{wayland_error}; {x11_error}"))
                });
            }
        }
    }
    x11::read(image)
}
pub fn copy_text(text: &str) -> Result<(), ClipboardError> {
    write(text.as_bytes().to_vec(), false)
}
pub fn read_text() -> Result<Option<String>, ClipboardError> {
    read(false)?
        .map(|bytes| String::from_utf8(bytes).map_err(|e| failure("native-unix", e.to_string())))
        .transpose()
}
pub fn copy_image(image: &NativeImage) -> Result<(), ClipboardError> {
    use image::ImageEncoder as _;
    let (width, height) = (u32::try_from(image.width), u32::try_from(image.height));
    let (Ok(width), Ok(height)) = (width, height) else {
        return Err(failure(
            "native-unix",
            "image dimensions overflow".to_string(),
        ));
    };
    if width == 0
        || height == 0
        || image
            .width
            .checked_mul(image.height)
            .and_then(|n| n.checked_mul(4))
            != Some(image.bytes.len())
        || image.bytes.len() > LIMIT
    {
        return Err(failure(
            "native-unix",
            "invalid or oversized RGBA image".to_string(),
        ));
    }
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(&image.bytes, width, height, image::ExtendedColorType::Rgba8)
        .map_err(|e| failure("native-unix", e.to_string()))?;
    write(bytes, true)
}
pub fn read_image() -> Result<Option<NativeImage>, ClipboardError> {
    let Some(bytes) = read(true)? else {
        return Ok(None);
    };
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(LIMIT as u64);
    reader.limits(limits);
    let decoded = reader
        .decode()
        .map_err(|e| failure("native-unix", e.to_string()))?;
    checked_rgba_size(decoded.width(), decoded.height())?;
    let image = decoded.into_rgba8();
    Ok(Some(NativeImage {
        width: image.width() as usize,
        height: image.height() as usize,
        bytes: image.into_raw(),
    }))
}

fn checked_rgba_size(width: u32, height: u32) -> Result<usize, ClipboardError> {
    let bytes = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4));
    match bytes.and_then(|bytes| usize::try_from(bytes).ok()) {
        Some(bytes) if bytes <= LIMIT => Ok(bytes),
        _ => Err(failure(
            "native-unix",
            "decoded RGBA image exceeds transfer limit".to_string(),
        )),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rgba_expansion_bound_precedes_conversion() {
        assert_eq!(checked_rgba_size(2, 2).unwrap(), 16);
        assert!(checked_rgba_size(100_000, 100_000).is_err());
        assert!(checked_rgba_size(u32::MAX, u32::MAX).is_err());
    }
}
