//! Print an iTerm2 checkerboard inline without an alternate screen or input loop.
//! `cargo run -p bmux_tui_runtime --example image_iterm2`

use std::io::{self, Write};

// A complete 2x2 RGBA PNG: red/blue on the first row, blue/red on the second.
const CHECKERBOARD_PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 2, 0, 0, 0, 2, 8, 6, 0,
    0, 0, 114, 182, 13, 36, 0, 0, 0, 18, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 0, 66, 64,
    12, 36, 64, 44, 0, 67, 206, 7, 249, 19, 116, 7, 229, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96,
    130,
];

fn main() -> io::Result<()> {
    let mut out = io::stdout().lock();
    writeln!(out, "BMUX iTerm2 inline image (16x8 cells)")?;
    for _ in 0..8 {
        writeln!(out)?;
    }
    // Restore the cursor below the reserved area regardless of the terminal's
    // cursor movement after OSC 1337. Leave the image displayed after exit.
    out.write_all(b"\r\x1b7\x1b[8A\x1b]1337;File=")?;
    out.write_all(&bmux_image::codec::iterm2::encode_body_with_cells(
        CHECKERBOARD_PNG,
        16,
        8,
    ))?;
    out.write_all(b"\x07\x1b8")?;
    out.flush()
}
