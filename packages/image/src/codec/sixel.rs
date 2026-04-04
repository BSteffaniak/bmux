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
                if i < data.len() && data[i] >= 0x3F && data[i] <= 0x7E {
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
                if i < data.len() && data[i] >= 0x3F && data[i] <= 0x7E {
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

/// Encode an RGBA pixel buffer as sixel DCS data (without the ESC P q / ESC \
/// wrapper).
///
/// Uses a basic 256-color quantization with direct RGB mapping.
pub fn encode(pixels: &PixelBuffer) -> Option<Vec<u8>> {
    if pixels.width == 0 || pixels.height == 0 {
        return None;
    }

    let width = pixels.width as usize;
    let height = pixels.height as usize;
    let pixel_data = match pixels.format {
        PixelFormat::Rgba8 => &pixels.data,
        PixelFormat::Rgb8 => &pixels.data,
        PixelFormat::Png => return None, // Can't encode PNG directly to sixel.
    };
    let bpp = match pixels.format {
        PixelFormat::Rgba8 => 4,
        PixelFormat::Rgb8 => 3,
        PixelFormat::Png => return None,
    };

    let mut out = Vec::with_capacity(width * height);

    // Raster attributes: Pan=1; Pad=1; Ph=width; Pv=height
    out.extend_from_slice(format!("\"1;1;{};{}", width, height).as_bytes());

    // Build a simple 216-color palette (6x6x6 RGB cube).
    for r in 0..6u8 {
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
    let num_bands = (height + 5) / 6;
    for band in 0..num_bands {
        let band_y = band * 6;

        // For each color that has pixels in this band, emit a sixel row.
        // Simple approach: iterate all colors, emit non-empty ones.
        let mut band_has_output = false;

        for color_idx in 0u16..216 {
            let cr = ((color_idx / 36) as u8) * 51;
            let cg = (((color_idx / 6) % 6) as u8) * 51;
            let cb = ((color_idx % 6) as u8) * 51;

            // Build the sixel column data for this color in this band.
            let mut has_pixels = false;
            let mut columns = Vec::with_capacity(width);

            for x in 0..width {
                let mut bits: u8 = 0;
                for bit in 0..6usize {
                    let y = band_y + bit;
                    if y >= height {
                        continue;
                    }
                    let offset = (y * width + x) * bpp;
                    if offset + 2 < pixel_data.len() {
                        // Skip transparent pixels (alpha < 128) for RGBA format.
                        if bpp == 4 && offset + 3 < pixel_data.len() && pixel_data[offset + 3] < 128
                        {
                            continue;
                        }
                        let pr = pixel_data[offset];
                        let pg = pixel_data[offset + 1];
                        let pb = pixel_data[offset + 2];
                        // Quantize to 6x6x6 cube.
                        let qr = ((pr as u16 + 25) / 51).min(5) as u8 * 51;
                        let qg = ((pg as u16 + 25) / 51).min(5) as u8 * 51;
                        let qb = ((pb as u16 + 25) / 51).min(5) as u8 * 51;
                        if qr == cr && qg == cg && qb == cb {
                            bits |= 1 << bit;
                        }
                    }
                }
                columns.push(bits);
            }

            // Check if any column has pixels for this color.
            if columns.iter().any(|&b| b != 0) {
                has_pixels = true;
            }

            if !has_pixels {
                continue;
            }

            // Select color.
            if band_has_output {
                out.push(b'$'); // Carriage return (same band, different color).
            }
            out.extend_from_slice(format!("#{color_idx}").as_bytes());

            // Emit sixel characters with run-length encoding.
            let mut i = 0;
            while i < columns.len() {
                let ch = columns[i];
                let sixel_char = ch + 0x3F;
                // Count consecutive identical characters.
                let mut run = 1;
                while i + run < columns.len() && columns[i + run] == ch {
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

    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

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
