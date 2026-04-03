//! Image compositor — renders images as a post-render overlay.
//!
//! Called after the cell-based `render_attach_scene` and before cursor
//! state application.  Translates pane-local image coordinates to host
//! terminal coordinates, clips to pane boundaries, and emits the
//! appropriate protocol-specific escape sequences.

use std::io::Write;

use crate::config::ImageDecodeMode;
use crate::host_caps::HostImageCapabilities;
use crate::model::PaneImage;

/// Rectangle describing a pane's position on the host terminal.
#[derive(Clone, Copy, Debug)]
pub struct PaneRect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

/// Render images for a single pane as an overlay on the host terminal.
///
/// Images are emitted after the cell content has been drawn.  The cursor
/// is assumed to be hidden (Fix 1) and the frame is inside a synchronized
/// update (Fix 2).
pub fn render_pane_images(
    out: &mut impl Write,
    images: &[PaneImage],
    pane_rect: PaneRect,
    host_caps: &HostImageCapabilities,
    decode_mode: ImageDecodeMode,
) -> std::io::Result<()> {
    if images.is_empty() {
        return Ok(());
    }

    let inner_x = pane_rect.x.saturating_add(1);
    let inner_y = pane_rect.y.saturating_add(1);
    let inner_w = pane_rect.w.saturating_sub(2);
    let inner_h = pane_rect.h.saturating_sub(2);

    for image in images {
        // Skip images entirely outside the pane inner area.
        if image.position.row >= inner_h || image.position.col >= inner_w {
            continue;
        }

        // Skip images that extend beyond pane boundaries for now.
        // TODO(phase 7): implement proper cropping instead of skipping.
        let img_bottom = image.position.row.saturating_add(image.cell_size.rows);
        let img_right = image.position.col.saturating_add(image.cell_size.cols);
        if img_bottom > inner_h || img_right > inner_w {
            // Image partially outside — skip for now.
            continue;
        }

        let host_x = inner_x.saturating_add(image.position.col);
        let host_y = inner_y.saturating_add(image.position.row);

        match decode_mode {
            ImageDecodeMode::Passthrough => {
                emit_passthrough(out, image, host_x, host_y, host_caps)?;
            }
            ImageDecodeMode::Server => {
                emit_from_pixels(out, image, host_x, host_y, host_caps)?;
            }
            ImageDecodeMode::Client => {
                emit_client_decode(out, image, host_x, host_y, host_caps)?;
            }
        }
    }

    Ok(())
}

/// Passthrough mode: re-emit raw protocol bytes at translated coordinates.
fn emit_passthrough(
    out: &mut impl Write,
    image: &PaneImage,
    host_x: u16,
    host_y: u16,
    _host_caps: &HostImageCapabilities,
) -> std::io::Result<()> {
    let Some(raw) = &image.payload.raw else {
        return Ok(());
    };

    // Move cursor to the image position.
    write!(out, "\x1b[{};{}H", host_y + 1, host_x + 1)?;

    match image.protocol {
        #[cfg(feature = "sixel")]
        crate::model::ImageProtocol::Sixel => {
            // Re-emit the sixel DCS sequence.
            out.write_all(b"\x1bPq")?;
            out.write_all(raw)?;
            out.write_all(b"\x1b\\")?;
        }
        #[cfg(feature = "kitty")]
        crate::model::ImageProtocol::KittyGraphics => {
            // TODO: Re-emit kitty placement with translated coordinates.
            let _ = raw;
        }
        #[cfg(feature = "iterm2")]
        crate::model::ImageProtocol::ITerm2 => {
            // Re-emit iTerm2 OSC 1337.
            out.write_all(b"\x1b]1337;File=")?;
            out.write_all(raw)?;
            out.write_all(b"\x07")?;
        }
        #[allow(unreachable_patterns)]
        _ => {}
    }

    Ok(())
}

/// Server-decode mode: encode decoded pixels for the host's preferred protocol.
fn emit_from_pixels(
    out: &mut impl Write,
    image: &PaneImage,
    host_x: u16,
    host_y: u16,
    host_caps: &HostImageCapabilities,
) -> std::io::Result<()> {
    let Some(pixels) = &image.payload.pixels else {
        // No decoded pixels available — fall back to passthrough if raw exists.
        return emit_passthrough(out, image, host_x, host_y, host_caps);
    };

    write!(out, "\x1b[{};{}H", host_y + 1, host_x + 1)?;

    let _preferred = host_caps.preferred_protocol();

    // TODO: encode pixels into the host's preferred protocol.
    // For now, this is a stub that will be filled in per-protocol.
    let _ = pixels;

    Ok(())
}

/// Client-decode mode: decode raw bytes, then encode for host protocol.
fn emit_client_decode(
    out: &mut impl Write,
    image: &PaneImage,
    host_x: u16,
    host_y: u16,
    host_caps: &HostImageCapabilities,
) -> std::io::Result<()> {
    // TODO: decode raw bytes to pixels, then delegate to emit_from_pixels.
    // For now, fall back to passthrough.
    emit_passthrough(out, image, host_x, host_y, host_caps)
}
