//! Three source protocols in one alternate-screen demo.
//! Run with `--features image-kitty,image-sixel,image-iterm2,crossterm`.

use std::io::{self, Write};
use std::time::Duration;

use bmux_keyboard::KeyCode;
use bmux_tui::crossterm::{CrosstermTerminalGuard, poll_event, terminal_size};
use bmux_tui::event::Event;

// Complete 2x2 RGBA checkerboard PNG for OSC 1337.
const PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 2, 0, 0, 0, 2, 8, 6, 0,
    0, 0, 114, 182, 13, 36, 0, 0, 0, 18, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 0, 66, 64,
    12, 36, 64, 44, 0, 67, 206, 7, 249, 19, 116, 7, 229, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96,
    130,
];
const RGBA: &[u8] = &[
    255, 0, 0, 255, 0, 0, 255, 255, 0, 0, 255, 255, 255, 0, 0, 255,
];

fn delete_kitty(out: &mut impl Write, id: u32) -> io::Result<()> {
    out.write_all(b"\x1b_")?;
    out.write_all(&bmux_image::codec::kitty::encode_delete_image(id))?;
    out.write_all(b"\x1b\\")
}

fn render(out: &mut impl Write, id: u32, sixel: &[u8]) -> io::Result<()> {
    let size = terminal_size()?;
    out.write_all(b"\x1b[H\x1b[?25l")?;
    // Paint the background explicitly, keeping the protocol probes separate
    // from erase-display commands that may discard terminal image resources.
    for row in 1..=size.height {
        write!(out, "\x1b[{row};1H{}", " ".repeat(usize::from(size.width)))?;
    }
    out.write_all(b"\x1b[H")?;
    if size.width < 60 || size.height < 14 {
        out.write_all(b"Resize to at least 60x14 (q to quit)")?;
        return out.flush();
    }
    out.write_all(b"Three image protocols - q / Esc / Ctrl-C to quit")?;
    out.write_all(b"\x1b[3;2HKitty\x1b[3;22HSixel\x1b[3;42HiTerm2")?;
    out.write_all(b"\x1b[5;2H\x1b_")?;
    out.write_all(&bmux_image::codec::kitty::encode_transmit(
        id,
        bmux_image::KittyFormat::Rgba,
        RGBA,
        2,
        2,
    ))?;
    write!(out, "\x1b\\\x1b_Ga=p,i={id},p={id},c=12,r=6,C=1,q=2;\x1b\\")?;
    out.write_all(b"\x1b[5;22H\x1bPq")?;
    out.write_all(sixel)?;
    out.write_all(b"\x1b\\\x1b[5;42H\x1b]1337;File=")?;
    out.write_all(&bmux_image::codec::iterm2::encode_body_with_cells(
        PNG, 12, 6,
    ))?;
    out.write_all(b"\x07\x1b[13;1HSource protocols; BMUX translates to the host protocol.")?;
    out.flush()
}

fn main() -> io::Result<()> {
    // Native Sixel has pixel dimensions, unlike the cell-sized other images.
    let mut data = Vec::with_capacity(64 * 48 * 4);
    for y in 0..48 {
        for x in 0..64 {
            data.extend_from_slice(if (x < 32) == (y < 24) {
                &RGBA[..4]
            } else {
                &RGBA[4..8]
            });
        }
    }
    let sixel = bmux_image::codec::sixel::encode(&bmux_image::PixelBuffer {
        data,
        width: 64,
        height: 48,
        format: bmux_image::PixelFormat::Rgba8,
    })
    .ok_or_else(|| io::Error::other("could not encode Sixel checkerboard"))?;
    let id = std::process::id().max(1);
    let mut guard = CrosstermTerminalGuard::enter(io::stdout())?;
    let result = (|| {
        let out = guard.writer_mut().expect("guard owns stdout");
        render(out, id, &sixel)?;
        loop {
            match poll_event(Duration::from_millis(100))? {
                Some(Event::Key(key))
                    if matches!(key.key, KeyCode::Escape | KeyCode::Char('q'))
                        || (key.key == KeyCode::Char('c') && key.modifiers.ctrl) =>
                {
                    break;
                }
                Some(Event::Resize(_)) => render(out, id, &sixel)?,
                _ => {}
            }
        }
        Ok(())
    })();
    let cleanup = (|| {
        let out = guard.writer_mut().expect("guard owns stdout");
        delete_kitty(out, id)?;
        out.write_all(b"\x1b[2J\x1b[?25h")?;
        out.flush()
    })();
    let restoration = guard.leave();
    restoration?;
    cleanup?;
    result
}
