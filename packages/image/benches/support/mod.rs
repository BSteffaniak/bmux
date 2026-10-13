//! Fixed generated fixtures shared by the opt-in harness and public-API tests.
use bmux_image::model::{PixelBuffer, PixelFormat};

/// Eight opaque colors exactly representable by the Sixel percentage palette.
pub fn pixels(width: u32, height: u32) -> PixelBuffer {
    fixture(width, height, "palette").expect("known fixture")
}

/// Fixed RGBA data: no RNG, files, host detection, or external decoder required.
pub fn fixture(width: u32, height: u32, kind: &str) -> Result<PixelBuffer, &'static str> {
    if !["palette", "solid", "gradient", "transparent", "detailed"].contains(&kind) {
        return Err("fixture must be palette, solid, gradient, transparent, or detailed");
    }
    let mut data = Vec::new();
    for y in 0..height {
        for x in 0..width {
            let color = (x / 8 + y / 6) % 8;
            let pixel = match kind {
                "solid" => [51, 153, 204, 255],
                "gradient" => [
                    (x * 255 / width.saturating_sub(1).max(1)) as u8,
                    (y * 255 / height.saturating_sub(1).max(1)) as u8,
                    ((x + y) % 256) as u8,
                    255,
                ],
                "transparent" => [
                    ((x * 17 + y * 3) % 256) as u8,
                    ((x * 5 + y * 19) % 256) as u8,
                    ((x * 11 + y * 7) % 256) as u8,
                    [0, 127, 128, 255][((x + y) % 4) as usize],
                ],
                "detailed" => [
                    ((x * 73 + y * 151 + (x ^ y) * 29) % 256) as u8,
                    ((x * 193 + y * 47 + (x ^ y) * 61) % 256) as u8,
                    ((x * 31 + y * 239 + (x ^ y) * 113) % 256) as u8,
                    255,
                ],
                _ => [
                    if color & 1 == 0 { 0 } else { 255 },
                    if color & 2 == 0 { 0 } else { 255 },
                    if color & 4 == 0 { 0 } else { 255 },
                    255,
                ],
            };
            data.extend_from_slice(&pixel);
        }
    }
    Ok(PixelBuffer {
        width,
        height,
        format: PixelFormat::Rgba8,
        data,
    })
}

/// Count complete protocol introducers, not incidental header substrings.
pub fn count_marker(bytes: &[u8], marker: &[u8]) -> usize {
    bytes
        .windows(marker.len())
        .filter(|window| *window == marker)
        .count()
}
