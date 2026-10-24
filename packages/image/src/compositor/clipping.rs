//! Bounded cell-geometry clipping for decoded pane image fragments.
use crate::model::{ImagePayload, PixelBuffer, PixelFormat};
use bmux_tui::geometry::Rect;

/// Incremental RGBA PNG encoding. Each compression/filter input is at most one
/// validated row (16 KiB); cancellation is checked between rows and before
/// finalization. Native PNG passthrough never calls this function.
pub(crate) fn encode_rgba_png_cancellable(
    pixels: &PixelBuffer,
    mut cancelled: impl FnMut() -> bool,
) -> std::io::Result<Vec<u8>> {
    use std::io::Write;
    let interrupted =
        || std::io::Error::new(std::io::ErrorKind::Interrupted, "PNG encoding cancelled");
    if cancelled() {
        return Err(interrupted());
    }
    if pixels.format != PixelFormat::Rgba8
        || pixels.width == 0
        || pixels.height == 0
        || pixels.width > 4096
        || pixels.height > 4096
        || pixels.data.len() != pixels.width as usize * pixels.height as usize * 4
    {
        return Err(std::io::Error::other("invalid RGBA PNG dimensions"));
    }
    // A shared callback permits the output sink to reject publication during
    // explicit finalization and the codec's Drop finalization paths as well.
    let callback = std::cell::RefCell::new(&mut cancelled);
    struct Sink<'a, F> {
        bytes: &'a mut Vec<u8>,
        cancelled: &'a std::cell::RefCell<F>,
    }
    impl<F: FnMut() -> bool> Write for Sink<'_, F> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if (self.cancelled.borrow_mut())() {
                return Err(std::io::Error::other("PNG output cancelled"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            if (self.cancelled.borrow_mut())() {
                Err(std::io::Error::other("PNG flush cancelled"))
            } else {
                Ok(())
            }
        }
    }
    let mut bytes = Vec::new();
    {
        let sink = Sink {
            bytes: &mut bytes,
            cancelled: &callback,
        };
        let mut encoder = png::Encoder::new(sink, pixels.width, pixels.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        // Fast selects the locked fdeflate streaming backend. Feed at most one
        // row per call; output chunks are capped at 16 KiB. Finalization retains
        // compressor state (not the whole raster). Cancellation on the sink also
        // prevents trailer/IDAT publication during drop.
        encoder.set_compression(png::Compression::Fast);
        encoder.set_filter(png::Filter::Adaptive);
        let mut writer = encoder.write_header().map_err(std::io::Error::other)?;
        let mut stream = writer
            .stream_writer_with_size(16 * 1024)
            .map_err(std::io::Error::other)?;
        for row in pixels.data.chunks_exact(pixels.width as usize * 4) {
            if (callback.borrow_mut())() {
                return Err(interrupted());
            }
            stream.write_all(row)?;
        }
        if (callback.borrow_mut())() {
            return Err(interrupted());
        }
        stream.finish().map_err(std::io::Error::other)?;
        writer.finish().map_err(std::io::Error::other)?;
    }
    Ok(bytes)
}

#[cfg(test)]
mod png_cancellation_tests {
    use super::*;

    #[test]
    fn incremental_decode_matches_pixels_and_stops_during_rows() {
        let pixels = PixelBuffer {
            width: 19,
            height: 23,
            format: PixelFormat::Rgba8,
            data: (0..19 * 23 * 4).map(|i| (i % 251) as u8).collect(),
        };
        let png = encode_rgba_png_cancellable(&pixels, || false).unwrap();
        let mut count = 0;
        let actual = decode_png_cancellable(&png, 64 * 1024 * 1024, || {
            count += 1;
            false
        })
        .unwrap();
        assert_eq!(
            actual.data,
            image::load_from_memory(&png)
                .unwrap()
                .into_rgba8()
                .into_raw()
        );
        for stop in [1, 3, 12, 26, count] {
            let mut steps = 0;
            let error = decode_png_cancellable(&png, 64 * 1024 * 1024, || {
                steps += 1;
                steps >= stop
            })
            .unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
            assert!(steps >= stop && steps <= stop + 1);
        }
        assert!(decode_png_cancellable(&png, 8, || false).is_err());
    }

    #[test]
    fn incremental_decode_adam7_palette_grayscale_and_sixteen_bit_match_image() {
        let adam7 = [
            137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 3, 0, 0, 0, 3,
            8, 6, 0, 0, 1, 33, 47, 133, 41, 0, 0, 0, 51, 73, 68, 65, 84, 120, 156, 99, 96, 96, 96,
            248, 207, 208, 199, 16, 119, 151, 129, 97, 89, 220, 203, 190, 101, 123, 142, 51, 184,
            51, 232, 191, 99, 112, 95, 214, 123, 131, 129, 33, 88, 255, 139, 123, 112, 220, 227,
            190, 224, 222, 75, 0, 60, 70, 16, 180, 196, 96, 195, 191, 0, 0, 0, 0, 73, 69, 78, 68,
            174, 66, 96, 130,
        ];
        let mut cases = vec![adam7.to_vec()];
        for (color, depth, data) in [
            (
                png::ColorType::Grayscale,
                png::BitDepth::Eight,
                vec![0, 127, 255],
            ),
            (
                png::ColorType::GrayscaleAlpha,
                png::BitDepth::Eight,
                vec![0, 255, 127, 83, 255, 0],
            ),
            (
                png::ColorType::Rgb,
                png::BitDepth::Sixteen,
                vec![
                    0, 0, 0, 128, 255, 255, 128, 0, 1, 0, 127, 255, 13, 17, 211, 91, 42, 198,
                ],
            ),
            (png::ColorType::Indexed, png::BitDepth::Eight, vec![0, 1, 2]),
        ] {
            let mut bytes = Vec::new();
            {
                let mut encoder = png::Encoder::new(&mut bytes, 3, 1);
                encoder.set_color(color);
                encoder.set_depth(depth);
                if color == png::ColorType::Indexed {
                    encoder.set_palette(vec![255, 0, 0, 0, 255, 0, 0, 0, 255]);
                    encoder.set_trns(vec![0, 127, 255]);
                }
                encoder
                    .write_header()
                    .unwrap()
                    .write_image_data(&data)
                    .unwrap();
            }
            cases.push(bytes);
        }
        for bytes in cases {
            let actual = decode_png_cancellable(&bytes, 64 * 1024 * 1024, || false).unwrap();
            assert_eq!(
                actual.data,
                image::load_from_memory(&bytes)
                    .unwrap()
                    .into_rgba8()
                    .into_raw()
            );
        }
    }

    #[test]
    fn incremental_png_matches_pixels_and_cancels_at_each_row_boundary() {
        let pixels = PixelBuffer {
            width: 31,
            height: 17,
            format: PixelFormat::Rgba8,
            data: (0..31 * 17 * 4).map(|i| (i % 251) as u8).collect(),
        };
        let mut checks = 0;
        let bytes = encode_rgba_png_cancellable(&pixels, || {
            checks += 1;
            false
        })
        .unwrap();
        let decoded = image::load_from_memory(&bytes).unwrap().into_rgba8();
        assert_eq!(decoded.as_raw(), &pixels.data);
        assert!(checks >= pixels.height as usize + 2);
        for stop in 1..=checks {
            let mut steps = 0;
            let error = encode_rgba_png_cancellable(&pixels, || {
                steps += 1;
                steps >= stop
            })
            .unwrap_err();
            assert!(steps >= stop);
            assert!(matches!(
                error.kind(),
                std::io::ErrorKind::Interrupted | std::io::ErrorKind::Other
            ));
        }
    }
}

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

pub(crate) fn decoded(image: &crate::model::PaneImage) -> std::io::Result<PixelBuffer> {
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

/// Equivalent image-0.25.10 Lanczos3 passes for RGBA8. Checkpoints occur within
/// tap accumulation (at most 64 taps), retaining f32 operation order/rounding.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)] // Validated raster dimensions and clamped channel output.
pub(crate) fn resize_rgba_cancellable(
    source: &image::RgbaImage,
    width: u32,
    height: u32,
    mut cancelled: impl FnMut() -> bool,
) -> std::io::Result<image::RgbaImage> {
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
    let (sw, sh) = source.dimensions();
    if sw == 0
        || sh == 0
        || width == 0
        || height == 0
        || sw > 4096
        || sh > 4096
        || width > 4096
        || height > 4096
    {
        return Err(std::io::Error::other("invalid resize dimensions"));
    }
    if (sw, sh) == (width, height) {
        return Ok(source.clone());
    }
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
    let mut vertical = image::ImageBuffer::<image::Rgba<f32>, Vec<f32>>::new(sw, height);
    for y in 0..height {
        check()?;
        let (left, ws) = weights(sh, height, y, &mut check)?;
        for x in 0..sw {
            let mut sums = [0.0f32; 4];
            for (i, weight) in ws.iter().enumerate() {
                if i % 64 == 0 {
                    check()?;
                }
                let pixel = source.get_pixel(x, left + i as u32);
                for channel in 0..4 {
                    sums[channel] += f32::from(pixel[channel]) * weight;
                }
            }
            vertical.put_pixel(x, y, image::Rgba(sums));
        }
    }
    let mut output = image::RgbaImage::new(width, height);
    for x in 0..width {
        check()?;
        let (left, ws) = weights(sw, width, x, &mut check)?;
        for y in 0..height {
            let mut sums = [0.0f32; 4];
            for (i, weight) in ws.iter().enumerate() {
                if i % 64 == 0 {
                    check()?;
                }
                let pixel = vertical.get_pixel(left + i as u32, y);
                for channel in 0..4 {
                    sums[channel] += pixel[channel] * weight;
                }
            }
            output.put_pixel(
                x,
                y,
                image::Rgba(sums.map(|v| v.clamp(0.0, 255.0).round() as u8)),
            );
        }
    }
    Ok(output)
}

#[cfg(test)]
mod resize_cancellation_tests {
    use super::*;
    #[test]
    fn lanczos_pixels_match_locked_image_and_cancel_inside_both_passes() {
        for (sw, sh, w, h) in [
            (7, 13, 19, 5),
            (31, 17, 8, 29),
            (1, 9, 7, 1),
            (20, 20, 20, 20),
        ] {
            let source = image::RgbaImage::from_fn(sw, sh, |x, y| {
                image::Rgba([
                    ((x * 71 + y * 29) % 256) as u8,
                    ((x * 17 + y * 101) % 256) as u8,
                    ((x * 89 + y * 11) % 256) as u8,
                    ((x * 31 + y * 37) % 256) as u8,
                ])
            });
            let expected =
                image::imageops::resize(&source, w, h, image::imageops::FilterType::Lanczos3);
            let mut total = 0;
            let actual = resize_rgba_cancellable(&source, w, h, || {
                total += 1;
                false
            })
            .unwrap();
            assert_eq!(actual, expected, "{sw}x{sh} -> {w}x{h}");
            for stop in [1, total / 3 + 1, total * 2 / 3 + 1, total] {
                let mut steps = 0;
                let error = resize_rgba_cancellable(&source, w, h, || {
                    steps += 1;
                    steps == stop
                })
                .unwrap_err();
                assert_eq!(steps, stop);
                assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
            }
        }
    }
}

/// Decode PNG rows with the locked codec's expansion/interlace implementation.
/// Limit metadata allocations as well as dimensions before raster allocation.
pub(crate) fn decode_png_cancellable(
    data: &[u8],
    max_bytes: usize,
    mut cancelled: impl FnMut() -> bool,
) -> std::io::Result<PixelBuffer> {
    let callback = std::cell::RefCell::new(&mut cancelled);
    let check = || {
        if (callback.borrow_mut())() {
            Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "PNG decoding cancelled",
            ))
        } else {
            Ok(())
        }
    };
    check()?;
    if data.len() > max_bytes {
        return Err(std::io::Error::other("PNG input exceeds budget"));
    }
    // Cap each codec input observation, including read_info metadata parsing.
    // Terminal errors (not Interrupted) prevent read_exact retry loops.
    struct Input<'a, F> {
        cursor: std::io::Cursor<&'a [u8]>,
        callback: &'a std::cell::RefCell<F>,
    }
    impl<F: FnMut() -> bool> std::io::Read for Input<'_, F> {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            if (self.callback.borrow_mut())() {
                return Err(std::io::Error::other("PNG input cancelled"));
            }
            let size = bytes.len().min(4096);
            std::io::Read::read(&mut self.cursor, &mut bytes[..size])
        }
    }
    impl<F: FnMut() -> bool> std::io::BufRead for Input<'_, F> {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            if (self.callback.borrow_mut())() {
                return Err(std::io::Error::other("PNG input cancelled"));
            }
            let bytes = std::io::BufRead::fill_buf(&mut self.cursor)?;
            Ok(&bytes[..bytes.len().min(4096)])
        }
        fn consume(&mut self, amount: usize) {
            std::io::BufRead::consume(&mut self.cursor, amount);
        }
    }
    impl<F: FnMut() -> bool> std::io::Seek for Input<'_, F> {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            if (self.callback.borrow_mut())() {
                return Err(std::io::Error::other("PNG input cancelled"));
            }
            std::io::Seek::seek(&mut self.cursor, pos)
        }
    }
    let input = Input {
        cursor: std::io::Cursor::new(data),
        callback: &callback,
    };
    let mut decoder = png::Decoder::new_with_limits(input, png::Limits { bytes: max_bytes });
    // Ancillary compressed metadata is not needed for the existing raw-pixel
    // result. Avoid uninterruptible metadata decompression inside read_info.
    decoder.set_ignore_text_chunk(true);
    decoder.set_ignore_iccp_chunk(true);
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().map_err(|error| {
        check()
            .err()
            .unwrap_or_else(|| std::io::Error::other(error))
    })?;
    check()?;
    let (width, height) = (reader.info().width, reader.info().height);
    let (color, depth) = reader.output_color_type();
    let channels = color.samples();
    let sample_bytes = if depth == png::BitDepth::Sixteen {
        2
    } else {
        1
    };
    let stride = width as usize * channels * sample_bytes;
    let length = stride
        .checked_mul(height as usize)
        .ok_or_else(|| std::io::Error::other("PNG size overflow"))?;
    if width == 0
        || height == 0
        || width > 4096
        || height > 4096
        || length > max_bytes
        || u64::from(width) * u64::from(height) * 4 > max_bytes as u64
    {
        return Err(std::io::Error::other("PNG dimensions exceed budget"));
    }
    let mut decoded = vec![0; length];
    let mut line = 0;
    loop {
        check()?;
        let Some(row) = reader.next_interlaced_row().map_err(|error| {
            check()
                .err()
                .unwrap_or_else(|| std::io::Error::other(error))
        })?
        else {
            break;
        };
        match row.interlace() {
            png::InterlaceInfo::Null(_) => {
                decoded[line * stride..(line + 1) * stride].copy_from_slice(row.data());
                line += 1;
            }
            png::InterlaceInfo::Adam7(info) => png::expand_interlaced_row(
                &mut decoded,
                stride,
                row.data(),
                info,
                (channels * sample_bytes * 8) as u8,
            ),
        }
    }
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for row in decoded.chunks_exact(stride) {
        check()?;
        for pixel in row.chunks_exact(channels * sample_bytes) {
            let channel = |i: usize| {
                if sample_bytes == 1 {
                    pixel[i]
                } else {
                    // image's u16 -> u8 conversion rounds a full-range sample.
                    let value = u32::from(u16::from_be_bytes([pixel[i * 2], pixel[i * 2 + 1]]));
                    ((value + 128) / 257) as u8
                }
            };
            match color {
                png::ColorType::Grayscale => {
                    rgba.extend_from_slice(&[channel(0), channel(0), channel(0), 255])
                }
                png::ColorType::GrayscaleAlpha => {
                    rgba.extend_from_slice(&[channel(0), channel(0), channel(0), channel(1)])
                }
                png::ColorType::Rgb => {
                    rgba.extend_from_slice(&[channel(0), channel(1), channel(2), 255])
                }
                png::ColorType::Rgba => {
                    rgba.extend_from_slice(&[channel(0), channel(1), channel(2), channel(3)])
                }
                png::ColorType::Indexed => {
                    return Err(std::io::Error::other("unexpanded PNG palette"));
                }
            }
        }
    }
    Ok(PixelBuffer {
        width,
        height,
        format: PixelFormat::Rgba8,
        data: rgba,
    })
}
