//! Image compositor — renders images as a post-render overlay.
//!
//! Called after the cell-based `render_attach_scene` and before cursor
//! state application.  Translates pane-local image coordinates to host
//! terminal coordinates, clips to pane boundaries, and emits the
//! appropriate protocol-specific escape sequences.

use std::io::Write;

#[cfg(feature = "iterm2")]
use image::ImageEncoder;

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

/// Tracks kitty images already transmitted to the host terminal.
/// This enables transmit-once-place-many: only new images are transmitted,
/// previously transmitted images are re-placed without re-sending data.
#[derive(Clone, Debug, Default)]
pub struct KittyHostState {
    /// Maps bmux-internal image ID → host-side kitty image ID.
    pub transmitted: std::collections::HashMap<u64, u32>,
    /// Next host-side kitty image ID to allocate.
    #[cfg(feature = "kitty")]
    next_host_id: u32,
}

impl KittyHostState {
    /// Get or allocate a host-side kitty image ID for a bmux image.
    #[cfg(feature = "kitty")]
    fn get_or_allocate(&mut self, bmux_image_id: u64) -> (u32, bool) {
        if let Some(&host_id) = self.transmitted.get(&bmux_image_id) {
            (host_id, false) // Already transmitted.
        } else {
            self.next_host_id = self.next_host_id.wrapping_add(1);
            if self.next_host_id == 0 {
                self.next_host_id = 1; // kitty image_id 0 is invalid.
            }
            let host_id = self.next_host_id;
            self.transmitted.insert(bmux_image_id, host_id);
            (host_id, true) // Newly allocated, needs transmission.
        }
    }
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
    kitty_state: &mut KittyHostState,
) -> std::io::Result<()> {
    if images.is_empty() {
        return Ok(());
    }

    // `pane_rect` is the pane's **content rect** — the PTY interior, not
    // the outer surface bounds. Callers are responsible for passing the
    // `content_rect` from the scene so decoration thickness is handled by
    // the scene producer once, not recomputed here.
    let inner_x = pane_rect.x;
    let inner_y = pane_rect.y;
    let inner_w = pane_rect.w;
    let inner_h = pane_rect.h;

    for image in images {
        // Skip images entirely outside the pane inner area.
        if image.position.row >= inner_h || image.position.col >= inner_w {
            continue;
        }

        // Allow partially-overlapping images: the pane border will
        // visually clip the overflow.  Only skip fully-outside images.
        let host_x = inner_x.saturating_add(image.position.col);
        let host_y = inner_y.saturating_add(image.position.row);

        match decode_mode {
            ImageDecodeMode::Passthrough => {
                emit_passthrough(out, image, host_x, host_y, host_caps, kitty_state)?;
            }
            ImageDecodeMode::Server => {
                emit_from_pixels(out, image, host_x, host_y, host_caps, kitty_state)?;
            }
            ImageDecodeMode::Client => {
                emit_client_decode(out, image, host_x, host_y, host_caps, kitty_state)?;
            }
        }
    }

    Ok(())
}

/// Passthrough mode: re-emit raw protocol bytes at translated coordinates.
#[allow(unused_variables)]
fn emit_passthrough(
    out: &mut impl Write,
    image: &PaneImage,
    host_x: u16,
    host_y: u16,
    _host_caps: &HostImageCapabilities,
    kitty_state: &mut KittyHostState,
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
            // Transmit-once-place-many with globally unique host IDs.
            let (host_image_id, needs_transmit) = kitty_state.get_or_allocate(image.id);
            let placement_id = host_image_id;

            if needs_transmit {
                out.write_all(b"\x1b_")?;
                out.write_all(&crate::codec::kitty::encode_transmit(
                    host_image_id,
                    crate::model::KittyFormat::Png,
                    raw,
                    image.pixel_size.width,
                    image.pixel_size.height,
                ))?;
                out.write_all(b"\x1b\\")?;
            }

            // Always re-place at the (potentially updated) position.
            out.write_all(b"\x1b_")?;
            out.write_all(&crate::codec::kitty::encode_place(
                host_image_id,
                placement_id,
                host_y,
                host_x,
            ))?;
            out.write_all(b"\x1b\\")?;
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
#[allow(unused_variables, unreachable_code)]
fn emit_from_pixels(
    out: &mut impl Write,
    image: &PaneImage,
    host_x: u16,
    host_y: u16,
    host_caps: &HostImageCapabilities,
    kitty_state: &mut KittyHostState,
) -> std::io::Result<()> {
    let Some(pixels) = &image.payload.pixels else {
        return emit_passthrough(out, image, host_x, host_y, host_caps, kitty_state);
    };

    write!(out, "\x1b[{};{}H", host_y + 1, host_x + 1)?;

    match host_caps.preferred_protocol() {
        #[cfg(feature = "sixel")]
        Some(crate::model::ImageProtocol::Sixel) => {
            if let Some(sixel_data) = crate::codec::sixel::encode(pixels) {
                out.write_all(b"\x1bPq")?;
                out.write_all(&sixel_data)?;
                out.write_all(b"\x1b\\")?;
            }
        }
        #[cfg(feature = "kitty")]
        Some(crate::model::ImageProtocol::KittyGraphics) => {
            let (kitty_format, data) = match pixels.format {
                crate::model::PixelFormat::Rgb8 => {
                    (crate::model::KittyFormat::Rgb, pixels.data.as_slice())
                }
                crate::model::PixelFormat::Rgba8 => {
                    (crate::model::KittyFormat::Rgba, pixels.data.as_slice())
                }
                crate::model::PixelFormat::Png => {
                    (crate::model::KittyFormat::Png, pixels.data.as_slice())
                }
            };
            let (host_id, needs_transmit) = kitty_state.get_or_allocate(image.id);
            if needs_transmit {
                out.write_all(b"\x1b_")?;
                out.write_all(&crate::codec::kitty::encode_transmit(
                    host_id,
                    kitty_format,
                    data,
                    pixels.width,
                    pixels.height,
                ))?;
                out.write_all(b"\x1b\\")?;
            }
            out.write_all(b"\x1b_")?;
            out.write_all(&crate::codec::kitty::encode_place_with_z_and_cells(
                host_id,
                host_id,
                0,
                image.cell_size.cols,
                image.cell_size.rows,
            ))?;
            out.write_all(b"\x1b\\")?;
        }
        #[cfg(feature = "iterm2")]
        Some(crate::model::ImageProtocol::ITerm2) => {
            let png = pixels_as_png(pixels)?;
            out.write_all(b"\x1b]1337;File=")?;
            out.write_all(&crate::codec::iterm2::encode_body_with_cells(
                &png,
                image.cell_size.cols,
                image.cell_size.rows,
            ))?;
            out.write_all(b"\x07")?;
        }
        _ => {}
    }

    Ok(())
}

#[cfg(feature = "iterm2")]
fn pixels_as_png(pixels: &crate::model::PixelBuffer) -> std::io::Result<Vec<u8>> {
    if pixels.format == crate::model::PixelFormat::Png {
        return Ok(pixels.data.clone());
    }
    let color = match pixels.format {
        crate::model::PixelFormat::Rgb8 => image::ColorType::Rgb8,
        crate::model::PixelFormat::Rgba8 => image::ColorType::Rgba8,
        crate::model::PixelFormat::Png => unreachable!(),
    };
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(&pixels.data, pixels.width, pixels.height, color.into())
        .map_err(std::io::Error::other)?;
    Ok(png)
}

/// Client-decode mode: decode raw bytes, then encode for host protocol.
fn emit_client_decode(
    out: &mut impl Write,
    image: &PaneImage,
    host_x: u16,
    host_y: u16,
    host_caps: &HostImageCapabilities,
    kitty_state: &mut KittyHostState,
) -> std::io::Result<()> {
    // TODO: decode raw bytes to pixels, then delegate to emit_from_pixels.
    // For now, fall back to passthrough.
    emit_passthrough(out, image, host_x, host_y, host_caps, kitty_state)
}
