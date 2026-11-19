//! Native Unix clipboard transports over pure-Rust protocol clients.
//!
//! X11 uses x11rb's typed protocol; Wayland uses generated ext/wlr data-control
//! protocols. Ignored interoperability tests require explicitly isolated displays.
use super::NativeImage;
use crate::ClipboardError;

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
        // In a Wayland-only session there is no X11 fallback. Preserve the
        // transport error and avoid cloning a potentially 256 MiB payload.
        if std::env::var_os("DISPLAY").is_none() {
            return wayland::write(bytes, image);
        }
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
        if std::env::var_os("DISPLAY").is_none() {
            return wayland::read(image);
        }
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
    let bytes = bmux_raster::encode_png(width, height, 4, &image.bytes)
        .map_err(|e| failure("native-unix", e))?;
    write(bytes, true)
}
pub fn read_image() -> Result<Option<NativeImage>, ClipboardError> {
    let Some(bytes) = read(true)? else {
        return Ok(None);
    };
    if !crate::is_png(&bytes) {
        return Err(failure("native-unix", "invalid PNG signature".into()));
    }
    let image = bmux_raster::decode(&bytes, u32::MAX, u32::MAX, LIMIT as u64)
        .map_err(|e| failure("native-unix", e))?;
    checked_rgba_size(image.width, image.height)?;
    Ok(Some(NativeImage {
        width: image.width as usize,
        height: image.height as usize,
        bytes: image.rgba,
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
/// Explicit normal-exit clipboard persistence, also invoked by X11's exit hook.
/// This does not promise persistence after SIGKILL, abort, or without a manager.
pub fn shutdown() -> Result<(), ClipboardError> {
    x11::shutdown()
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
