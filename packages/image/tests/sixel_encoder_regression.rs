//! Byte-for-byte coverage for the fixed-palette Sixel encoder; no timing gates.
#![cfg(feature = "sixel")]

use bmux_image::codec::sixel;
use bmux_image::model::{PixelBuffer, PixelFormat};

fn buffer(width: u32, height: u32, format: PixelFormat, data: Vec<u8>) -> PixelBuffer {
    PixelBuffer {
        width,
        height,
        format,
        data,
    }
}

fn palette_header(width: u32, height: u32) -> Vec<u8> {
    let mut out = format!("\"1;1;{width};{height}").into_bytes();
    for color in 0..216 {
        let r = color / 36 * 20;
        let g = color / 6 % 6 * 20;
        let b = color % 6 * 20;
        out.extend_from_slice(format!("#{color};2;{r};{g};{b}").as_bytes());
    }
    out
}

// Deliberately retain the pre-optimization algorithm: every color scans every
// pixel in every band. Keep this independent of production scratch/masks.
fn reference_encode(pixels: &PixelBuffer) -> Option<Vec<u8>> {
    if pixels.width == 0 || pixels.height == 0 || pixels.format == PixelFormat::Png {
        return None;
    }
    let width = pixels.width as usize;
    let height = pixels.height as usize;
    let bpp = if pixels.format == PixelFormat::Rgba8 {
        4
    } else {
        3
    };
    let mut out = palette_header(pixels.width, pixels.height);
    for band in 0..height.div_ceil(6) {
        let mut emitted = false;
        for color in 0..216 {
            let target = [color / 36 * 51, color / 6 % 6 * 51, color % 6 * 51];
            let mut columns = vec![0u8; width];
            for (x, column) in columns.iter_mut().enumerate() {
                for bit in 0..6 {
                    let y = band * 6 + bit;
                    if y >= height {
                        continue;
                    }
                    let offset = (y * width + x) * bpp;
                    if offset + 2 >= pixels.data.len() {
                        continue;
                    }
                    if bpp == 4 && offset + 3 < pixels.data.len() && pixels.data[offset + 3] < 128 {
                        continue;
                    }
                    let quantized = std::array::from_fn::<_, 3, _>(|channel| {
                        ((u16::from(pixels.data[offset + channel]) + 25) / 51).min(5) as usize * 51
                    });
                    if quantized == target {
                        *column |= 1 << bit;
                    }
                }
            }
            if !columns.iter().any(|&bits| bits != 0) {
                continue;
            }
            if emitted {
                out.push(b'$');
            }
            emitted = true;
            out.extend_from_slice(format!("#{color}").as_bytes());
            let mut start = 0;
            while start < width {
                let bits = columns[start];
                let mut end = start + 1;
                while end < width && columns[end] == bits {
                    end += 1;
                }
                let run = end - start;
                if run > 3 {
                    out.extend_from_slice(format!("!{run}").as_bytes());
                    out.push(bits + b'?');
                } else {
                    out.extend(std::iter::repeat_n(bits + b'?', run));
                }
                start = end;
            }
        }
        if band + 1 < height.div_ceil(6) {
            out.push(b'-');
        }
    }
    Some(out)
}

fn assert_golden(pixels: &PixelBuffer, body: &[u8]) {
    let mut expected = palette_header(pixels.width, pixels.height);
    expected.extend_from_slice(body);
    assert_eq!(sixel::encode(pixels), Some(expected));
}

#[test]
fn golden_palette_order_partial_band_and_transparency_threshold() {
    // Input order is white, transparent red, black, just-opaque red.
    // Output order must instead be black (0), red (180), white (215).
    let pixels = buffer(
        4,
        1,
        PixelFormat::Rgba8,
        vec![
            255, 255, 255, 255, 255, 0, 0, 127, 0, 0, 0, 255, 255, 0, 0, 128,
        ],
    );
    assert_golden(&pixels, b"#0??@?$#180???@$#215@???");
}

#[test]
fn golden_rle_threshold_and_full_width_zero_columns() {
    for (width, body) in [
        (1, "#215@"),
        (3, "#215@@@"),
        (4, "#215!4@"),
        (17, "#215!17@"),
    ] {
        assert_golden(
            &buffer(width, 1, PixelFormat::Rgb8, vec![255; width as usize * 3]),
            body.as_bytes(),
        );
    }
    let mut data = vec![0; 9 * 4];
    data[16..20].copy_from_slice(&[0, 0, 0, 255]);
    assert_golden(&buffer(9, 1, PixelFormat::Rgba8, data), b"#0!4?@!4?");
}

#[test]
fn golden_empty_bands_and_reappearing_colors_have_no_stale_bits() {
    let mut data = vec![0; 2 * 19 * 4];
    data[..4].copy_from_slice(&[255, 0, 0, 255]);
    data[(18 * 2 + 1) * 4..].copy_from_slice(&[255, 0, 0, 255]);
    assert_golden(&buffer(2, 19, PixelFormat::Rgba8, data), b"#180@?---#180?@");
    assert_golden(
        &buffer(4, 13, PixelFormat::Rgba8, vec![0; 4 * 13 * 4]),
        b"--",
    );
}

#[test]
fn every_channel_value_and_alpha_boundary_matches_reference() {
    for channel in 0..3 {
        let mut data = Vec::new();
        for value in 0..=255u8 {
            for alpha in [0, 127, 128, 255] {
                let mut pixel = [51, 102, 153, alpha];
                pixel[channel] = value;
                data.extend_from_slice(&pixel);
            }
        }
        let pixels = buffer(256, 4, PixelFormat::Rgba8, data);
        assert_eq!(sixel::encode(&pixels), reference_encode(&pixels));
    }
}

#[test]
fn all_palette_colors_across_bands_match_reference() {
    let mut data = Vec::new();
    for _ in 0..7 {
        for color in (0..216u16).rev() {
            data.extend_from_slice(&[
                (color / 36 * 51) as u8,
                (color / 6 % 6 * 51) as u8,
                (color % 6 * 51) as u8,
            ]);
        }
    }
    let pixels = buffer(216, 7, PixelFormat::Rgb8, data);
    assert_eq!(sixel::encode(&pixels), reference_encode(&pixels));
}

#[test]
fn deterministic_generated_rgb_and_rgba_match_reference() {
    let mut state = 0x91e1_0da5u32;
    for format in [PixelFormat::Rgb8, PixelFormat::Rgba8] {
        let bpp = if format == PixelFormat::Rgba8 { 4 } else { 3 };
        for width in [1, 2, 3, 4, 9, 31] {
            for height in [1, 5, 6, 7, 11, 12, 13, 19] {
                let mut data = Vec::new();
                for _ in 0..width * height * bpp {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    data.push(state.to_le_bytes()[0]);
                }
                let pixels = buffer(width, height, format, data);
                let expected = reference_encode(&pixels);
                assert_eq!(
                    sixel::encode(&pixels),
                    expected,
                    "{format:?} {width}x{height}"
                );
                assert_eq!(
                    sixel::encode(&pixels),
                    expected,
                    "repeat must be deterministic"
                );
            }
        }
    }
}

#[test]
fn truncated_rgb_and_rgba_keep_established_missing_alpha_semantics() {
    for format in [PixelFormat::Rgb8, PixelFormat::Rgba8] {
        let bpp = if format == PixelFormat::Rgba8 { 4 } else { 3 };
        let data = [255, 26, 230, 128].repeat(16);
        for len in 0..=2 * 7 * bpp + 2 {
            let pixels = buffer(2, 7, format, data[..len].to_vec());
            assert_eq!(
                sixel::encode(&pixels),
                reference_encode(&pixels),
                "{format:?} len={len}"
            );
        }
    }
    assert_golden(&buffer(1, 1, PixelFormat::Rgba8, vec![255, 0, 0]), b"#180@");
}

#[test]
fn unsupported_or_empty_buffers_do_not_encode() {
    for (width, height, format) in [
        (0, 1, PixelFormat::Rgba8),
        (1, 0, PixelFormat::Rgb8),
        (1, 1, PixelFormat::Png),
    ] {
        assert_eq!(
            sixel::encode(&buffer(width, height, format, Vec::new())),
            None
        );
    }
}
