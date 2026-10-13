//! Frozen pre-optimization encoder from 73b533482f80; benchmark-only reference.
//! Intentionally retains 216 palette scans per band and original allocation behavior.
use bmux_image::model::{PixelBuffer, PixelFormat};

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
    let num_bands = height.div_ceil(6);
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
