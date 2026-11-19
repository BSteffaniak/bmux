use std::io::Cursor;

use ::png::{
    BitDepth, ColorType, Compression, Decoder, Encoder, Filter, Limits, Reader, Transformations,
};

use crate::Raster;

/// Encode tightly packed eight-bit grayscale, gray-alpha, RGB, or RGBA samples.
///
/// # Errors
/// Returns an error for invalid dimensions, channel count, or buffer length.
pub fn encode_png(width: u32, height: u32, channels: u8, bytes: &[u8]) -> Result<Vec<u8>, String> {
    let color = match channels {
        1 => ColorType::Grayscale,
        2 => ColorType::GrayscaleAlpha,
        3 => ColorType::Rgb,
        4 => ColorType::Rgba,
        _ => return Err("unsupported PNG channel count".into()),
    };
    if width == 0 || height == 0 || byte_len(width, height, channels)? != bytes.len() {
        return Err("invalid PNG dimensions or buffer length".into());
    }
    encode_samples(width, height, color, BitDepth::Eight, bytes)
}

fn encode_samples(
    width: u32,
    height: u32,
    color: ColorType,
    depth: BitDepth,
    bytes: &[u8],
) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    let mut encoder = Encoder::new(&mut output, width, height);
    encoder.set_color(color);
    encoder.set_depth(depth);
    // PngEncoder::new used Fast (not Balanced) and explicitly selected Adaptive.
    encoder.set_compression(Compression::Fast);
    encoder.set_filter(Filter::Adaptive);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(bytes).map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())?;
    Ok(output)
}

fn byte_len(width: u32, height: u32, channels: u8) -> Result<usize, String> {
    (u64::from(width) * u64::from(height))
        .checked_mul(u64::from(channels))
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| "PNG buffer length overflow".to_owned())
}

fn open(
    bytes: &[u8],
    max_width: u32,
    max_height: u32,
    max_alloc: u64,
) -> Result<Reader<Cursor<&[u8]>>, String> {
    let mut decoder = Decoder::new_with_limits(
        Cursor::new(bytes),
        Limits {
            bytes: usize::try_from(max_alloc).unwrap_or(usize::MAX),
        },
    );
    decoder.set_ignore_text_chunk(false);
    let header = decoder.read_header_info().map_err(|e| e.to_string())?;
    if header.width > max_width || header.height > max_height {
        return Err("PNG dimensions exceed limit".to_owned());
    }
    // Expand packed gray, palettes and tRNS. STRIP_16 would truncate, not round.
    decoder.set_transformations(Transformations::EXPAND);
    decoder.read_info().map_err(|e| e.to_string())
}

/// Read PNG canvas dimensions without decoding the frame payload.
///
/// # Errors
/// Returns an error if decoder initialization fails or exceeds its memory limit.
pub fn png_dimensions(bytes: &[u8], max_alloc: u64) -> Result<(u32, u32), String> {
    Ok(open(bytes, u32::MAX, u32::MAX, max_alloc)?.info().size())
}

/// Normalize the first frame using the same sample layout as the native decode.
/// Like DynamicImage::write_to, write fresh metadata rather than copying input
/// color-space, EXIF, or text chunks. Sixteen-bit samples stay in PNG byte order.
pub(crate) fn transcode_native_png(
    bytes: &[u8],
    max_width: u32,
    max_height: u32,
    max_alloc: u64,
) -> Result<Vec<u8>, crate::TranscodeError> {
    let reader = open(bytes, max_width, max_height, max_alloc)?;
    let (width, height) = reader.info().size();
    let (color, depth) = reader.output_color_type();
    let samples = read_frame(reader, max_alloc, 0)?;
    encode_samples(width, height, color, depth, &samples).map_err(crate::TranscodeError::Encode)
}

fn read_frame(
    mut reader: Reader<Cursor<&[u8]>>,
    max_alloc: u64,
    min_capacity: usize,
) -> Result<Vec<u8>, String> {
    let len = reader
        .output_buffer_size()
        .ok_or("PNG buffer length overflow")?;
    let capacity = min_capacity.max(len);
    if u64::try_from(capacity).map_err(|e| e.to_string())? > max_alloc {
        return Err("PNG buffer exceeds allocation limit".to_owned());
    }
    let mut samples = Vec::new();
    samples
        .try_reserve_exact(capacity)
        .map_err(|e| e.to_string())?;
    samples.resize(len, 0);
    reader.next_frame(&mut samples).map_err(|e| e.to_string())?;
    Ok(samples)
}

/// Decode the first PNG frame, verifying the exact canvas dimensions without
/// expanding the native samples to an unnecessary RGBA buffer.
///
/// # Errors
/// Returns an error for invalid data, mismatched dimensions, or exceeded limits.
pub fn validate_png(bytes: &[u8], width: u32, height: u32, max_alloc: u64) -> Result<(), String> {
    let reader = open(bytes, width, height, max_alloc)?;
    if reader.info().size() != (width, height) {
        return Err("image dimensions mismatch".to_owned());
    }
    read_frame(reader, max_alloc, 0).map(drop)
}

pub(crate) fn decode_png(
    bytes: &[u8],
    max_width: u32,
    max_height: u32,
    max_alloc: u64,
) -> Result<Raster, String> {
    let reader = open(bytes, max_width, max_height, max_alloc)?;
    let (width, height) = reader.info().size();
    let (color, depth) = reader.output_color_type();
    let channels = color.samples();
    let len = byte_len(width, height, 4)?;
    let mut rgba = read_frame(reader, max_alloc, len)?;
    if depth == BitDepth::Sixteen {
        // PNG samples are big-endian. Round to the nearest eight-bit sample,
        // including alpha, compacting forward before expanding to four channels.
        let sample_count = rgba.len() / 2;
        for i in 0..sample_count {
            let value = u16::from_be_bytes([rgba[2 * i], rgba[2 * i + 1]]);
            rgba[i] = ((u32::from(value) + 128) / 257) as u8;
        }
        rgba.truncate(sample_count);
    }
    if color != ColorType::Rgba {
        rgba.resize(len, 0);
        // Work backwards so the expanded pixel cannot overwrite unread samples.
        for i in (0..len / 4).rev() {
            let p = i * channels;
            let gray = rgba[p];
            let pixel = match color {
                ColorType::Grayscale => [gray, gray, gray, 255],
                ColorType::GrayscaleAlpha => [gray, gray, gray, rgba[p + 1]],
                ColorType::Rgb => [rgba[p], rgba[p + 1], rgba[p + 2], 255],
                _ => return Err("unsupported expanded PNG color type".into()),
            };
            rgba[4 * i..4 * i + 4].copy_from_slice(&pixel);
        }
    }
    Ok(Raster {
        width,
        height,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimensions_needs_decoder_info_but_not_a_complete_frame() {
        let png = encode_png(32, 1024, 1, &vec![0; 32768]).unwrap();
        let start = png.windows(4).position(|b| b == b"IDAT").unwrap();
        let partial = &png[..start + 4];
        assert_eq!(png_dimensions(partial, 32767).unwrap(), (32, 1024));
        assert!(png_dimensions(&png[..33], 32767).is_err());
        assert!(png_dimensions(&png, 0).is_err());
        assert!(transcode_native_png(partial, 32, 1024, 1 << 20).is_err());
        assert!(transcode_native_png(&png, 32, 1024, 32767).is_err());
        assert!(transcode_native_png(&png, 32, 1024, 32768).is_ok());
    }

    #[test]
    fn transcode_preserves_native_depth_and_samples_but_not_source_metadata() {
        let mut png = Vec::new();
        let mut encoder = Encoder::new(&mut png, 2, 1);
        encoder.set_color(ColorType::GrayscaleAlpha);
        encoder.set_depth(BitDepth::Sixteen);
        encoder.set_source_srgb(::png::SrgbRenderingIntent::Perceptual);
        let samples = [0, 99, 255, 123, 7, 55, 3, 11];
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&samples).unwrap();
        writer.finish().unwrap();
        assert!(open(&png, 2, 1, 1 << 20).unwrap().info().srgb.is_some());
        let output = transcode_native_png(&png, 2, 1, 1 << 20).unwrap();
        let reader = open(&output, 2, 1, 1 << 20).unwrap();
        assert_eq!(
            reader.output_color_type(),
            (ColorType::GrayscaleAlpha, BitDepth::Sixteen)
        );
        assert!(reader.info().srgb.is_none());
        assert_eq!(read_frame(reader, 1 << 20, 0).unwrap(), samples);
    }

    #[test]
    fn roundtrip_each_channel_layout() {
        for (channels, data, expected) in [
            (1, vec![9, 3], vec![9, 9, 9, 255, 3, 3, 3, 255]),
            (2, vec![9, 10, 3, 4], vec![9, 9, 9, 10, 3, 3, 3, 4]),
            (3, vec![1, 2, 3, 4, 5, 6], vec![1, 2, 3, 255, 4, 5, 6, 255]),
            (
                4,
                vec![1, 2, 3, 4, 5, 6, 7, 8],
                vec![1, 2, 3, 4, 5, 6, 7, 8],
            ),
        ] {
            let png = encode_png(2, 1, channels, &data).unwrap();
            let raster = decode_png(&png, 2, 1, 1024 * 1024).unwrap();
            assert_eq!((raster.width, raster.height), (2, 1));
            assert_eq!(raster.rgba, expected);
        }
    }

    #[test]
    fn rounds_sixteen_bit_gray_and_alpha() {
        let mut png = Vec::new();
        let mut encoder = Encoder::new(&mut png, 2, 1);
        encoder.set_depth(BitDepth::Sixteen);
        encoder.set_color(ColorType::GrayscaleAlpha);
        let mut writer = encoder.write_header().unwrap();
        writer
            .write_image_data(&[0, 129, 255, 0, 0, 128, 255, 255])
            .unwrap();
        writer.finish().unwrap();
        assert_eq!(
            decode_png(&png, 2, 1, 1 << 20).unwrap().rgba,
            [1, 1, 1, 254, 0, 0, 0, 255]
        );
    }

    #[test]
    fn expands_packed_palette_and_transparency() {
        let mut png = Vec::new();
        let mut encoder = Encoder::new(&mut png, 2, 1);
        encoder.set_depth(BitDepth::One);
        encoder.set_color(ColorType::Indexed);
        encoder.set_palette([1, 2, 3, 4, 5, 6].as_slice());
        encoder.set_trns([7].as_slice());
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[0x40]).unwrap();
        writer.finish().unwrap();
        assert_eq!(
            decode_png(&png, 2, 1, 1 << 20).unwrap().rgba,
            [1, 2, 3, 7, 4, 5, 6, 255]
        );
    }

    #[test]
    fn checks_dimensions_frame_data_and_allocation_before_conversion() {
        let png = encode_png(32, 1024, 1, &vec![0; 32768]).unwrap();
        validate_png(&png, 32, 1024, 32768).unwrap();
        assert!(validate_png(&png, 32, 1024, 32767).is_err());
        assert!(validate_png(&png, 33, 1024, 1 << 20).is_err());
        assert!(decode_png(&png, 31, 1024, 1 << 20).is_err());
        assert!(decode_png(&png, 32, 1024, 32768).is_err());
        let idat = png.windows(4).position(|b| b == b"IDAT").unwrap();
        assert!(validate_png(&png[..idat + 5], 32, 1024, 32768).is_err());
        assert!(encode_png(1, 1, 0, &[]).is_err());
        assert!(encode_png(1, 1, 3, &[0; 4]).is_err());
        assert!(encode_png(u32::MAX, u32::MAX, 4, &[]).is_err());
    }
}
