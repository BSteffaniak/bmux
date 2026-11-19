//! Copy this file alone into the pinned shipping checkout's clipboard examples.
//! Uses only the public API shared by shipping and candidate, plus `std`: the
//! checkouts depend on different image crates, so PNG handling is local.
use bmux_clipboard::{ClipboardPayload, copy_png_image, copy_text, read_payload};
use std::io::{Read, Write};
use std::process::ExitCode;

const WIDTH: u32 = 37;
const HEIGHT: u32 = 29;

fn run() -> Result<(), &'static str> {
    if std::env::var("BMUX_CLIPBOARD_ISOLATED_TEST").as_deref() != Ok("1") {
        return Err("refusing non-isolated clipboard");
    }
    // Bound standalone invocations too, including a blocked backend or control pipe.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(60));
        std::process::exit(124);
    });
    let args: Vec<_> = std::env::args().skip(1).collect();
    let [operation, case, seed] = args.as_slice() else {
        return Err("expected write|check empty|text|image SEED");
    };
    let seed = seed.parse::<u32>().map_err(|_| "invalid seed")?;
    // Opaque pixels avoid platform-dependent rounding of premultiplied alpha.
    let rgba: Vec<u8> = (0..WIDTH * HEIGHT)
        .flat_map(|i| {
            let [r, g, b, _] = (seed ^ i.wrapping_mul(0x9e37_79b9)).to_le_bytes();
            [r, g, b, 255]
        })
        .collect();
    let text = match case.as_str() {
        "text" => format!("bmux {seed} 界 🦀 e\u{301}\n").repeat(8192),
        "empty" | "image" => String::new(),
        _ => return Err("unknown case"),
    };
    match operation.as_str() {
        "write" => {
            if case == "image" {
                copy_png_image(&png::encode(WIDTH, HEIGHT, &rgba))
                    .map_err(|_| "image write failed")?;
            } else {
                copy_text(&text).map_err(|_| "text write failed")?;
            }
            println!("ready");
            std::io::stdout().flush().map_err(|_| "ready failed")?;
            // Harness closes its pipe to request normal exit (including owner handoff).
            std::io::stdin()
                .read(&mut [0])
                .map_err(|_| "control pipe failed")?;
        }
        "check" => {
            let payload = read_payload(true).map_err(|_| "payload read failed")?;
            let bytes = match payload {
                ClipboardPayload::Text(actual) if case != "image" && actual == text => {
                    actual.into_bytes()
                }
                ClipboardPayload::ImagePng(encoded) if case == "image" => {
                    let (width, height, actual) = png::decode(&encoded).ok_or("invalid PNG")?;
                    if (width, height) != (WIDTH, HEIGHT) || actual != rgba {
                        return Err("RGBA mismatch");
                    }
                    actual
                }
                _ => return Err("flavor or content mismatch"),
            };
            // Digest is diagnostic; correctness above requires exact bytes and dimensions.
            let digest = bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
                (h ^ u64::from(*b)).wrapping_mul(0x100_0000_01b3)
            });
            println!("bytes={} fnv1a64={digest:016x}", bytes.len());
        }
        _ => return Err("unknown operation"),
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(reason) => {
            eprintln!("{reason}");
            ExitCode::FAILURE
        }
    }
}

/// Just enough PNG for this harness: 8-bit RGBA, no interlacing. Both checkouts
/// re-encode clipboard images in that layout; anything else fails the check.
mod png {
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0_u32;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb8_8320 & (crc & 1).wrapping_neg());
            }
        }
        !crc
    }

    fn adler32(bytes: &[u8]) -> u32 {
        let (mut a, mut b) = (1_u32, 0_u32);
        for &byte in bytes {
            a = (a + u32::from(byte)) % 65_521;
            b = (b + a) % 65_521;
        }
        (b << 16) | a
    }

    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        let len = u32::try_from(data.len()).expect("chunk fits u32");
        out.extend_from_slice(&len.to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let crc = crc32(&out[start..]);
        out.extend_from_slice(&crc.to_be_bytes());
    }

    /// Unfiltered rows in stored (uncompressed) deflate blocks.
    pub fn encode(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        let raw: Vec<u8> = rgba
            .chunks_exact(width as usize * 4)
            .flat_map(|row| std::iter::once(0).chain(row.iter().copied()))
            .collect();
        let mut zlib = vec![0x78, 0x01];
        let mut blocks = raw.chunks(usize::from(u16::MAX)).peekable();
        while let Some(block) = blocks.next() {
            zlib.push(u8::from(blocks.peek().is_none()));
            let len = u16::try_from(block.len()).expect("block fits u16");
            zlib.extend_from_slice(&len.to_le_bytes());
            zlib.extend_from_slice(&(!len).to_le_bytes());
            zlib.extend_from_slice(block);
        }
        zlib.extend_from_slice(&adler32(&raw).to_be_bytes());
        let mut header = Vec::new();
        header.extend_from_slice(&width.to_be_bytes());
        header.extend_from_slice(&height.to_be_bytes());
        header.extend_from_slice(&[8, 6, 0, 0, 0]);
        let mut png = SIGNATURE.to_vec();
        chunk(&mut png, b"IHDR", &header);
        chunk(&mut png, b"IDAT", &zlib);
        chunk(&mut png, b"IEND", &[]);
        png
    }

    pub fn decode(png: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
        let mut rest = png.strip_prefix(SIGNATURE)?;
        let (mut header, mut zlib) = (None, Vec::new());
        loop {
            let len = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
            let body = rest.get(8..8 + len)?;
            match rest.get(4..8)? {
                b"IHDR" => header = Some(body),
                b"IDAT" => zlib.extend_from_slice(body),
                b"IEND" => break,
                _ => {}
            }
            rest = rest.get(12 + len..)?;
        }
        let header = header?;
        let width = u32::from_be_bytes(header.get(..4)?.try_into().ok()?);
        let height = u32::from_be_bytes(header.get(4..8)?.try_into().ok()?);
        if header.get(8..13)? != [8, 6, 0, 0, 0] {
            return None;
        }
        let raw = inflate::inflate(zlib.get(2..)?)?;
        let stride = width as usize * 4;
        if raw.len() < (stride + 1).checked_mul(height as usize)? {
            return None;
        }
        let mut rgba = Vec::with_capacity(stride * height as usize);
        let mut previous = vec![0; stride];
        for line in raw.chunks_exact(stride + 1).take(height as usize) {
            let mut row = vec![0_u8; stride];
            for i in 0..stride {
                let left = if i >= 4 { row[i - 4] } else { 0 };
                let up = previous[i];
                let corner = if i >= 4 { previous[i - 4] } else { 0 };
                let prediction = match line[0] {
                    0 => 0,
                    1 => left,
                    2 => up,
                    3 => ((u16::from(left) + u16::from(up)) / 2) as u8,
                    4 => paeth(left, up, corner),
                    _ => return None,
                };
                row[i] = line[1 + i].wrapping_add(prediction);
            }
            rgba.extend_from_slice(&row);
            previous = row;
        }
        Some((width, height, rgba))
    }

    fn paeth(a: u8, b: u8, c: u8) -> u8 {
        let p = i16::from(a) + i16::from(b) - i16::from(c);
        let (pa, pb, pc) = (
            (p - i16::from(a)).abs(),
            (p - i16::from(b)).abs(),
            (p - i16::from(c)).abs(),
        );
        if pa <= pb && pa <= pc {
            a
        } else if pb <= pc {
            b
        } else {
            c
        }
    }

    /// RFC 1951 inflate, after zlib's `puff` reference decoder.
    mod inflate {
        const LENGTH_BASE: [u16; 29] = [
            3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99,
            115, 131, 163, 195, 227, 258,
        ];
        const LENGTH_EXTRA: [u8; 29] = [
            0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
        ];
        const DISTANCE_BASE: [u16; 30] = [
            1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025,
            1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
        ];
        const DISTANCE_EXTRA: [u8; 30] = [
            0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12,
            12, 13, 13,
        ];
        const CODE_LENGTH_ORDER: [usize; 19] = [
            16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
        ];

        struct Bits<'a> {
            data: &'a [u8],
            pos: usize,
            buffer: u32,
            count: u32,
        }

        impl Bits<'_> {
            fn take(&mut self, need: u32) -> Option<u32> {
                let mut value = self.buffer;
                while self.count < need {
                    value |= u32::from(*self.data.get(self.pos)?) << self.count;
                    self.pos += 1;
                    self.count += 8;
                }
                self.buffer = value >> need;
                self.count -= need;
                Some(value & ((1 << need) - 1))
            }
        }

        struct Huffman {
            count: [u16; 16],
            symbol: Vec<u16>,
        }

        fn huffman(lengths: &[u8]) -> Huffman {
            let mut count = [0_u16; 16];
            for &length in lengths {
                count[usize::from(length)] += 1;
            }
            let mut offset = [0_u16; 16];
            for length in 1..15 {
                offset[length + 1] = offset[length] + count[length];
            }
            let mut symbol = vec![0; lengths.len()];
            for (value, &length) in lengths.iter().enumerate() {
                if length != 0 {
                    let slot = &mut offset[usize::from(length)];
                    symbol[usize::from(*slot)] = value as u16;
                    *slot += 1;
                }
            }
            Huffman { count, symbol }
        }

        fn decode(bits: &mut Bits, table: &Huffman) -> Option<usize> {
            let (mut code, mut first, mut index) = (0_usize, 0_usize, 0_usize);
            for &count in &table.count[1..] {
                code |= bits.take(1)? as usize;
                let count = usize::from(count);
                if code < first + count {
                    return table
                        .symbol
                        .get(index + code - first)
                        .map(|&s| usize::from(s));
                }
                index += count;
                first = (first + count) << 1;
                code <<= 1;
            }
            None
        }

        fn codes(
            bits: &mut Bits,
            out: &mut Vec<u8>,
            lengths: &Huffman,
            distances: &Huffman,
        ) -> Option<()> {
            loop {
                let symbol = decode(bits, lengths)?;
                match symbol {
                    0..=255 => out.push(symbol as u8),
                    256 => return Some(()),
                    _ => {
                        let i = symbol - 257;
                        let len = usize::from(*LENGTH_BASE.get(i)?)
                            + bits.take(u32::from(LENGTH_EXTRA[i]))? as usize;
                        let d = decode(bits, distances)?;
                        let distance = usize::from(*DISTANCE_BASE.get(d)?)
                            + bits.take(u32::from(DISTANCE_EXTRA[d]))? as usize;
                        let start = out.len().checked_sub(distance)?;
                        for k in 0..len {
                            out.push(out[start + k]);
                        }
                    }
                }
            }
        }

        pub fn inflate(data: &[u8]) -> Option<Vec<u8>> {
            let mut bits = Bits {
                data,
                pos: 0,
                buffer: 0,
                count: 0,
            };
            let mut out = Vec::new();
            loop {
                let last = bits.take(1)? == 1;
                match bits.take(2)? {
                    0 => {
                        // Stored blocks restart on a byte boundary.
                        (bits.buffer, bits.count) = (0, 0);
                        let p = bits.pos;
                        let len =
                            usize::from(u16::from_le_bytes([*data.get(p)?, *data.get(p + 1)?]));
                        out.extend_from_slice(data.get(p + 4..p + 4 + len)?);
                        bits.pos = p + 4 + len;
                    }
                    1 => {
                        let mut fixed = [8_u8; 288];
                        fixed[144..256].fill(9);
                        fixed[256..280].fill(7);
                        codes(&mut bits, &mut out, &huffman(&fixed), &huffman(&[5; 30]))?;
                    }
                    2 => {
                        let literals = bits.take(5)? as usize + 257;
                        let distances = bits.take(5)? as usize + 1;
                        let header = bits.take(4)? as usize + 4;
                        let mut lengths = [0_u8; 320];
                        for &i in &CODE_LENGTH_ORDER[..header] {
                            lengths[i] = bits.take(3)? as u8;
                        }
                        let code_lengths = huffman(&lengths[..19]);
                        let total = literals + distances;
                        let mut i = 0;
                        while i < total {
                            let (value, repeat) = match decode(&mut bits, &code_lengths)? {
                                symbol @ 0..=15 => (symbol as u8, 1),
                                16 => {
                                    (*lengths.get(i.checked_sub(1)?)?, 3 + bits.take(2)? as usize)
                                }
                                17 => (0, 3 + bits.take(3)? as usize),
                                _ => (0, 11 + bits.take(7)? as usize),
                            };
                            lengths
                                .get_mut(i..i + repeat)
                                .filter(|_| i + repeat <= total)?
                                .fill(value);
                            i += repeat;
                        }
                        let table = huffman(&lengths[..literals]);
                        codes(
                            &mut bits,
                            &mut out,
                            &table,
                            &huffman(&lengths[literals..total]),
                        )?;
                    }
                    _ => return None,
                }
                if last {
                    return Some(out);
                }
            }
        }
    }
}
