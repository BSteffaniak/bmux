//! Signature-based still image decoding. Animated GIFs contribute only their first frame.
use crate::Raster;
use zune_core::{bytestream::ZCursor, colorspace::ColorSpace, options::DecoderOptions};

mod bmp;
#[cfg(test)]
mod tests;

#[derive(Clone, Copy)]
struct Limits {
    width: u32,
    height: u32,
    alloc: u64,
}

impl Limits {
    fn output_len(self, w: u32, h: u32, channels: u8) -> Result<usize, String> {
        if w == 0 || h == 0 || w > self.width || h > self.height {
            return Err("image dimensions out of bounds".into());
        }
        (u64::from(w) * u64::from(h))
            .checked_mul(u64::from(channels))
            .filter(|&n| n <= self.alloc)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| "image exceeds allocation limit".into())
    }
}

fn buffer(len: usize) -> Result<Vec<u8>, String> {
    let mut v = Vec::new();
    v.try_reserve_exact(len).map_err(|e| e.to_string())?;
    v.resize(len, 0);
    Ok(v)
}

#[derive(Clone, Copy)]
enum Layout {
    Rgba,
    Native,
}

struct Samples {
    width: u32,
    height: u32,
    channels: u8,
    bytes: Vec<u8>,
}

impl Layout {
    fn channels(self, native: u8) -> u8 {
        match self {
            Self::Rgba => 4,
            Self::Native => native,
        }
    }
}

pub(crate) fn decode_other(
    bytes: &[u8],
    max_width: u32,
    max_height: u32,
    max_alloc: u64,
) -> Result<Raster, String> {
    let samples = decode_samples(bytes, max_width, max_height, max_alloc, Layout::Rgba)?;
    Ok(Raster {
        width: samples.width,
        height: samples.height,
        rgba: samples.bytes,
    })
}

pub(crate) fn transcode_other(
    bytes: &[u8],
    max_width: u32,
    max_height: u32,
    max_alloc: u64,
) -> Result<Vec<u8>, crate::TranscodeError> {
    let samples = decode_samples(bytes, max_width, max_height, max_alloc, Layout::Native)?;
    crate::encode_png(
        samples.width,
        samples.height,
        samples.channels,
        &samples.bytes,
    )
    .map_err(crate::TranscodeError::Encode)
}

fn decode_samples(
    bytes: &[u8],
    width: u32,
    height: u32,
    alloc: u64,
    layout: Layout,
) -> Result<Samples, String> {
    let limits = Limits {
        width,
        height,
        alloc,
    };
    match bytes {
        [0xff, 0xd8, 0xff, ..] => jpeg(bytes, limits, layout),
        _ if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") => gif(bytes, limits),
        [b'B', b'M', ..] => bmp::decode(bytes, limits, layout),
        _ => Err("unsupported image format".into()),
    }
}

fn jpeg(bytes: &[u8], limits: Limits, layout: Layout) -> Result<Samples, String> {
    let options = DecoderOptions::default()
        .set_strict_mode(false)
        .set_max_width(limits.width as usize)
        .set_max_height(limits.height as usize);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(bytes), options);
    decoder.decode_headers().map_err(|e| e.to_string())?;
    let (w, h) = decoder.dimensions().ok_or("missing JPEG dimensions")?;
    let (width, height) = (w as u32, h as u32); // JPEG dimensions are u16.
    let color = match decoder.input_colorspace() {
        Some(c @ (ColorSpace::Luma | ColorSpace::LumaA | ColorSpace::RGBA)) => c,
        _ => ColorSpace::RGB,
    };
    decoder.set_options(decoder.options().jpeg_set_out_colorspace(color));
    let channels = color.num_components();
    let output_channels =
        layout.channels(u8::try_from(channels).map_err(|_| "invalid JPEG color space")?);
    let len = limits.output_len(width, height, output_channels)?;
    let mut rgba = buffer(len)?;
    decoder
        .decode_into(&mut rgba[..len / usize::from(output_channels) * channels])
        .map_err(|e| e.to_string())?;
    // Expand in place, backwards so unread samples are never overwritten.
    if channels != usize::from(output_channels) {
        for i in (0..len / 4).rev() {
            let off = i * channels;
            let first = rgba[off];
            let pixel = match channels {
                1 => [first, first, first, 255],
                2 => [first, first, first, rgba[off + 1]],
                _ => [first, rgba[off + 1], rgba[off + 2], 255],
            };
            rgba[i * 4..i * 4 + 4].copy_from_slice(&pixel);
        }
    }
    Ok(Samples {
        width,
        height,
        channels: output_channels,
        bytes: rgba,
    })
}

fn gif(bytes: &[u8], limits: Limits) -> Result<Samples, String> {
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    // Preserve gif's default 50 MB cap on decoder-owned allocations (e.g.
    // metadata blocks), while also respecting smaller caller budgets.
    let metadata_budget = std::num::NonZeroU64::new(limits.alloc.min(50_000_000))
        .ok_or("image exceeds allocation limit")?;
    options.set_memory_limit(gif::MemoryLimit::Bytes(metadata_budget));
    let mut dec = options.read_info(bytes).map_err(|e| e.to_string())?;
    let (width, height) = (u32::from(dec.width()), u32::from(dec.height()));
    let len = limits.output_len(width, height, 4)?;
    let frame = dec
        .next_frame_info()
        .map_err(|e| e.to_string())?
        .ok_or("GIF has no frames")?;
    let (left, top, fw, fh) = (
        usize::from(frame.left),
        usize::from(frame.top),
        usize::from(frame.width),
        usize::from(frame.height),
    );
    let (w, h) = (width as usize, height as usize);
    // A zero-sized first frame leaves the whole logical screen transparent.
    let mut rgba = buffer(len)?;
    if left == 0 && fw == w && top + fh <= h {
        dec.read_into_buffer(&mut rgba[top * w * 4..(top + fh) * w * 4])
            .map_err(|e| e.to_string())?;
    } else {
        let frame_len = fw
            .checked_mul(fh)
            .and_then(|n| n.checked_mul(4))
            .ok_or("GIF frame too large")?;
        if frame_len as u64 > limits.alloc - len as u64 {
            return Err("GIF frame exceeds allocation limit".into());
        }
        let mut frame = buffer(frame_len)?;
        dec.read_into_buffer(&mut frame)
            .map_err(|e| e.to_string())?;
        let n = fw.min(w.saturating_sub(left)) * 4;
        if n > 0 {
            for (y, row) in frame
                .chunks_exact(fw * 4)
                .take(h.saturating_sub(top))
                .enumerate()
            {
                let start = ((top + y) * w + left) * 4;
                rgba[start..start + n].copy_from_slice(&row[..n]);
            }
        }
    }
    Ok(Samples {
        width,
        height,
        channels: 4,
        bytes: rgba,
    })
}
