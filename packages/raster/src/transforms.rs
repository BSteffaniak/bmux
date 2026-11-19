//! RGBA8 sampling and source-over compositing in encoded channel space.
use crate::Raster;
use std::io::{Error, Result};

fn length(width: u32, height: u32, channels: usize) -> Result<usize> {
    if width == 0 || height == 0 {
        return Err(Error::other("empty raster"));
    }
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(channels))
        .filter(|&n| n <= isize::MAX as usize)
        .ok_or_else(|| Error::other("invalid raster dimensions"))
}
fn validate(source: &Raster) -> Result<()> {
    if length(source.width, source.height, 4)? != source.rgba.len() {
        return Err(Error::other("invalid RGBA length"));
    }
    Ok(())
}
/// Convert tightly packed RGB8 or RGBA8 samples to straight-alpha RGBA8.
pub fn from_pixels(width: u32, height: u32, channels: u8, data: Vec<u8>) -> Result<Raster> {
    if !matches!(channels, 3 | 4) || data.len() != length(width, height, usize::from(channels))? {
        return Err(Error::other("invalid pixel buffer"));
    }
    let rgba = if channels == 4 {
        data
    } else {
        let mut rgba = Vec::with_capacity(length(width, height, 4)?);
        for px in data.as_chunks::<3>().0 {
            rgba.extend_from_slice(px);
            rgba.push(255);
        }
        rgba
    };
    Ok(Raster {
        width,
        height,
        rgba,
    })
}
fn offset(width: u32, x: u32, y: u32) -> usize {
    (y as usize * width as usize + x as usize) * 4
}
/// Allocate a transparent canvas.
pub fn blank(width: u32, height: u32) -> Result<Raster> {
    Ok(Raster {
        width,
        height,
        rgba: vec![0; length(width, height, 4)?],
    })
}

/// Preserve both historical nearest-coordinate evaluation orders at f32 boundaries.
pub fn nearest(
    source: &Raster,
    width: u32,
    height: u32,
    cancellable_path: bool,
    mut check: impl FnMut() -> Result<()>,
) -> Result<Raster> {
    validate(source)?;
    let mut output = blank(width, height)?;
    let sample = |i: u32, src: u32, dst: u32| {
        let center = i as f32 + 0.5;
        let pos = if cancellable_path {
            center * src as f32 / dst as f32
        } else {
            center * (src as f32 / dst as f32)
        };
        (pos as u32).min(src - 1)
    };
    for y in 0..height {
        check()?;
        let sy = sample(y, source.height, height);
        for x in 0..width {
            if x.is_multiple_of(256) {
                check()?;
            }
            let i = offset(source.width, sample(x, source.width, width), sy);
            let j = offset(width, x, y);
            output.rgba[j..j + 4].copy_from_slice(&source.rgba[i..i + 4]);
        }
    }
    Ok(output)
}
/// Blend clipped RGBA pixels, preserving the original f32 normalization and truncation.
pub fn overlay(dst: &mut Raster, src: &Raster, left: i64, top: i64) -> Result<()> {
    validate(dst)?;
    validate(src)?;
    for y in 0..src.height {
        let dy = top.saturating_add(i64::from(y));
        if !(0..i64::from(dst.height)).contains(&dy) {
            continue;
        }
        for x in 0..src.width {
            let dx = left.saturating_add(i64::from(x));
            if !(0..i64::from(dst.width)).contains(&dx) {
                continue;
            }
            let i = offset(src.width, x, y);
            let j = offset(dst.width, dx as u32, dy as u32);
            let s = &src.rgba[i..i + 4];
            let d = &mut dst.rgba[j..j + 4];
            match s[3] {
                0 => (),
                255 => d.copy_from_slice(s),
                _ => {
                    let fa = f32::from(s[3]) / 255.;
                    let ba = f32::from(d[3]) / 255.;
                    let alpha = ba + fa - ba * fa;
                    for c in 0..3 {
                        let fg = f32::from(s[c]) / 255. * fa;
                        let bg = f32::from(d[c]) / 255. * ba;
                        d[c] = (255. * ((fg + bg * (1. - fa)) / alpha)) as u8;
                    }
                    d[3] = (255. * alpha) as u8;
                }
            }
        }
    }
    Ok(())
}
/// Equivalent image-0.25.10 Lanczos3 passes for RGBA8. Checkpoints occur within
/// tap accumulation (at most 64 taps), retaining f32 operation order/rounding.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)] // Validated raster dimensions and clamped channel output.
pub fn lanczos3(
    source: &Raster,
    width: u32,
    height: u32,
    mut cancelled: impl FnMut() -> bool,
) -> std::io::Result<Raster> {
    validate(source)?;
    if (source.width, source.height) == (width, height) {
        if cancelled() {
            return Err(Error::new(
                std::io::ErrorKind::Interrupted,
                "resampling cancelled",
            ));
        }
        return Ok(source.clone());
    }
    resample(
        (source.width, source.height),
        (width, height),
        |x, y| {
            let i = offset(source.width, x, y);
            std::array::from_fn(|c| f32::from(source.rgba[i + c]))
        },
        |v| v.clamp(0., 255.).round() as u8,
        cancelled,
    )
}

/// Resize the first frame of a 16-bit PNG at native precision before narrowing to RGBA8.
/// The RGBA8 decoder alone cannot represent these intermediate samples.
pub fn lanczos3_png16(
    bytes: &[u8],
    size: (u32, u32),
    target: (u32, u32),
    max_alloc: usize,
) -> Result<Raster> {
    let mut decoder = png::Decoder::new_with_limits(
        std::io::Cursor::new(bytes),
        png::Limits { bytes: max_alloc },
    );
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().map_err(Error::other)?;
    let (color, depth) = reader.output_color_type();
    if reader.info().size() != size
        || depth != png::BitDepth::Sixteen
        || color == png::ColorType::Indexed
    {
        return Err(Error::other("invalid 16-bit PNG"));
    }
    let channels = color.samples();
    let n = length(size.0, size.1, 2 * channels)?;
    if reader.output_buffer_size() != Some(n) || n > max_alloc {
        return Err(Error::other("PNG exceeds allocation limit"));
    }
    let mut samples = vec![0; n];
    reader.next_frame(&mut samples).map_err(Error::other)?;
    let sample = |x, y| {
        let i = (offset(size.0, x, y) / 4) * channels * 2;
        let channel = |c: usize| {
            f32::from(u16::from_be_bytes([
                samples[i + 2 * c],
                samples[i + 2 * c + 1],
            ]))
        };
        let first = channel(0);
        match color {
            png::ColorType::Rgba => [first, channel(1), channel(2), channel(3)],
            png::ColorType::Rgb => [first, channel(1), channel(2), 65535.],
            png::ColorType::GrayscaleAlpha => [first, first, first, channel(1)],
            png::ColorType::Grayscale => [first, first, first, 65535.],
            png::ColorType::Indexed => unreachable!(),
        }
    };
    let narrow = |v: f32| ((v.clamp(0., 65535.).round() as u32 + 128) / 257) as u8;
    resample(size, target, sample, narrow, || false)
}

fn resample(
    (sw, sh): (u32, u32),
    (width, height): (u32, u32),
    sample: impl Fn(u32, u32) -> [f32; 4],
    narrow: impl Fn(f32) -> u8 + Copy,
    mut cancelled: impl FnMut() -> bool,
) -> Result<Raster> {
    length(width, height, 4)?;
    let mut check = || {
        if cancelled() {
            Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "resampling cancelled",
            ))
        } else {
            Ok(())
        }
    };
    check()?;
    fn kernel(x: f32) -> f32 {
        fn sinc(x: f32) -> f32 {
            let a = x * std::f32::consts::PI;
            if x == 0.0 { 1.0 } else { a.sin() / a }
        }
        if x.abs() < 3.0 {
            sinc(x) * sinc(x / 3.0)
        } else {
            0.0
        }
    }
    fn weights(
        size: u32,
        target: u32,
        pos: u32,
        check: &mut impl FnMut() -> std::io::Result<()>,
    ) -> std::io::Result<(u32, Vec<f32>)> {
        let ratio = size as f32 / target as f32;
        let sratio = ratio.max(1.0);
        let support = 3.0 * sratio;
        let center = (pos as f32 + 0.5) * ratio;
        let left = ((center - support).floor() as i64).clamp(0, i64::from(size) - 1) as u32;
        let right =
            ((center + support).ceil() as i64).clamp(i64::from(left) + 1, i64::from(size)) as u32;
        let center = center - 0.5;
        let mut values = Vec::new();
        let mut sum = 0.0;
        for i in left..right {
            if (i - left).is_multiple_of(64) {
                check()?;
            }
            let value = kernel((i as f32 - center) / sratio);
            values.push(value);
            sum += value;
        }
        for (i, value) in values.iter_mut().enumerate() {
            if i % 64 == 0 {
                check()?;
            }
            *value /= sum;
        }
        Ok((left, values))
    }
    // Stream the vertical pass a row at a time rather than allocating sw * height float pixels.
    // Cache common terminal sizes, not one heap allocation per pixel of a very wide row.
    let horizontal = if width <= 4096 && sw <= 4096 {
        Some(
            (0..width)
                .map(|x| weights(sw, width, x, &mut check))
                .collect::<Result<Vec<_>>>()?,
        )
    } else {
        None
    };
    let mut vertical = vec![[0.0; 4]; sw as usize];
    let mut output = blank(width, height)?;
    for y in 0..height {
        check()?;
        let (left, ws) = weights(sh, height, y, &mut check)?;
        for x in 0..sw {
            let mut sums = [0.; 4];
            for (i, w) in ws.iter().enumerate() {
                if i % 64 == 0 {
                    check()?;
                }
                let pixel = sample(x, left + i as u32);
                for c in 0..4 {
                    sums[c] += pixel[c] * w;
                }
            }
            vertical[x as usize] = sums;
        }
        for x in 0..width {
            let temporary;
            let (left, ws) = if let Some(cache) = &horizontal {
                &cache[x as usize]
            } else {
                temporary = weights(sw, width, x, &mut check)?;
                &temporary
            };
            let mut sums = [0.; 4];
            for (i, w) in ws.iter().enumerate() {
                if i % 64 == 0 {
                    check()?;
                }
                let px = vertical[*left as usize + i];
                for c in 0..4 {
                    sums[c] += px[c] * w;
                }
            }
            let at = offset(width, x, y);
            output.rgba[at..at + 4].copy_from_slice(&sums.map(narrow));
        }
    }
    Ok(output)
}
#[cfg(test)]
mod resize_cancellation_tests {
    use super::*;
    #[test]
    fn nearest_preserves_historical_half_pixel_boundary() {
        let src = from_pixels(2, 1, 4, vec![0, 0, 0, 0, 255, 255, 255, 255]).unwrap();
        for (cancellable, expected) in [(false, 0), (true, 255)] {
            let dst = nearest(&src, 41, 1, cancellable, || Ok(())).unwrap();
            assert_eq!(&dst.rgba[80..84], &[expected; 4]);
        }
    }
    #[test]
    fn clipped_overlay_matches_source_over_golden() {
        let mut dst = from_pixels(3, 1, 4, [17, 81, 119, 91].repeat(3)).unwrap();
        let src = from_pixels(
            4,
            1,
            4,
            vec![0, 0, 0, 0, 123, 17, 255, 29, 89, 23, 4, 255, 0, 0, 0, 0],
        )
        .unwrap();
        overlay(&mut dst, &src, -1, 0).unwrap();
        assert_eq!(
            dst.rgba,
            [45, 64, 154, 109, 89, 23, 4, 255, 17, 81, 119, 91]
        );
    }
    #[test]
    fn sixteen_bit_lanczos_rounds_after_filtering() {
        let mut data = vec![];
        for y in 0..3 {
            for x in 0..7 {
                for sample in [
                    (x * 11701 + y * 15337) % 65536,
                    (x * 18911 + y * 3001) % 65536,
                    (x * 34567 + y * 21) % 65536,
                    (x * 11389 + y * 731) % 65536,
                ] {
                    data.extend_from_slice(&(sample as u16).to_be_bytes());
                }
            }
        }
        let mut bytes = vec![];
        let mut encoder = png::Encoder::new(&mut bytes, 7, 3);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Sixteen);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&data)
            .unwrap();
        assert_eq!(
            lanczos3_png16(&bytes, (7, 3), (4, 2), 65536).unwrap().rgba,
            [
                32, 27, 54, 18, 118, 175, 81, 89, 208, 106, 94, 192, 89, 145, 90, 109, 130, 44, 54,
                22, 157, 192, 81, 93, 84, 123, 94, 196, 96, 162, 90, 113
            ]
        );
    }
    #[test]
    fn lanczos_pixels_match_locked_image_and_cancel_inside_both_passes() {
        for (sw, sh, w, h, lanczos, point) in [
            (7, 13, 19, 5, 0xf2d2411a83b23b5e, 0xc9b32cffe16544e5),
            (31, 17, 8, 29, 0x7769caec22c2345d, 0xb677e60d2b4e86a5),
            (1, 9, 7, 1, 0x9c6c6fd171d29f7e, 0xeafc4e80ad6f249d),
            (20, 20, 20, 20, 0xfdd45a5274fe73cd, 0xfdd45a5274fe73cd),
        ] {
            let mut rgba = vec![];
            for y in 0..sh {
                for x in 0..sw {
                    rgba.extend_from_slice(&[
                        ((x * 71 + y * 29) % 256) as u8,
                        ((x * 17 + y * 101) % 256) as u8,
                        ((x * 89 + y * 11) % 256) as u8,
                        ((x * 31 + y * 37) % 256) as u8,
                    ]);
                }
            }
            let source = Raster {
                width: sw,
                height: sh,
                rgba,
            };
            let hash = |r: Raster| {
                r.rgba.iter().fold(0xcbf29ce484222325u64, |a, x| {
                    (a ^ u64::from(*x)).wrapping_mul(1099511628211)
                })
            };
            assert_eq!(
                hash(nearest(&source, w, h, false, || Ok(())).unwrap()),
                point
            );
            let mut total = 0;
            let actual = lanczos3(&source, w, h, || {
                total += 1;
                false
            })
            .unwrap();
            assert_eq!(hash(actual), lanczos, "{sw}x{sh} -> {w}x{h}");
            for stop in [1, total / 3 + 1, total * 2 / 3 + 1, total] {
                let mut steps = 0;
                let error = lanczos3(&source, w, h, || {
                    steps += 1;
                    steps == stop
                })
                .err()
                .unwrap();
                assert_eq!(steps, stop);
                assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
            }
        }
    }
}
