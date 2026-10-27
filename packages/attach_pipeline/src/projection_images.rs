//! Bounded image projection using compositor visibility and authoritative content geometry.
use bmux_tui::geometry::Rect;
use bmux_tui::image::{ImageKey, ImageLifecycle, ImagePayload, ImagePixelFormat, ImagePlacement};
use image::ImageEncoder;
use std::collections::BTreeMap;

/// Maximum decoded pixels for any one observed resource.
pub const MAX_IMAGE_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_PLACEMENTS: usize = 4096;
const MAX_PROJECTED_BYTES: usize = 8 * 1024 * 1024;

/// Losslessly compact large raw pixel resources before clip expansion/capture cloning.
///
/// # Errors
/// Rejects inconsistent dimensions and resources outside decoded-pixel bounds.
pub fn compact_payload(
    payload: &ImagePayload,
) -> Result<std::borrow::Cow<'_, ImagePayload>, String> {
    let ImagePayload::Pixels {
        bytes,
        width,
        height,
        format,
    } = payload
    else {
        return Ok(std::borrow::Cow::Borrowed(payload));
    };
    let pixels = u64::from(*width) * u64::from(*height);
    let channels = match format {
        ImagePixelFormat::Rgb8 => 3,
        ImagePixelFormat::Rgba8 => 4,
    };
    if pixels == 0
        || pixels > MAX_IMAGE_PIXELS
        || pixels.checked_mul(channels) != Some(bytes.len() as u64)
    {
        return Err(format!(
            "image dimensions/pixel admission failed: dimensions={width}x{height}, pixels={pixels}, pixel_limit={MAX_IMAGE_PIXELS}, payload_bytes={}",
            bytes.len()
        ));
    }
    if bytes.len() < 256 * 1024 {
        return Ok(std::borrow::Cow::Borrowed(payload));
    }
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(
            bytes,
            *width,
            *height,
            match format {
                ImagePixelFormat::Rgb8 => image::ExtendedColorType::Rgb8,
                ImagePixelFormat::Rgba8 => image::ExtendedColorType::Rgba8,
            },
        )
        .map_err(|_| "lossless image resource encoding failed".to_owned())?;
    if png.len() >= bytes.len() {
        return Ok(std::borrow::Cow::Borrowed(payload));
    }
    Ok(std::borrow::Cow::Owned(ImagePayload::Png {
        bytes: png,
        width: *width,
        height: *height,
    }))
}

/// Project a resource through compositor visibility.
///
/// Retain its full sampling destination and payload. Cell clipping must not
/// round into source texels: stretched resources paint every visible cell.
///
/// # Errors
/// Rejects malformed resources and bounded placement/payload budget overflow.
pub fn visible_image_strips(
    key: &str,
    payload: &ImagePayload,
    destination: Rect,
    clip: Rect,
    visible: impl Fn(u16, u16) -> bool,
) -> Result<Vec<ImagePlacement>, String> {
    let payload = compact_payload(payload)?;
    let byte_len = validate_projection_payload(&payload)?;
    let clipped = destination.intersection(clip);
    if destination.is_empty() || clipped.is_empty() {
        return Ok(Vec::new());
    }
    let mut rectangles: Vec<Rect> = Vec::new();
    let mut previous = BTreeMap::new();
    for y in clipped.y..clipped.bottom() {
        let mut current = BTreeMap::new();
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
            let span = (start, x - start);
            let index = if let Some(&index) = previous.get(&span) {
                let rect: &mut Rect = &mut rectangles[index];
                rect.height += 1;
                index
            } else {
                if rectangles.len() == MAX_PLACEMENTS {
                    return Err(format!(
                        "image placement budget exceeded: image={key}, placements={}, limit={MAX_PLACEMENTS}",
                        rectangles.len() + 1
                    ));
                }
                rectangles.push(Rect::new(start, y, x - start, 1));
                rectangles.len() - 1
            };
            current.insert(span, index);
        }
        previous = current;
    }
    if byte_len.saturating_mul(rectangles.len()) > MAX_PROJECTED_BYTES {
        return Err(format!(
            "image projection byte budget exceeded: payload_bytes={byte_len}, placements={}, projected_bytes={}, limit={MAX_PROJECTED_BYTES}",
            rectangles.len(),
            byte_len.saturating_mul(rectangles.len())
        ));
    }
    let single = rectangles.len() == 1;
    Ok(rectangles
        .into_iter()
        .map(|rect| ImagePlacement {
            fit: bmux_tui::image::ImageFit::Stretch,
            allocation: None,
            key: ImageKey::new(if single {
                key.to_owned()
            } else {
                format!(
                    "{key}:clip:{}:{}:{}:{}",
                    rect.x - destination.x,
                    rect.y - destination.y,
                    rect.width,
                    rect.height
                )
            }),
            payload: payload.as_ref().clone(),
            destination,
            clip: rect,
            lifecycle: ImageLifecycle::Frame,
        })
        .collect())
}

fn validate_projection_payload(payload: &ImagePayload) -> Result<usize, String> {
    let (width, height, bytes) = match payload {
        ImagePayload::Png {
            width,
            height,
            bytes,
        }
        | ImagePayload::Pixels {
            width,
            height,
            bytes,
            ..
        } => (*width, *height, bytes),
    };
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS {
        return Err(format!(
            "image pixel budget exceeded: dimensions={width}x{height}, pixels={}, limit={MAX_IMAGE_PIXELS}",
            u64::from(width) * u64::from(height)
        ));
    }
    if bytes.len() > MAX_PROJECTED_BYTES {
        return Err(format!(
            "image resource byte budget exceeded: used={}, limit={MAX_PROJECTED_BYTES}",
            bytes.len()
        ));
    }
    match payload {
        ImagePayload::Png { .. } => {
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
                return Err("image dimensions mismatch".to_owned());
            }
        }
        ImagePayload::Pixels { format, .. } => {
            let channels = match format {
                ImagePixelFormat::Rgba8 => 4_u64,
                ImagePixelFormat::Rgb8 => 3_u64,
            };
            if u64::try_from(bytes.len()).map_err(|error| error.to_string())?
                != u64::from(width) * u64::from(height) * channels
            {
                return Err("invalid pixel length".to_owned());
            }
        }
    }
    Ok(bytes.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_payload() -> ImagePayload {
        ImagePayload::Pixels {
            bytes: vec![255; 16],
            width: 2,
            height: 2,
            format: ImagePixelFormat::Rgba8,
        }
    }

    #[test]
    fn scaled_tiny_resource_has_full_coverage_and_move_identity() {
        let payload = tiny_payload();
        let first = Rect::new(4, 4, 12, 6);
        let moved = Rect::new(14, 8, 12, 6);
        let project = |destination| {
            visible_image_strips(
                "owned",
                &payload,
                destination,
                Rect::new(0, 0, 80, 24),
                |_, _| true,
            )
            .unwrap()
        };
        let initial = project(first);
        let next = project(moved);
        assert_eq!(initial.len(), 1);
        assert_eq!(next.len(), 1);
        assert_eq!(initial[0].key, next[0].key);
        assert_eq!(initial[0].payload, payload);
        assert_eq!(next[0].payload, payload);
        assert_eq!(initial[0].destination, first);
        assert_eq!(initial[0].clip, first);
        assert_eq!(next[0].destination, moved);
        assert_eq!(next[0].clip, moved);
    }

    #[test]
    fn content_origin_and_occlusion_clip_without_changing_sampling() {
        let payload = tiny_payload();
        let destination = Rect::new(7, 3, 12, 6);
        let viewport = Rect::new(8, 3, 10, 5);
        let strips = visible_image_strips("owned", &payload, destination, viewport, |x, y| {
            !(x == 10 && (4..6).contains(&y))
        })
        .unwrap();
        for y in destination.y..destination.bottom() {
            for x in destination.x..destination.right() {
                let count = strips
                    .iter()
                    .filter(|p| {
                        x >= p.clip.x && x < p.clip.right() && y >= p.clip.y && y < p.clip.bottom()
                    })
                    .count();
                let expected = x >= viewport.x
                    && x < viewport.right()
                    && y >= viewport.y
                    && y < viewport.bottom()
                    && !(x == 10 && (4..6).contains(&y));
                assert_eq!(count, usize::from(expected), "cell {x},{y}");
            }
        }
        assert!(
            strips
                .iter()
                .all(|p| p.destination == destination && p.payload == payload)
        );
        assert_eq!(
            visible_image_strips(
                "owned",
                &payload,
                destination,
                Rect::new(30, 30, 2, 2),
                |_, _| true
            )
            .unwrap(),
            [] as [bmux_tui::image::ImagePlacement; 0]
        );
        assert_eq!(
            visible_image_strips("owned", &payload, destination, viewport, |_, _| false).unwrap(),
            [] as [bmux_tui::image::ImagePlacement; 0]
        );
    }

    #[test]
    fn four_k_pixels_compact_losslessly_before_clip_expansion() {
        let original = ImagePayload::Pixels {
            bytes: vec![255; 3840 * 2160 * 4],
            width: 3840,
            height: 2160,
            format: ImagePixelFormat::Rgba8,
        };
        let compact = compact_payload(&original).unwrap();
        let ImagePayload::Png { bytes, .. } = compact.as_ref() else {
            panic!("4K resource was not compacted");
        };
        assert!(bytes.len() < 256 * 1024);
        let decoded = image::load_from_memory(bytes).unwrap().to_rgba8();
        assert_eq!(decoded.as_raw(), &vec![255; 3840 * 2160 * 4]);
        let clips = visible_image_strips(
            "4k",
            &original,
            Rect::new(0, 0, 5, 1),
            Rect::new(0, 0, 5, 1),
            |x, _| x % 2 == 0,
        )
        .unwrap();
        assert_eq!(clips.len(), 3);
        assert!(
            clips
                .iter()
                .all(|clip| matches!(clip.payload, ImagePayload::Png { .. }))
        );
    }

    #[test]
    fn projection_budgets_reject_fragmentation_before_payload_cloning() {
        let payload = tiny_payload();
        assert!(
            visible_image_strips(
                "owned",
                &payload,
                Rect::new(0, 0, 130, 130),
                Rect::new(0, 0, 130, 130),
                |x, y| (x + y) % 2 == 0
            )
            .is_err()
        );
        let large = ImagePayload::Pixels {
            bytes: vec![255; 1024 * 1024 * 4],
            width: 1024,
            height: 1024,
            format: ImagePixelFormat::Rgba8,
        };
        assert!(
            visible_image_strips(
                "owned",
                &large,
                Rect::new(0, 0, 5, 1),
                Rect::new(0, 0, 5, 1),
                |x, _| x % 2 == 0
            )
            .is_ok()
        );
    }
}
