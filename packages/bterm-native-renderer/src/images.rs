//! Bounded image decoding. Visible geometry is supplied solely by PreparedPresentation.
use crate::renderer::RenderError;
use bmux_tui::image::{ImagePayload, ImagePixelFormat};

const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_IMAGE_EDGE: u32 = 4096;

pub(crate) fn decode(payload: &ImagePayload) -> Result<(u32, u32, Vec<u8>), RenderError> {
    let (width, height) = match payload {
        ImagePayload::Png { width, height, .. } | ImagePayload::Pixels { width, height, .. } => {
            (*width, *height)
        }
    };
    if width == 0 || height == 0 || width > MAX_IMAGE_EDGE || height > MAX_IMAGE_EDGE {
        return Err(RenderError::Capacity(
            "image dimensions exceed upload limit",
        ));
    }
    let length = width as usize * height as usize * 4;
    if length > MAX_IMAGE_BYTES {
        return Err(RenderError::Capacity("image exceeds byte limit"));
    }
    let rgba = match payload {
        ImagePayload::Pixels {
            bytes,
            format: ImagePixelFormat::Rgba8,
            ..
        } => {
            if bytes.len() != length {
                return Err(RenderError::Image("invalid RGBA payload length".into()));
            }
            bytes.clone()
        }
        ImagePayload::Pixels {
            bytes,
            format: ImagePixelFormat::Rgb8,
            ..
        } => {
            if bytes.len() != width as usize * height as usize * 3 {
                return Err(RenderError::Image("invalid RGB payload length".into()));
            }
            let mut rgba = Vec::with_capacity(length);
            for pixel in bytes.as_chunks::<3>().0 {
                rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
            }
            rgba
        }
        ImagePayload::Png { bytes, .. } => {
            if bytes.len() > MAX_IMAGE_BYTES {
                return Err(RenderError::Capacity("encoded image exceeds byte limit"));
            }
            let mut reader = image::ImageReader::with_format(
                std::io::Cursor::new(bytes),
                image::ImageFormat::Png,
            );
            let mut limits = image::Limits::default();
            limits.max_image_width = Some(MAX_IMAGE_EDGE);
            limits.max_image_height = Some(MAX_IMAGE_EDGE);
            limits.max_alloc = Some(MAX_IMAGE_BYTES as u64);
            reader.limits(limits);
            let image = reader
                .decode()
                .map_err(|e| RenderError::Image(e.to_string()))?;
            if image.width() != width || image.height() != height {
                return Err(RenderError::Image(
                    "PNG declared dimensions differ from decoded dimensions".into(),
                ));
            }
            image.into_rgba8().into_raw()
        }
    };
    Ok((width, height, rgba))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rgb_is_opaque_and_bad_lengths_rejected() {
        let payload = ImagePayload::Pixels {
            bytes: vec![10, 20, 30],
            width: 1,
            height: 1,
            format: ImagePixelFormat::Rgb8,
        };
        assert_eq!(decode(&payload).unwrap().2, [10, 20, 30, 255]);
        assert!(
            decode(&ImagePayload::Pixels {
                bytes: vec![],
                width: 1,
                height: 1,
                format: ImagePixelFormat::Rgba8
            })
            .is_err()
        );
    }
    #[test]
    fn huge_declared_images_rejected_before_decode() {
        assert!(
            decode(&ImagePayload::Png {
                bytes: vec![],
                width: u32::MAX,
                height: 1
            })
            .is_err()
        );
    }
}
