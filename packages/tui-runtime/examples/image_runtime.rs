//! Print a Kitty image inline, leaving it in the terminal after exit.
//!
//! `cargo run -p bmux_tui_runtime --example image_runtime --features image-kitty`
//! No alternate screen, raw mode, input reader, or image deletion is needed.

use std::io::{self, Write};

fn main() -> io::Result<()> {
    let mut out = io::stdout().lock();
    let id = std::process::id().max(1);
    let pixels = [
        255, 0, 0, 255, 0, 0, 255, 255, 0, 0, 255, 255, 255, 0, 0, 255,
    ];
    writeln!(out, "BMUX inline image")?;
    // Reserve space first (including at the bottom of the screen), then return
    // to its top using relative movement. Never overwrite earlier shell output.
    for _ in 0..8 {
        writeln!(out)?;
    }
    write!(out, "\r\x1b[8A")?;
    for chunk in bmux_image::codec::kitty::encode_transmit_chunks(
        id,
        bmux_image::KittyFormat::Rgba,
        &pixels,
        2,
        2,
    ) {
        out.write_all(b"\x1b_")?;
        out.write_all(&chunk)?;
        out.write_all(b"\x1b\\")?;
    }
    // C=1 keeps the cursor at the placement origin; advance below it ourselves.
    write!(
        out,
        "\x1b_Ga=p,i={id},p={id},c=16,r=8,C=1,q=2;\x1b\\\x1b[8B\r"
    )?;
    out.flush()
}
