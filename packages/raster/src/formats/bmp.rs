//! Decode the BMP DIB variants used by the interchange path. BMP offsets are
//! file-relative; pixel rows in uncompressed DIBs are DWORD-aligned.
use super::{Layout, Limits, Samples, buffer};

const EOF: &str = "truncated BMP";
fn field<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N], String> {
    bytes
        .get(offset..)
        .and_then(|rest| rest.first_chunk())
        .copied()
        .ok_or_else(|| EOF.into())
}
fn word(b: &[u8], p: usize) -> Result<u16, String> {
    Ok(u16::from_le_bytes(field(b, p)?))
}
fn dword(b: &[u8], p: usize) -> Result<u32, String> {
    Ok(u32::from_le_bytes(field(b, p)?))
}

type Palette = [[u8; 4]; 256];

// BMP bitfields use contiguous runs. For fields wider than eight bits the
// reference decoder takes the high eight; narrower fields scale to 0..255.
#[derive(Clone, Copy, Default)]
struct Mask {
    shift: u32,
    range: u32,
}
impl Mask {
    fn new(bits: u32, depth: u16) -> Result<Self, String> {
        if bits == 0 {
            return Ok(Self::default());
        }
        let n = bits.count_ones();
        let shift = bits.trailing_zeros();
        if (bits >> shift).trailing_ones() != n || shift + n > u32::from(depth) {
            return Err("invalid BMP bitfield".into());
        }
        Ok(Self {
            shift: shift + n.saturating_sub(8),
            range: (1 << n.min(8)) - 1,
        })
    }
    fn sample(self, v: u32) -> u8 {
        let scaled = (v >> self.shift & self.range) * 255 + self.range / 2;
        scaled.checked_div(self.range).unwrap_or(255) as u8
    }
}

enum Encoding {
    Indexed(u16),
    Bgr(usize),
    Bitfields(usize, [Mask; 4]),
    Rle(u16),
}

pub(super) fn decode(b: &[u8], limits: Limits, layout: Layout) -> Result<Samples, String> {
    let off = dword(b, 10)? as usize;
    let header = dword(b, 14)? as usize;
    if !matches!(header, 12 | 40 | 52 | 56 | 108 | 124) {
        return Err(format!("unsupported BMP DIB header {header}"));
    }
    let core = header == 12;
    let (width, height, planes, depth, mode, colors) = if core {
        (
            i32::from(word(b, 18)?),
            i32::from(word(b, 20)?),
            word(b, 22)?,
            word(b, 24)?,
            0,
            0,
        )
    } else {
        (
            i32::from_le_bytes(field(b, 18)?),
            i32::from_le_bytes(field(b, 22)?),
            word(b, 26)?,
            word(b, 28)?,
            dword(b, 30)?,
            dword(b, 46)?,
        )
    };
    if planes != 1 || !(1..=65535).contains(&width) || height == i32::MIN || height > 65535 {
        return Err("invalid BMP geometry".into());
    }
    let top_down = height < 0;
    let (width, height) = (width as u32, height.unsigned_abs());
    if core && !matches!(depth, 1 | 4 | 8 | 24) || top_down && !matches!(mode, 0 | 3) {
        return Err("unsupported BMP encoding".into());
    }
    let encoding = match (mode, depth) {
        (0, 1 | 2 | 4 | 8) => Encoding::Indexed(depth),
        (0, 24 | 32) => Encoding::Bgr(usize::from(depth / 8)),
        (0, 16) | (3, 16 | 32) => {
            let bits = if mode == 0 {
                [0x7c00, 0x03e0, 0x001f, 0]
            } else {
                [
                    dword(b, 54)?,
                    dword(b, 58)?,
                    dword(b, 62)?,
                    if header >= 56 { dword(b, 66)? } else { 0 },
                ]
            };
            if bits[..3].contains(&0) {
                return Err("missing BMP RGB masks".into());
            }
            let [r, g, blue, a] = bits.map(|v| Mask::new(v, depth));
            Encoding::Bitfields(usize::from(depth / 8), [r?, g?, blue?, a?])
        }
        (1, 8) | (2, 4) => Encoding::Rle(depth),
        _ => {
            return Err(format!(
                "unsupported BMP compression {mode} at {depth} bits"
            ));
        }
    };
    let has_alpha = matches!(&encoding, Encoding::Bitfields(_, masks) if masks[3].range != 0);
    let channels = layout.channels(if has_alpha { 4 } else { 3 });
    let len = limits.output_len(width, height, channels)?;
    let pixel_size = usize::from(channels);
    let mut palette = [[0, 0, 0, 255]; 256];
    if depth <= 8 {
        let count = if colors == 0 { 1 << depth } else { colors };
        if count > 1 << depth {
            return Err("invalid BMP palette size".into());
        }
        let entry = if core { 3 } else { 4 };
        let start = 14 + header;
        let data = b.get(start..start + count as usize * entry).ok_or(EOF)?;
        for (dst, p) in palette.iter_mut().zip(data.chunks_exact(entry)) {
            *dst = [p[2], p[1], p[0], 255];
        }
    }
    let data = b.get(off..).ok_or(EOF)?;
    let w = width as usize;
    let rgba = if let Encoding::Rle(bits) = encoding {
        let mut rgba = buffer(len)?;
        if channels == 4 {
            for p in rgba.as_chunks_mut::<4>().0 {
                p[3] = 255;
            }
        }
        rle(data, &mut rgba, w, bits, &palette, pixel_size)?;
        rgba
    } else {
        let stride = (u64::from(width) * u64::from(depth)).div_ceil(32) * 4;
        if stride * u64::from(height) > data.len() as u64 {
            return Err(EOF.into());
        }
        let mut rgba = buffer(len)?;
        for (y, line) in rgba.chunks_exact_mut(w * pixel_size).enumerate() {
            let src_y = if top_down { y } else { height as usize - 1 - y };
            let start = src_y * stride as usize;
            let row = &data[start..start + stride as usize];
            for (x, p) in line.chunks_exact_mut(pixel_size).enumerate() {
                let rgba = match &encoding {
                    Encoding::Indexed(bits) => palette[index(row, x, *bits)],
                    Encoding::Bgr(size) => {
                        let i = x * size;
                        [row[i + 2], row[i + 1], row[i], 255]
                    }
                    Encoding::Bitfields(size, masks) => {
                        let i = x * size;
                        let v = if *size == 2 {
                            u32::from(word(row, i)?)
                        } else {
                            dword(row, i)?
                        };
                        masks.map(|m| m.sample(v))
                    }
                    Encoding::Rle(_) => unreachable!(),
                };
                p.copy_from_slice(&rgba[..pixel_size]);
            }
        }
        rgba
    };
    Ok(Samples {
        width,
        height,
        channels,
        bytes: rgba,
    })
}

fn index(bytes: &[u8], x: usize, bits: u16) -> usize {
    let bits = usize::from(bits);
    let offset = x * bits;
    usize::from(bytes[offset / 8]) >> (8 - bits - offset % 8) & ((1 << bits) - 1)
}

fn rle(
    mut data: &[u8],
    rgba: &mut [u8],
    w: usize,
    bits: u16,
    palette: &Palette,
    pixel_size: usize,
) -> Result<(), String> {
    let h = rgba.len() / pixel_size / w;
    let (mut x, mut y) = (0usize, 0usize);
    while y < h {
        let (&[count, op], rest) = data.split_first_chunk().ok_or(EOF)?;
        data = rest;
        let n = usize::from(count);
        let (n, source) = match (count, op) {
            (0, 0) => {
                x = 0;
                y += 1;
                continue;
            }
            (0, 1) => break,
            (0, 2) => {
                let (&[dx, dy], rest) = data.split_first_chunk().ok_or(EOF)?;
                data = rest;
                x = x
                    .checked_add(usize::from(dx))
                    .ok_or("BMP RLE delta overflow")?;
                y += usize::from(dy);
                if x > w || y >= h {
                    return Err("BMP RLE delta outside image".into());
                }
                continue;
            }
            (0, count) => {
                let n = usize::from(count);
                let len = (n * usize::from(bits)).div_ceil(8).next_multiple_of(2);
                let (src, rest) = data.split_at_checked(len).ok_or(EOF)?;
                data = rest;
                (n, Some(src))
            }
            // Some writers produce RLE8 encoded runs that exceed a row.
            _ if bits == 8 => (n.min(w - x), None),
            _ => (n, None),
        };
        if n > w - x {
            return Err("BMP RLE run crosses scanline".into());
        }
        let offset = ((h - 1 - y) * w + x) * pixel_size;
        for (i, p) in rgba[offset..offset + pixel_size * n]
            .chunks_exact_mut(pixel_size)
            .enumerate()
        {
            let k = source.map_or_else(
                || index(&[op], if bits == 4 { i % 2 } else { 0 }, bits),
                |src| index(src, i, bits),
            );
            p.copy_from_slice(&palette[k][..pixel_size]);
        }
        x += n;
    }
    Ok(())
}
