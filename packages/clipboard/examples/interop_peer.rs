//! Copy this file alone into the pinned shipping checkout's clipboard examples.
//! Uses only the public API shared by shipping and candidate; no native-only cfgs.
use bmux_clipboard::{ClipboardPayload, copy_png_image, copy_text, read_payload};
use std::io::{Read, Write};
use std::process::ExitCode;

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
    let rgba: Vec<u8> = (0..37_u32 * 29)
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
                let png = bmux_raster::encode_png(37, 29, 4, &rgba).map_err(|_| "encode failed")?;
                copy_png_image(&png).map_err(|_| "image write failed")?;
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
                ClipboardPayload::ImagePng(png) if case == "image" => {
                    if !png.starts_with(b"\x89PNG\r\n\x1a\n") {
                        return Err("invalid PNG");
                    }
                    let actual = bmux_raster::decode(&png, 37, 29, 512 * 1024 * 1024)
                        .map_err(|_| "invalid PNG")?;
                    if (actual.width, actual.height) != (37, 29) || actual.rgba != rgba {
                        return Err("RGBA mismatch");
                    }
                    actual.rgba
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
