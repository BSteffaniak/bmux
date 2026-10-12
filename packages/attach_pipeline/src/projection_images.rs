//! Bounded image projection using compositor visibility and authoritative content geometry.
use bmux_tui::geometry::Rect;
use bmux_tui::image::{ImageKey, ImageLifecycle, ImagePayload, ImagePixelFormat, ImagePlacement};

/// Maximum decoded pixels for any one observed resource.
pub const MAX_IMAGE_PIXELS: u64 = 4 * 1024 * 1024;

/// Crop one resource into visible strips. The callback uses the retained compositor,
/// not an independently reconstructed overlay model.
///
/// # Errors
/// Rejects malformed or oversized resources before pixel allocation.
pub fn visible_image_strips(
    key: &str,
    payload: &ImagePayload,
    destination: Rect,
    clip: Rect,
    visible: impl Fn(u16, u16) -> bool,
) -> Result<Vec<ImagePlacement>, String> {
    let (width, height) = match payload {
        ImagePayload::Png { width, height, .. } | ImagePayload::Pixels { width, height, .. } => {
            (*width, *height)
        }
    };
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS {
        return Err("image pixel budget exceeded".to_string());
    }
    let rgba = match payload {
        ImagePayload::Png { bytes, .. } => {
            if bytes.len() > 16 * 1024 * 1024 {
                return Err("encoded image budget exceeded".to_string());
            }
            let mut reader = image::ImageReader::with_format(
                std::io::Cursor::new(bytes),
                image::ImageFormat::Png,
            );
            let mut limits = image::Limits::default();
            limits.max_image_width = Some(width);
            limits.max_image_height = Some(height);
            limits.max_alloc = Some(MAX_IMAGE_PIXELS * 8);
            reader.limits(limits);
            let decoded = reader.decode().map_err(|error| error.to_string())?;
            if decoded.width() != width || decoded.height() != height {
                return Err("image dimensions mismatch".to_string());
            }
            decoded.to_rgba8()
        }
        ImagePayload::Pixels { bytes, format, .. } => match format {
            ImagePixelFormat::Rgba8 => image::RgbaImage::from_raw(width, height, bytes.clone())
                .ok_or("invalid RGBA length")?,
            ImagePixelFormat::Rgb8 => image::DynamicImage::ImageRgb8(
                image::RgbImage::from_raw(width, height, bytes.clone())
                    .ok_or("invalid RGB length")?,
            )
            .to_rgba8(),
        },
    };
    let clipped = destination.intersection(clip);
    if destination.is_empty() || clipped.is_empty() {
        return Ok(Vec::new());
    }
    let mut strips = Vec::new();
    for y in clipped.y..clipped.bottom() {
        let mut x = clipped.x;
        while x < clipped.right() {
            if !visible(x, y) {
                x += 1;
                continue;
            }
            let start = x;
            while x < clipped.right() && visible(x, y) {
                x += 1;
            }
            let left =
                u64::from(start - destination.x) * u64::from(width) / u64::from(destination.width);
            let right =
                u64::from(x - destination.x) * u64::from(width) / u64::from(destination.width);
            let top =
                u64::from(y - destination.y) * u64::from(height) / u64::from(destination.height);
            let bottom = u64::from(y + 1 - destination.y) * u64::from(height)
                / u64::from(destination.height);
            let left = u32::try_from(left).map_err(|error| error.to_string())?;
            let top = u32::try_from(top).map_err(|error| error.to_string())?;
            let crop_width = u32::try_from(right)
                .map_err(|error| error.to_string())?
                .saturating_sub(left);
            let crop_height = u32::try_from(bottom)
                .map_err(|error| error.to_string())?
                .saturating_sub(top);
            if crop_width == 0 || crop_height == 0 {
                continue;
            }
            let pixels =
                image::imageops::crop_imm(&rgba, left, top, crop_width, crop_height).to_image();
            let rect = Rect::new(start, y, x - start, 1);
            strips.push(ImagePlacement {
                key: ImageKey::new(format!("{key}:{start}:{y}")),
                payload: ImagePayload::Pixels {
                    bytes: pixels.into_raw(),
                    width: crop_width,
                    height: crop_height,
                    format: ImagePixelFormat::Rgba8,
                },
                destination: rect,
                clip: rect,
                lifecycle: ImageLifecycle::Frame,
            });
            if strips.len() > 4096 {
                return Err("image placement budget exceeded".to_string());
            }
        }
    }
    Ok(strips)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn content_origin_and_occlusion_crop_pixels_together() {
        let payload = ImagePayload::Pixels {
            bytes: vec![255; 8 * 4 * 4],
            width: 8,
            height: 4,
            format: ImagePixelFormat::Rgba8,
        };
        let strips = visible_image_strips(
            "owned",
            &payload,
            Rect::new(7, 3, 4, 2),
            Rect::new(8, 3, 3, 2),
            |x, _| x != 9,
        )
        .unwrap();
        assert_eq!(strips.len(), 4);
        assert!(
            strips
                .iter()
                .all(|image| image.destination.x != 9 && image.destination.x >= 8)
        );
        assert!(strips.iter().all(|image| image.clip == image.destination));
    }
}
