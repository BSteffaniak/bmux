use image::{imageops, DynamicImage, ImageEncoder, ImageFormat, Rgba, RgbaImage};
use std::{fmt::Write, io::Cursor, path::Path};

fn hex(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len() * 2);
    for b in data {
        write!(s, "{b:02x}").unwrap();
    }
    s
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn pattern(w: u32, h: u32) -> RgbaImage {
    RgbaImage::from_fn(w, h, |x, y| {
        Rgba([
            (x * 71 + y * 29) as u8,
            (x * 17 + y * 101) as u8,
            (x * 89 + y * 11) as u8,
            (x * 31 + y * 37) as u8,
        ])
    })
}

fn reference_decode(data: &[u8], w: u32, h: u32, cap: u64) -> Result<(u32, u32, Vec<u8>), String> {
    let mut reader = image::ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(w);
    limits.max_image_height = Some(h);
    limits.max_alloc = Some(cap);
    reader.limits(limits);
    let pic = reader.decode().map_err(|e| e.to_string())?.into_rgba8();
    Ok((pic.width(), pic.height(), pic.into_raw()))
}

fn transform_results() -> String {
    let mut out = String::new();
    for (sw, sh, w, h) in [(7, 13, 19, 5), (31, 17, 8, 29), (1, 9, 7, 1), (3, 2, 3, 2)] {
        let src = pattern(sw, sh);
        for (name, f) in [
            ("nearest", imageops::FilterType::Nearest),
            ("lanczos3", imageops::FilterType::Lanczos3),
        ] {
            writeln!(
                out,
                "{name}\t{sw}\t{sh}\t{w}\t{h}\t{}",
                hex(&imageops::resize(&src, w, h, f))
            )
            .unwrap();
        }
    }
    let mut canvas = pattern(9, 7);
    imageops::overlay(&mut canvas, &pattern(7, 5), -2, 3);
    writeln!(out, "overlay-negative\t9\t7\t9\t7\t{}", hex(&canvas)).unwrap();
    writeln!(
        out,
        "crop-1-2\t9\t7\t4\t3\t{}",
        hex(&imageops::crop_imm(&pattern(9, 7), 1, 2, 4, 3).to_image())
    )
    .unwrap();
    out
}

fn format_results() -> String {
    let mut out = String::new();
    for (name, fmt) in [
        ("jpeg", ImageFormat::Jpeg),
        ("jpeg-gray", ImageFormat::Jpeg),
        ("gif-offset", ImageFormat::Gif),
        ("bmp", ImageFormat::Bmp),
        ("gif", ImageFormat::Gif),
    ] {
        let mut data = Cursor::new(Vec::new());
        let src = DynamicImage::ImageRgba8(pattern(9, 7));
        if fmt == ImageFormat::Gif {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut data);
            encoder
                .encode_frame(image::Frame::new(src.into_rgba8()))
                .unwrap();
            encoder
                .encode_frame(image::Frame::new(RgbaImage::new(9, 7)))
                .unwrap();
        } else if name == "jpeg-gray" {
            DynamicImage::ImageLuma8(src.into_luma8())
                .write_to(&mut data, fmt)
                .unwrap();
        } else {
            DynamicImage::ImageRgb8(src.into_rgb8())
                .write_to(&mut data, fmt)
                .unwrap();
        }
        if name == "gif-offset" {
            // Expand logical screen; move the first subimage one pixel from the top/left.
            let bytes = data.get_mut();
            bytes[6..10].copy_from_slice(&[11, 0, 9, 0]);
            let gce = bytes.windows(3).position(|s| s == [0x21, 0xf9, 4]).unwrap();
            let desc = gce + 8;
            assert_eq!(bytes[desc], 0x2c);
            bytes[desc + 1..desc + 5].copy_from_slice(&[1, 0, 1, 0]);
        }
        let (w, h, rgba) = reference_decode(data.get_ref(), 20, 20, 1 << 20).unwrap();
        writeln!(
            out,
            "{name}\t{w}\t{h}\t{}\t{}",
            hex(data.get_ref()),
            hex(&rgba)
        )
        .unwrap();
    }
    out
}

#[derive(Clone, Copy)]
enum Backend {
    Reference,
    #[cfg(feature = "candidate-png")]
    Candidate,
}
impl Backend {
    fn decode(self, data: &[u8], w: u32, h: u32, cap: u64) -> Result<(u32, u32, Vec<u8>), String> {
        match self {
            Self::Reference => reference_decode(data, w, h, cap),
            #[cfg(feature = "candidate-png")]
            Self::Candidate => {
                bmux_raster::decode(data, w, h, cap).map(|r| (r.width, r.height, r.rgba))
            }
        }
    }
    fn encode(self, w: u32, h: u32, c: u8, data: &[u8]) -> Result<Vec<u8>, String> {
        match self {
            Self::Reference => {
                let mut png = Vec::new();
                image::codecs::png::PngEncoder::new(&mut png)
                    .write_image(
                        data,
                        w,
                        h,
                        if c == 3 {
                            image::ExtendedColorType::Rgb8
                        } else {
                            image::ExtendedColorType::Rgba8
                        },
                    )
                    .map_err(|e| e.to_string())?;
                Ok(png)
            }
            #[cfg(feature = "candidate-png")]
            Self::Candidate => bmux_raster::encode_png(w, h, c, data),
        }
    }
}

#[cfg(feature = "candidate-png")]
fn compare_transforms(root: &Path) {
    use bmux_raster::{transforms, Raster};
    for row in std::fs::read_to_string(root.join("transforms.tsv"))
        .unwrap()
        .lines()
    {
        let f: Vec<_> = row.split('\t').collect();
        let [sw, sh, w, h]: [u32; 4] = std::array::from_fn(|i| f[i + 1].parse().unwrap());
        let make = |w, h| Raster {
            width: w,
            height: h,
            rgba: pattern(w, h).into_raw(),
        };
        let src = make(sw, sh);
        let actual = match f[0] {
            "nearest" => transforms::nearest(&src, w, h, false, || Ok(())).unwrap(),
            "lanczos3" => transforms::lanczos3(&src, w, h, || false).unwrap(),
            "overlay-negative" => {
                let mut canvas = src;
                transforms::overlay(&mut canvas, &make(7, 5), -2, 3).unwrap();
                canvas
            }
            // Crop is caller-owned, not a raster transform API.
            "crop-1-2" => continue,
            other => panic!("unknown transform {other}"),
        };
        assert_eq!((actual.width, actual.height), (w, h));
        assert_eq!(actual.rgba, unhex(f[5]), "{}", f[0]);
    }
}

fn check(backend: Backend, root: &Path) {
    #[cfg(feature = "candidate-png")]
    if matches!(backend, Backend::Candidate) {
        compare_transforms(root);
    }
    let mut count = 0;
    for file in ["png.tsv", "formats.tsv", "bmp.tsv"] {
        #[cfg(all(feature = "candidate-png", not(feature = "candidate")))]
        if matches!(backend, Backend::Candidate) && file != "png.tsv" {
            continue;
        }
        for row in std::fs::read_to_string(root.join(file)).unwrap().lines() {
            let fields: Vec<_> = row.split('\t').collect();
            let name = fields[0];
            let w = fields[1].parse().unwrap();
            let h = fields[2].parse().unwrap();
            let data = unhex(fields[3]);
            let expected = unhex(fields[4]);
            let actual = backend
                .decode(&data, w, h, 1 << 20)
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!((actual.0, actual.1), (w, h), "{name}");
            assert_eq!(actual.2, expected, "{name}");
            for (mw, mh, cap) in [(w - 1, h, 1 << 20), (w, h - 1, 1 << 20), (w, h, 1)] {
                assert!(
                    backend.decode(&data, mw, mh, cap).is_err(),
                    "admission {name}"
                );
            }
            assert!(
                backend.decode(&data[..12], w, h, 1 << 20).is_err(),
                "truncated {name}"
            );
            #[cfg(feature = "candidate-png")]
            if matches!(backend, Backend::Candidate) {
                if file == "png.tsv" {
                    let encoded = bmux_raster::transcode_png(&data, w, h, 1 << 20).unwrap();
                    let before = image::load_from_memory(&data).unwrap();
                    let after = image::load_from_memory(&encoded).unwrap();
                    assert_eq!(after.color(), before.color(), "native color {name}");
                    assert_eq!(after.as_bytes(), before.as_bytes(), "native samples {name}");
                }
                assert_eq!(
                    bmux_raster::validate_png(&data, w, h, 1 << 20).is_ok(),
                    file == "png.tsv",
                    "PNG admission {name}"
                );
                assert!(bmux_raster::validate_png(&data, w + 1, h, 1 << 20).is_err());
            }
            count += 1;
        }
    }
    for row in std::fs::read_to_string(root.join("invalid.tsv"))
        .unwrap()
        .lines()
    {
        let (name, s) = row.split_once('\t').unwrap();
        let data = unhex(s);
        assert!(
            backend.decode(&data, 1024, 1024, 1 << 20).is_err(),
            "{name}"
        );
        #[cfg(feature = "candidate-png")]
        if matches!(backend, Backend::Candidate) {
            assert!(
                bmux_raster::validate_png(&data, 9, 7, 1 << 20).is_err(),
                "{name}"
            );
        }
    }
    assert_eq!(
        std::fs::read_to_string(root.join("transforms.tsv")).unwrap(),
        transform_results()
    );
    compact(backend);
    println!("verified {count} valid images plus malformed, compaction and reference transforms");
}

fn noise(len: usize) -> Vec<u8> {
    let mut state = 0x927aeb19u32;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect()
}

fn compact(backend: Backend) {
    // Projection: attempt iff raw >= 256 KiB; retain iff strictly smaller.
    for (w, h, c, random, retain) in [
        (255, 257, 4, false, false),
        (256, 256, 4, false, true),
        (256, 256, 4, true, false),
        (512, 171, 3, false, true),
        (1, 1, 4, false, false),
    ] {
        let n = (w * h) as usize * usize::from(c);
        let raw = if random { noise(n) } else { vec![51; n] };
        let png = backend.encode(w, h, c, &raw).unwrap();
        assert_eq!(n >= 256 * 1024 && png.len() < n, retain);
        let rgba: Vec<_> = raw
            .chunks_exact(c.into())
            .flat_map(|p| [p[0], p[1], p[2], *p.get(3).unwrap_or(&255)])
            .collect();
        // Independent cross-decoder verification: do not rely on a candidate round-trip.
        assert_eq!(reference_decode(&png, w, h, 8 << 20).unwrap().2, rgba);
    }
}

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let backend = match args.get(1).map(String::as_str) {
        None | Some("reference") => Backend::Reference,
        #[cfg(feature = "candidate-png")]
        Some("candidate") => Backend::Candidate,
        _ => panic!("unknown or unavailable backend"),
    };
    match args.first().map(String::as_str) {
        Some("freeze") => {
            std::fs::write(root.join("formats.tsv"), format_results()).unwrap();
            std::fs::write(root.join("transforms.tsv"), transform_results()).unwrap();
        }
        None | Some("check") => check(backend, root),
        Some("perf") => {
            let work = args.get(2).expect("flat or noise");
            let (width, height) = (3840, 2160);
            let n = (width * height * 4) as usize;
            let raw = match work.as_str() {
                "flat" => vec![39; n],
                "noise" => noise(n),
                _ => panic!("flat or noise"),
            };
            let start = std::time::Instant::now();
            let png = backend.encode(width, height, 4, &raw).unwrap();
            let encoded = start.elapsed();
            let start = std::time::Instant::now();
            assert_eq!(
                backend.decode(&png, width, height, 128 << 20).unwrap().2,
                raw
            );
            println!(
                "{work}: raw={n} png={} encode_ms={} decode_ms={}",
                png.len(),
                encoded.as_millis(),
                start.elapsed().as_millis()
            );
        }
        _ => panic!("usage: freeze | check [backend] | perf backend flat|noise"),
    }
}
