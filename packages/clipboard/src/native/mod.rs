//! Private native clipboard boundary. Public clipboard policy stays in the facade.

use crate::ClipboardError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeImage {
    pub(super) width: usize,
    pub(super) height: usize,
    /// Straight (not premultiplied) RGBA pixels, in top-to-bottom row order.
    pub(super) bytes: Vec<u8>,
}

#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
))]
mod unix;
#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
))]
use unix as platform;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use windows as platform;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as platform;

#[cfg(not(any(
    target_os = "windows",
    target_os = "macos",
    all(unix, not(any(target_os = "android", target_os = "emscripten")))
)))]
mod platform {
    use super::{ClipboardError, NativeImage};

    fn unavailable() -> ClipboardError {
        ClipboardError::BackendUnavailable {
            os: std::env::consts::OS.to_string(),
        }
    }
    pub fn copy_text(_: &str) -> Result<(), ClipboardError> {
        Err(unavailable())
    }
    pub fn read_text() -> Result<Option<String>, ClipboardError> {
        Err(unavailable())
    }
    pub fn copy_image(_: &NativeImage) -> Result<(), ClipboardError> {
        Err(unavailable())
    }
    pub fn read_image() -> Result<Option<NativeImage>, ClipboardError> {
        Err(unavailable())
    }
}

pub fn copy_text(text: &str) -> Result<(), ClipboardError> {
    platform::copy_text(text)
}

pub fn read_text() -> Result<Option<String>, ClipboardError> {
    platform::read_text()
}

pub fn copy_image(image: &NativeImage) -> Result<(), ClipboardError> {
    validate_image(image)?;
    platform::copy_image(image)
}

pub fn read_image() -> Result<Option<NativeImage>, ClipboardError> {
    let image = platform::read_image()?;
    if let Some(image) = &image {
        validate_image(image)?;
    }
    Ok(image)
}

fn validate_image(image: &NativeImage) -> Result<(), ClipboardError> {
    let len = image
        .width
        .checked_mul(image.height)
        .and_then(|pixels| pixels.checked_mul(4));
    if image.width == 0 || image.height == 0 || len != Some(image.bytes.len()) {
        return Err(ClipboardError::BackendFailed {
            backend: "native".to_string(),
            message: "invalid RGBA clipboard image dimensions or buffer".to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires BMUX_CLIPBOARD_ISOLATED_TEST=1 and a disposable native clipboard"]
    fn isolated_native_roundtrip_without_command_fallback() {
        assert_eq!(
            std::env::var("BMUX_CLIPBOARD_ISOLATED_TEST").as_deref(),
            Ok("1")
        );
        for text in ["", "Unicode 界 🦀 e\u{301}\n", "trailing newline\n"] {
            copy_text(text).unwrap();
            assert_eq!(read_text().unwrap().as_deref(), Some(text));
        }
        let text = "large 界\n".repeat(200_000);
        copy_text(&text).unwrap();
        assert_eq!(read_text().unwrap(), Some(text));
        let image = NativeImage {
            width: 2,
            height: 2,
            bytes: vec![255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 0, 1, 2, 3, 255],
        };
        copy_image(&image).unwrap();
        assert_eq!(read_image().unwrap(), Some(image));
    }

    #[test]
    fn image_validation_checks_dimensions_and_overflow() {
        for (width, height, bytes) in [(0, 1, vec![]), (1, 1, vec![0; 3]), (usize::MAX, 2, vec![])]
        {
            assert!(
                validate_image(&NativeImage {
                    width,
                    height,
                    bytes
                })
                .is_err()
            );
        }
        assert!(
            validate_image(&NativeImage {
                width: 1,
                height: 1,
                bytes: vec![1, 2, 3, 4]
            })
            .is_ok()
        );
    }
}
