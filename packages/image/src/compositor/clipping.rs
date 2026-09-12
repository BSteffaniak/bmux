//! Bounded cell-geometry clipping for decoded pane image fragments.
use crate::model::{ImagePayload, PixelBuffer, PixelFormat};
use bmux_tui::geometry::Rect;

pub(super) fn subtract(rect: Rect, cover: Rect) -> Vec<Rect> {
    let hit = rect.intersection(cover);
    if hit.is_empty() {
        return vec![rect];
    }
    [
        Rect::new(rect.x, rect.y, rect.width, hit.y - rect.y),
        Rect::new(
            rect.x,
            hit.bottom(),
            rect.width,
            rect.bottom() - hit.bottom(),
        ),
        Rect::new(rect.x, hit.y, hit.x - rect.x, hit.height),
        Rect::new(hit.right(), hit.y, rect.right() - hit.right(), hit.height),
    ]
    .into_iter()
    .filter(|rect| !rect.is_empty())
    .collect()
}

pub(super) fn decoded(image: &crate::model::PaneImage) -> std::io::Result<PixelBuffer> {
    if image.payload.pixels.is_some() {
        return pixels(&image.payload);
    }
    let raw = image
        .payload
        .raw
        .as_deref()
        .ok_or_else(|| std::io::Error::other("image has no payload"))?;
    if raw.len() > 64 * 1024 * 1024 {
        return Err(std::io::Error::other("image payload exceeds decode budget"));
    }
    #[cfg(feature = "sixel")]
    if image.protocol == crate::model::ImageProtocol::Sixel {
        let size = crate::codec::sixel::estimate_pixel_size(raw);
        if u64::from(size.width) * u64::from(size.height) > 16 * 1024 * 1024 {
            return Err(std::io::Error::other(
                "sixel dimensions exceed decode budget",
            ));
        }
        return crate::codec::sixel::decode(raw)
            .ok_or_else(|| std::io::Error::other("invalid sixel image"));
    }
    #[cfg(feature = "iterm2")]
    if image.protocol == crate::model::ImageProtocol::ITerm2 {
        let (_, png) = crate::codec::iterm2::parse_body(raw)
            .ok_or_else(|| std::io::Error::other("invalid iTerm2 payload"))?;
        return decode_png(&png);
    }
    decode_png(raw)
}

fn decode_png(data: &[u8]) -> std::io::Result<PixelBuffer> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .map_err(std::io::Error::other)?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(64 * 1024 * 1024);
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    reader.limits(limits);
    let decoded = reader.decode().map_err(std::io::Error::other)?.to_rgba8();
    Ok(PixelBuffer {
        width: decoded.width(),
        height: decoded.height(),
        format: PixelFormat::Rgba8,
        data: decoded.into_raw(),
    })
}

pub(super) fn pixels(payload: &ImagePayload) -> std::io::Result<PixelBuffer> {
    let pixels = payload.pixels.as_ref().ok_or_else(|| {
        std::io::Error::other(
            "clipped pane images require decoded pixels; raw passthrough cannot preserve occlusion",
        )
    })?;
    if pixels.format == PixelFormat::Png {
        return decode_png(&pixels.data);
    }
    let channels = if pixels.format == PixelFormat::Rgb8 {
        3
    } else {
        4
    };
    let expected = usize::try_from(pixels.width)
        .ok()
        .and_then(|width| {
            usize::try_from(pixels.height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|count| count.checked_mul(channels));
    if expected != Some(pixels.data.len()) || pixels.data.len() > 64 * 1024 * 1024 {
        return Err(std::io::Error::other("invalid or oversized decoded image"));
    }
    Ok(pixels.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fragments_exclude_overlay_and_preserve_uncovered_area() {
        let fragments = subtract(Rect::new(0, 0, 10, 10), Rect::new(2, 2, 4, 4));
        assert_eq!(fragments.len(), 4);
        assert_eq!(
            fragments
                .iter()
                .map(|r| u32::from(r.width) * u32::from(r.height))
                .sum::<u32>(),
            84
        );
        assert!(
            fragments
                .iter()
                .all(|r| r.intersection(Rect::new(2, 2, 4, 4)).is_empty())
        );
    }
}
