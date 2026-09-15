//! Print a Sixel checkerboard inline without an alternate screen or input loop.
//! `cargo run -p bmux_tui_runtime --example image_sixel --features image-sixel`

use std::io::{self, Write};

fn main() -> io::Result<()> {
    // Sixel dimensions are pixels, not terminal cells. This remains visible
    // without relying on a particular terminal's font metrics.
    let (width, height) = (128, 96);
    let mut data = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        for x in 0..width {
            data.extend_from_slice(if (x < width / 2) == (y < height / 2) {
                &[255, 0, 0, 255]
            } else {
                &[0, 0, 255, 255]
            });
        }
    }
    let image = bmux_image::PixelBuffer {
        data,
        width: 128,
        height: 96,
        format: bmux_image::PixelFormat::Rgba8,
    };
    let encoded = bmux_image::codec::sixel::encode(&image)
        .ok_or_else(|| io::Error::other("could not encode checkerboard"))?;
    let mut out = io::stdout().lock();
    writeln!(out, "BMUX Sixel inline image (128x96 pixels)")?;
    // Reserve rows before drawing so an image at the bottom cannot overwrite
    // existing output. Save/restore avoids relying on Sixel cursor semantics.
    for _ in 0..12 {
        writeln!(out)?;
    }
    out.write_all(b"\r\x1b7\x1b[12A\x1bPq")?;
    out.write_all(&encoded)?;
    out.write_all(b"\x1b\\\x1b8")?;
    out.flush()
}
