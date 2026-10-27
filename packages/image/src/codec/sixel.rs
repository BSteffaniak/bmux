//! Sixel codec — parse and emit sixel image data.
//!
//! Sixel is a DCS-based protocol: `ESC P q <parameters> <sixel-data> ESC \`
//!
//! Each row of sixel data encodes a 6-pixel-tall band.  Colors are defined
//! with `#` directives.  The data consists of characters in the range
//! `0x3F`..`0x7E`, each representing a 6-bit column of pixels.

use crate::model::{ImagePixelSize, PixelBuffer, PixelFormat};

/// Estimate the pixel dimensions of a sixel image from its raw body data
/// (the bytes between `ESC P q` and `ESC \`).
///
/// This does a fast scan without fully decoding the image.
pub fn estimate_pixel_size(data: &[u8]) -> ImagePixelSize {
    let mut max_x: u32 = 0;
    let mut current_x: u32 = 0;
    let mut band_count: u32 = 1;

    let mut i = 0;
    while i < data.len() {
        let b = data[i];
        match b {
            // Sixel data characters: 0x3F ('?') through 0x7E ('~')
            // Each represents one column of 6 vertical pixels.
            0x3F..=0x7E => {
                current_x += 1;
                if current_x > max_x {
                    max_x = current_x;
                }
            }
            // '$' = Graphics Carriage Return (return to column 0, same band)
            b'$' => {
                current_x = 0;
            }
            // '-' = Graphics New Line (next 6-pixel band)
            b'-' => {
                current_x = 0;
                band_count += 1;
            }
            // '!' = Repeat introducer: !<count><sixel-char>
            b'!' => {
                // Parse the repeat count.
                let mut count: u32 = 0;
                i += 1;
                while i < data.len() && data[i].is_ascii_digit() {
                    count = count
                        .saturating_mul(10)
                        .saturating_add((data[i] - b'0') as u32);
                    i += 1;
                }
                // The next byte is the sixel character to repeat.
                if i < data.len() && (0x3F..=0x7E).contains(&data[i]) {
                    current_x += count;
                    if current_x > max_x {
                        max_x = current_x;
                    }
                }
                // Don't increment i again — the outer loop will.
            }
            // '#' = Color introducer — skip the color definition parameters.
            b'#' => {
                i += 1;
                while i < data.len() && (data[i].is_ascii_digit() || data[i] == b';') {
                    i += 1;
                }
                continue; // Don't increment i again.
            }
            // '"' = Raster attributes: "Pan;Pad;Ph;Pv
            // Ph = pixel width, Pv = pixel height (optional hints).
            b'"' => {
                let start = i + 1;
                i += 1;
                while i < data.len() && (data[i].is_ascii_digit() || data[i] == b';') {
                    i += 1;
                }
                // Parse the raster attributes if present.
                let params: Vec<u32> = data[start..i]
                    .split(|&b| b == b';')
                    .filter_map(|s| std::str::from_utf8(s).ok()?.parse().ok())
                    .collect();
                if params.len() >= 4 {
                    // Ph and Pv are params[2] and params[3].
                    return ImagePixelSize {
                        width: params[2],
                        height: params[3],
                    };
                }
                continue;
            }
            _ => {
                // Skip unknown bytes.
            }
        }
        i += 1;
    }

    ImagePixelSize {
        width: max_x,
        height: band_count.saturating_mul(6),
    }
}

/// Decode sixel image data to an RGBA pixel buffer.
///
/// Returns `None` if the data is not valid sixel.
pub fn decode(data: &[u8]) -> Option<PixelBuffer> {
    let size = estimate_pixel_size(data);
    if size.width == 0 || size.height == 0 {
        return None;
    }

    let width = size.width as usize;
    let height = size.height as usize;
    let mut pixels = vec![0u8; width * height * 4]; // RGBA

    // Color palette (up to 256 entries).
    let mut palette = [[0u8; 4]; 256];
    // Default palette entry 0 = transparent.
    let mut current_color: usize = 0;
    let mut x: usize = 0;
    let mut band: usize = 0;

    let mut i = 0;
    while i < data.len() {
        let b = data[i];
        match b {
            // Sixel data characters
            0x3F..=0x7E => {
                let bits = b - 0x3F;
                paint_sixel_column(
                    &mut pixels,
                    width,
                    height,
                    x,
                    band,
                    bits,
                    &palette[current_color],
                );
                x += 1;
            }
            b'$' => x = 0,
            b'-' => {
                x = 0;
                band += 1;
            }
            b'!' => {
                // Repeat
                let mut count: usize = 0;
                i += 1;
                while i < data.len() && data[i].is_ascii_digit() {
                    count = count
                        .saturating_mul(10)
                        .saturating_add((data[i] - b'0') as usize);
                    i += 1;
                }
                if i < data.len() && (0x3F..=0x7E).contains(&data[i]) {
                    let bits = data[i] - 0x3F;
                    for _ in 0..count {
                        paint_sixel_column(
                            &mut pixels,
                            width,
                            height,
                            x,
                            band,
                            bits,
                            &palette[current_color],
                        );
                        x += 1;
                    }
                }
            }
            b'#' => {
                // Color definition or selection
                i += 1;
                let start = i;
                while i < data.len() && (data[i].is_ascii_digit() || data[i] == b';') {
                    i += 1;
                }
                let params: Vec<u32> = data[start..i]
                    .split(|&b| b == b';')
                    .filter_map(|s| std::str::from_utf8(s).ok()?.parse().ok())
                    .collect();

                if params.len() == 1 {
                    // Color selection: #<index>
                    current_color = params[0] as usize % 256;
                } else if params.len() >= 5 && params[1] == 2 {
                    // HLS definition: #index;2;H;L;S  (we treat as RGB for simplicity)
                    // TODO: proper HLS-to-RGB conversion.
                    let idx = params[0] as usize % 256;
                    let r = (params[2] * 255 / 100) as u8;
                    let g = (params[3] * 255 / 100) as u8;
                    let b_val = (params[4] * 255 / 100) as u8;
                    palette[idx] = [r, g, b_val, 255];
                    current_color = idx;
                }
                continue;
            }
            b'"' => {
                // Raster attributes — skip.
                i += 1;
                while i < data.len() && (data[i].is_ascii_digit() || data[i] == b';') {
                    i += 1;
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }

    Some(PixelBuffer {
        width: size.width,
        height: size.height,
        format: PixelFormat::Rgba8,
        data: pixels,
    })
}

/// Paint a single sixel column (1 pixel wide, 6 pixels tall) into the pixel
/// buffer at position (x, band*6).
fn paint_sixel_column(
    pixels: &mut [u8],
    width: usize,
    height: usize,
    x: usize,
    band: usize,
    bits: u8,
    color: &[u8; 4],
) {
    if x >= width {
        return;
    }
    for bit in 0..6 {
        if bits & (1 << bit) != 0 {
            let y = band * 6 + bit;
            if y < height {
                let offset = (y * width + x) * 4;
                if offset + 4 <= pixels.len() {
                    pixels[offset..offset + 4].copy_from_slice(color);
                }
            }
        }
    }
}

/// Encode an RGB or RGBA pixel buffer as sixel DCS data (without the ESC P q / ESC \
/// wrapper).
///
/// Uses a fixed 216-color palette (6x6x6 RGB cube).
pub fn encode(pixels: &PixelBuffer) -> Option<Vec<u8>> {
    encode_cancellable(pixels, || false).expect("uncancelled Sixel")
}

/// Encode with checkpoints inside band fill, palette and RLE traversal.
///
/// # Errors
/// * Interrupted when cancellation is requested; no partial body is returned.
pub fn encode_cancellable(
    pixels: &PixelBuffer,
    mut cancelled: impl FnMut() -> bool,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut check = || {
        if cancelled() {
            Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "Sixel cancelled",
            ))
        } else {
            Ok(())
        }
    };
    check()?;
    if pixels.width == 0 || pixels.height == 0 || pixels.format == PixelFormat::Png {
        return Ok(None);
    }

    let width = pixels.width as usize;
    let height = pixels.height as usize;
    let bpp = match pixels.format {
        PixelFormat::Rgba8 => 4,
        PixelFormat::Rgb8 => 3,
        PixelFormat::Png => return Ok(None),
    };
    let Some(pixel_count) = width.checked_mul(height) else {
        return Ok(None);
    };
    if pixel_count.checked_mul(bpp).is_none() {
        return Ok(None);
    }

    let mut out = Vec::with_capacity(pixel_count);

    // Raster attributes: Pan=1; Pad=1; Ph=width; Pv=height
    out.extend_from_slice(format!("\"1;1;{};{}", width, height).as_bytes());

    // Build a simple 216-color palette (6x6x6 RGB cube).
    for r in 0..6u8 {
        check()?;
        for g in 0..6u8 {
            for b in 0..6u8 {
                let idx = (r as u16) * 36 + (g as u16) * 6 + (b as u16);
                let rp = (r as u32) * 100 / 5;
                let gp = (g as u32) * 100 / 5;
                let bp = (b as u32) * 100 / 5;
                out.extend_from_slice(format!("#{idx};2;{rp};{gp};{bp}").as_bytes());
            }
        }
    }

    // Encode pixel data in sixel bands (6 rows per band).
    let num_bands = height.div_ceil(6);
    let mut band_data = SixelBand::new();
    for band in 0..num_bands {
        band_data.fill(pixels, band, quantize_color, &mut check)?;
        let mut band_has_output = false;

        // Palette order is part of the established byte representation.
        for color_idx in 0..216 {
            check()?;
            if !band_data.contains(color_idx) {
                continue;
            }
            let columns = &band_data.columns[color_idx];

            // Select color.
            if band_has_output {
                out.push(b'$'); // Carriage return (same band, different color).
            }
            out.extend_from_slice(format!("#{color_idx}").as_bytes());

            // Emit sixel characters with run-length encoding.
            let mut i = 0;
            while i < columns.len() {
                check()?;
                let ch = columns[i];
                let sixel_char = ch + 0x3F;
                // Count consecutive identical characters.
                let mut run = 1;
                while i + run < columns.len() && columns[i + run] == ch {
                    if run % 256 == 0 {
                        check()?;
                    }
                    run += 1;
                }
                if run > 3 {
                    out.extend_from_slice(format!("!{run}").as_bytes());
                    out.push(sixel_char);
                } else {
                    for _ in 0..run {
                        out.push(sixel_char);
                    }
                }
                i += run;
            }

            band_has_output = true;
        }

        // New line (next band), except for the last band.
        if band + 1 < num_bands {
            out.push(b'-');
        }
    }

    Ok(Some(out))
}

/// Quantize once, retaining the established round-to-nearest cube mapping.
fn quantize_color(rgb: [u8; 3]) -> usize {
    let level = |value: u8| usize::from((u16::from(value) + 25) / 51).min(5);
    level(rgb[0]) * 36 + level(rgb[1]) * 6 + level(rgb[2])
}

/// Reusable band-local scratch: at most 216 full-width column buffers.
/// Buffers are allocated only for encountered colors; no image-sized cache is kept.
struct SixelBand {
    columns: [Vec<u8>; 216],
    used: [u64; 4],
}

impl SixelBand {
    fn new() -> Self {
        Self {
            columns: std::array::from_fn(|_| Vec::new()),
            used: [0; 4],
        }
    }

    fn contains(&self, color: usize) -> bool {
        self.used[color / 64] & (1u64 << (color % 64)) != 0
    }

    fn fill(
        &mut self,
        pixels: &PixelBuffer,
        band: usize,
        mut quantize: impl FnMut([u8; 3]) -> usize,
        check: &mut impl FnMut() -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        for color in 0..216 {
            if self.contains(color) {
                for chunk in self.columns[color].chunks_mut(256) {
                    check()?;
                    chunk.fill(0);
                }
            }
        }
        self.used.fill(0);
        let width = pixels.width as usize;
        let height = pixels.height as usize;
        let bpp = if pixels.format == PixelFormat::Rgba8 {
            4
        } else {
            3
        };
        for bit in 0..6 {
            let y = band * 6 + bit;
            if y >= height {
                break;
            }
            for x in 0..width {
                if x % 256 == 0 {
                    check()?;
                }
                let offset = (y * width + x) * bpp;
                let Some(rgb) = pixels.data.get(offset..offset + 3) else {
                    continue;
                };
                // Preserve truncated RGBA semantics: a missing alpha is opaque.
                if bpp == 4
                    && pixels
                        .data
                        .get(offset + 3)
                        .is_some_and(|&alpha| alpha < 128)
                {
                    continue;
                }
                let color = quantize([rgb[0], rgb[1], rgb[2]]);
                if !self.contains(color) {
                    self.columns[color].resize(width, 0);
                    self.used[color / 64] |= 1u64 << (color % 64);
                }
                self.columns[color][x] |= 1 << bit;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellable_encoding_preserves_wire_and_stops_inside_band_steps() {
        let pixels = PixelBuffer {
            width: 769,
            height: 13,
            format: PixelFormat::Rgba8,
            data: (0..769 * 13 * 4).map(|i| (i % 251) as u8).collect(),
        };
        let expected = encode(&pixels).unwrap();
        let mut total = 0;
        let actual = encode_cancellable(&pixels, || {
            total += 1;
            false
        })
        .unwrap()
        .unwrap();
        assert_eq!(actual, expected);
        assert!(total > 216 * 3);
        for stop in [1, 8, 20, total / 3, total / 2, total - 1] {
            let mut steps = 0;
            let error = encode_cancellable(&pixels, || {
                steps += 1;
                steps == stop
            })
            .unwrap_err();
            assert_eq!(steps, stop);
            assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
        }
    }

    #[test]
    fn band_work_quantizes_each_drawable_pixel_once_and_reuses_scratch() {
        let width = 640;
        let height = 13;
        let mut pixels = PixelBuffer {
            width,
            height,
            format: PixelFormat::Rgba8,
            data: [255, 0, 0, 255].repeat(width as usize * height as usize),
        };
        let mut scratch = SixelBand::new();
        let mut quantizations = 0;
        for band in 0..(height as usize).div_ceil(6) {
            scratch
                .fill(
                    &pixels,
                    band,
                    |rgb| {
                        quantizations += 1;
                        quantize_color(rgb)
                    },
                    &mut || Ok(()),
                )
                .unwrap();
            assert_eq!(
                scratch
                    .used
                    .iter()
                    .map(|word| word.count_ones())
                    .sum::<u32>(),
                1
            );
            assert!(scratch.contains(180));
            assert_eq!(
                scratch.columns.iter().map(Vec::len).sum::<usize>(),
                width as usize
            );
            let bits = if band == 2 { 1 } else { 63 };
            assert!(scratch.columns[180].iter().all(|&column| column == bits));
        }
        assert_eq!(quantizations, width as usize * height as usize);
        let allocation = scratch.columns[180].as_ptr();

        // An empty intervening band must clear old bits without quantization.
        for pixel in pixels.data.as_chunks_mut::<4>().0 {
            pixel[3] = 127;
        }
        scratch
            .fill(
                &pixels,
                0,
                |_| panic!("transparent pixels must not be quantized"),
                &mut || Ok(()),
            )
            .unwrap();
        assert_eq!(scratch.used, [0; 4]);
        assert!(scratch.columns[180].iter().all(|&column| column == 0));
        pixels.data[..4].copy_from_slice(&[255, 0, 0, 128]);
        let mut quantizations = 0;
        scratch
            .fill(
                &pixels,
                0,
                |rgb| {
                    quantizations += 1;
                    quantize_color(rgb)
                },
                &mut || Ok(()),
            )
            .unwrap();
        assert_eq!(quantizations, 1);
        assert_eq!(scratch.columns[180].as_ptr(), allocation);
        assert_eq!(scratch.columns[180][0], 1);
        assert!(scratch.columns[180][1..].iter().all(|&column| column == 0));
    }

    #[test]
    fn band_masks_cover_all_colors_with_width_bounded_scratch() {
        let mut data = Vec::new();
        for color in 0..216u16 {
            data.extend_from_slice(&[
                (color / 36 * 51) as u8,
                (color / 6 % 6 * 51) as u8,
                (color % 6 * 51) as u8,
            ]);
        }
        let pixels = PixelBuffer {
            width: 216,
            height: 1,
            format: PixelFormat::Rgb8,
            data,
        };
        let mut scratch = SixelBand::new();
        let mut quantizations = 0;
        scratch
            .fill(
                &pixels,
                0,
                |rgb| {
                    quantizations += 1;
                    quantize_color(rgb)
                },
                &mut || Ok(()),
            )
            .unwrap();
        assert_eq!(quantizations, 216);
        assert_eq!(scratch.used, [u64::MAX, u64::MAX, u64::MAX, (1 << 24) - 1]);
        assert_eq!(
            scratch.columns.iter().map(Vec::len).sum::<usize>(),
            216 * 216
        );
        for color in 0..216 {
            assert!(scratch.contains(color));
            assert_eq!(scratch.columns[color][color], 1);
        }
    }

    #[test]
    fn estimate_simple_sixel() {
        // Two columns, one band: "??" (each '?' = 0x3F = all zeros)
        let data = b"??";
        let size = estimate_pixel_size(data);
        assert_eq!(size.width, 2);
        assert_eq!(size.height, 6); // 1 band = 6 pixels
    }

    #[test]
    fn estimate_with_newline() {
        // Two bands: "??-??"
        let data = b"??-??";
        let size = estimate_pixel_size(data);
        assert_eq!(size.width, 2);
        assert_eq!(size.height, 12); // 2 bands = 12 pixels
    }

    #[test]
    fn estimate_with_repeat() {
        // "!10~" = 10 repetitions of '~' = 10 columns
        let data = b"!10~";
        let size = estimate_pixel_size(data);
        assert_eq!(size.width, 10);
        assert_eq!(size.height, 6);
    }

    #[test]
    fn estimate_with_raster_attributes() {
        // Raster attributes: "1;1;80;48" -> 80x48 pixels
        let data = b"\"1;1;80;48~";
        let size = estimate_pixel_size(data);
        assert_eq!(size.width, 80);
        assert_eq!(size.height, 48);
    }

    #[test]
    fn decode_minimal_sixel() {
        // A single pixel column: all 6 pixels set in color 0
        // '#0;2;100;0;0' sets color 0 to red, then '~' = 0x3F + 0x3F = all bits set
        let data = b"#0;2;100;0;0~";
        let buf = decode(data).unwrap();
        assert_eq!(buf.width, 1);
        assert_eq!(buf.height, 6);
        // First pixel should be red (255, 0, 0, 255).
        assert_eq!(&buf.data[0..4], &[255, 0, 0, 255]);
    }
}
