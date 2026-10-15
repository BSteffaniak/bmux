//! Bounded structured copies of committed BMUX scenes; never ANSI or executable code.
use crate::HostError;
use bmux_tui::{
    frame::Frame,
    image::{ImagePayload, ImagePixelFormat},
    image_scene::ImageScene,
    style::{Color, Modifier},
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireColor {
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WireCell {
    pub symbol: String,
    pub width: u8,
    pub continuation: bool,
    pub fg: Option<WireColor>,
    pub bg: Option<WireColor>,
    pub modifiers: u8,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanvasRect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum WireImagePayload {
    Png {
        bytes: Vec<u8>,
        width: u32,
        height: u32,
    },
    Pixels {
        bytes: Vec<u8>,
        width: u32,
        height: u32,
        rgba: bool,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WireImage {
    pub key: String,
    pub destination: CanvasRect,
    #[serde(default)]
    pub fit: bmux_tui::image::ImageFit,
    #[serde(default)]
    pub allocation: Option<bmux_tui::image::ImageBounds>,
    pub clip: CanvasRect,
    pub payload: WireImagePayload,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MirrorInteraction {
    pub id: String,
    pub input: String,
    pub area: CanvasRect,
    pub enabled: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MirrorPrivateScene {
    pub frame: Box<MirrorFrame>,
    pub interactions: Vec<MirrorInteraction>,
    pub identity: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MirrorFrame {
    pub surface_id: String,
    pub slide_id: String,
    pub revision: u64,
    pub width: u16,
    pub height: u16,
    pub cells: Vec<WireCell>,
    pub images: Vec<WireImage>,
    /// Audience-safe authored terminal and chrome; native rendering uses fallback cells.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playbook: Option<bmux_presenter::audience::AudiencePlaybook>,
    /// Retained target revision, independent of output-only damage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interaction_revision: Option<u64>,
    /// Authenticated operator projection; never included in audience snapshots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_scene: Option<MirrorPrivateScene>,
}

fn color(value: Color) -> WireColor {
    match value {
        Color::Default => WireColor::Default,
        Color::Indexed(index) => WireColor::Indexed(index),
        Color::Rgb(r, g, b) => WireColor::Rgb(r, g, b),
        Color::Black => WireColor::Indexed(0),
        Color::Red => WireColor::Indexed(1),
        Color::Green => WireColor::Indexed(2),
        Color::Yellow => WireColor::Indexed(3),
        Color::Blue => WireColor::Indexed(4),
        Color::Magenta => WireColor::Indexed(5),
        Color::Cyan => WireColor::Indexed(6),
        Color::White => WireColor::Indexed(7),
        Color::BrightBlack => WireColor::Indexed(8),
        Color::BrightRed => WireColor::Indexed(9),
        Color::BrightGreen => WireColor::Indexed(10),
        Color::BrightYellow => WireColor::Indexed(11),
        Color::BrightBlue => WireColor::Indexed(12),
        Color::BrightMagenta => WireColor::Indexed(13),
        Color::BrightCyan => WireColor::Indexed(14),
        Color::BrightWhite => WireColor::Indexed(15),
    }
}
fn rect(value: bmux_tui::geometry::Rect) -> CanvasRect {
    CanvasRect {
        x: value.x,
        y: value.y,
        width: value.width,
        height: value.height,
    }
}

impl MirrorFrame {
    /// Capture only after the owner commits presentation. The reconciled image scene
    /// preserves persistent-image lifecycle; a frame-only image list is insufficient.
    pub fn capture(
        surface_id: String,
        slide_id: String,
        revision: u64,
        frame: &Frame<'_>,
        images: &ImageScene,
        max_cells: usize,
        max_bytes: usize,
    ) -> Result<Self, HostError> {
        let area = frame.area();
        if area.x != 0 || area.y != 0 || frame.buffer().cells().len() > max_cells {
            return Err(HostError::Limit);
        }
        if frame
            .buffer()
            .cells()
            .iter()
            .any(|cell| cell.symbol.len() > 64)
        {
            return Err(HostError::Limit);
        }
        let modifiers = [
            Modifier::BOLD,
            Modifier::DIM,
            Modifier::ITALIC,
            Modifier::UNDERLINE,
            Modifier::SLOW_BLINK,
            Modifier::REVERSED,
            Modifier::HIDDEN,
            Modifier::CROSSED_OUT,
        ];
        let cells = frame
            .buffer()
            .cells()
            .iter()
            .map(|cell| WireCell {
                symbol: cell.symbol.clone(),
                width: cell.width(),
                continuation: cell.is_wide_continuation(),
                fg: cell.style.fg.map(color),
                bg: cell.style.bg.map(color),
                modifiers: modifiers.iter().enumerate().fold(0, |bits, (index, flag)| {
                    bits | if cell.style.modifiers.contains(*flag) {
                        1 << index
                    } else {
                        0
                    }
                }),
            })
            .collect();
        let mut wire_images = Vec::new();
        let mut bytes = 0_usize;
        for placement in images.placements() {
            let (payload_bytes, payload) = match &placement.payload {
                ImagePayload::Png {
                    bytes,
                    width,
                    height,
                } => (
                    bytes,
                    WireImagePayload::Png {
                        bytes: Vec::new(),
                        width: *width,
                        height: *height,
                    },
                ),
                ImagePayload::Pixels {
                    bytes,
                    width,
                    height,
                    format,
                } => (
                    bytes,
                    WireImagePayload::Pixels {
                        bytes: Vec::new(),
                        width: *width,
                        height: *height,
                        rgba: *format == ImagePixelFormat::Rgba8,
                    },
                ),
            };
            bytes = bytes
                .checked_add(payload_bytes.len())
                .ok_or(HostError::Limit)?;
            if bytes > max_bytes || wire_images.len() >= max_cells {
                return Err(HostError::Limit);
            }
            let payload = match payload {
                WireImagePayload::Png { width, height, .. } => WireImagePayload::Png {
                    bytes: payload_bytes.clone(),
                    width,
                    height,
                },
                WireImagePayload::Pixels {
                    width,
                    height,
                    rgba,
                    ..
                } => WireImagePayload::Pixels {
                    bytes: payload_bytes.clone(),
                    width,
                    height,
                    rgba,
                },
            };
            wire_images.push(WireImage {
                key: placement.key.as_str().to_owned(),
                payload,
                destination: rect(placement.destination),
                fit: placement.fit,
                allocation: placement.allocation,
                clip: rect(placement.clip.intersection(area)),
            });
        }
        let result = Self {
            surface_id,
            slide_id,
            revision,
            width: area.width,
            height: area.height,
            cells,
            images: wire_images,
            playbook: None,
            interaction_revision: None,
            private_scene: None,
        };
        result.validate(max_cells, max_bytes)?;
        Ok(result)
    }

    pub fn validate(&self, max_cells: usize, max_bytes: usize) -> Result<(), HostError> {
        self.validate_wire(max_cells, max_bytes, true)
    }

    fn validate_wire(
        &self,
        max_cells: usize,
        max_bytes: usize,
        decode_png: bool,
    ) -> Result<(), HostError> {
        if let Some(scene) = &self.private_scene {
            if scene.frame.private_scene.is_some()
                || scene.frame.playbook.is_some()
                || scene.interactions.len() > 64
            {
                return Err(HostError::Limit);
            }
            scene
                .frame
                .validate_wire(max_cells, max_bytes, decode_png)?;
            if serde_json::to_vec(&scene.identity)
                .map_err(|_| HostError::Invalid)?
                .len()
                > 1024
            {
                return Err(HostError::Limit);
            }
            for interaction in &scene.interactions {
                let area = interaction.area;
                if interaction.id.len() > 256
                    || interaction.input.len() > 128
                    || area.width == 0
                    || area.height == 0
                    || u32::from(area.x) + u32::from(area.width) > u32::from(scene.frame.width)
                    || u32::from(area.y) + u32::from(area.height) > u32::from(scene.frame.height)
                {
                    return Err(HostError::Invalid);
                }
            }
        }
        let count = usize::from(self.width) * usize::from(self.height);
        if let Some(playbook) = &self.playbook {
            playbook.validate().map_err(|_| HostError::Invalid)?;
            let terminal = &playbook.terminal;
            // Optional public data must not smuggle authenticated source geometry,
            // selection text or operator control identities into audience snapshots.
            if !terminal.surfaces().is_empty()
                || !terminal.hits().regions().is_empty()
                || terminal.focus_scope().is_some()
                || !terminal.selection_text().is_empty()
                || !terminal.selection().fragments().is_empty()
                || !terminal.semantics().regions().is_empty()
                || terminal.focus().active().is_some()
            {
                return Err(HostError::Invalid);
            }
            if count
                .checked_add(terminal.buffer().cells().len())
                .is_none_or(|total| total > max_cells)
                || self
                    .images
                    .len()
                    .checked_add(terminal.image_scene().placements().count())
                    .is_none_or(|total| total > max_cells)
            {
                return Err(HostError::Limit);
            }
        }
        if count == 0
            || count > max_cells
            || self.cells.len() != count
            || self.images.len() > max_cells
            || self.surface_id.is_empty()
            || self.surface_id.len() > 128
            || self.slide_id.is_empty()
            || self.slide_id.len() > 128
            || self.surface_id.chars().any(char::is_control)
            || self.slide_id.chars().any(char::is_control)
        {
            return Err(HostError::Limit);
        }
        if self.cells.iter().any(|cell| {
            cell.symbol.len() > 64 || cell.symbol.chars().any(char::is_control) || cell.width > 2
        }) {
            return Err(HostError::Invalid);
        }
        validate_cells(self)?;
        validate_images(self, max_bytes, decode_png)?;
        if serde_json::to_vec(self)
            .map_err(|_| HostError::Invalid)?
            .len()
            > max_bytes
        {
            return Err(HostError::Limit);
        }
        Ok(())
    }

    /// Center a fixed canvas without reflow. Larger canvases clip centrally.
    pub fn placement(&self, viewport_width: u16, viewport_height: u16) -> CanvasPlacement {
        CanvasPlacement::centered(self.width, self.height, viewport_width, viewport_height)
    }
}

pub use bmux_tui::embedded_surface::SurfacePlacement as CanvasPlacement;

fn validate_cells(frame: &MirrorFrame) -> Result<(), HostError> {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    for row in frame.cells.chunks_exact(usize::from(frame.width)) {
        for (index, cell) in row.iter().enumerate() {
            if cell.continuation {
                if cell.width != 0
                    || !cell.symbol.is_empty()
                    || index == 0
                    || row[index - 1].width != 2
                    || row[index - 1].continuation
                    || cell.fg != row[index - 1].fg
                    || cell.bg != row[index - 1].bg
                    || cell.modifiers != row[index - 1].modifiers
                {
                    return Err(HostError::Invalid);
                }
            } else {
                if cell.symbol.graphemes(true).count() != 1
                    || cell.symbol.width() != usize::from(cell.width)
                    || !(1..=2).contains(&cell.width)
                {
                    return Err(HostError::Invalid);
                }
                if cell.width == 2 && row.get(index + 1).is_none_or(|next| !next.continuation) {
                    return Err(HostError::Invalid);
                }
            }
        }
    }
    Ok(())
}
fn validate_images(
    frame: &MirrorFrame,
    max_bytes: usize,
    decode_png: bool,
) -> Result<(), HostError> {
    let mut keys = std::collections::BTreeSet::new();
    let mut total = 0_usize;
    for image in &frame.images {
        if image.key.is_empty()
            || image.key.len() > 128
            || image.key.chars().any(char::is_control)
            || !keys.insert(&image.key)
            || !within(image.destination, frame.width, frame.height)
            || !within(image.clip, frame.width, frame.height)
            || image.allocation.is_some_and(|bounds| {
                !bounds.covers(bmux_tui::geometry::Rect::new(
                    image.destination.x,
                    image.destination.y,
                    image.destination.width,
                    image.destination.height,
                ))
            })
        {
            return Err(HostError::Invalid);
        }
    }
    let fallback = frame.images.iter().map(|image| match &image.payload {
        WireImagePayload::Png {
            bytes,
            width,
            height,
        } => (bytes, *width, *height, 4_usize, true),
        WireImagePayload::Pixels {
            bytes,
            width,
            height,
            rgba,
        } => (bytes, *width, *height, if *rgba { 4 } else { 3 }, false),
    });
    let terminal = frame.playbook.iter().flat_map(|playbook| {
        playbook
            .terminal
            .image_scene()
            .placements()
            .map(|image| match &image.payload {
                ImagePayload::Png {
                    bytes,
                    width,
                    height,
                } => (bytes, *width, *height, 4_usize, true),
                ImagePayload::Pixels {
                    bytes,
                    width,
                    height,
                    format,
                } => (
                    bytes,
                    *width,
                    *height,
                    if *format == ImagePixelFormat::Rgba8 {
                        4
                    } else {
                        3
                    },
                    false,
                ),
            })
    });
    // Both projections share encoded-wire and decoded-image allocation budgets.
    for (bytes, width, height, channels, png) in fallback.chain(terminal) {
        if width == 0 || height == 0 || width > 4096 || height > 4096 {
            return Err(HostError::Limit);
        }
        let decoded = usize::try_from(width)
            .map_err(|_| HostError::Limit)?
            .checked_mul(usize::try_from(height).map_err(|_| HostError::Limit)?)
            .and_then(|pixels| pixels.checked_mul(channels))
            .ok_or(HostError::Limit)?;
        total = total.checked_add(decoded).ok_or(HostError::Limit)?;
        if decoded > 16 * 1024 * 1024 || total > 32 * 1024 * 1024 || bytes.len() > max_bytes {
            return Err(HostError::Limit);
        }
        if !png && bytes.len() != decoded {
            return Err(HostError::Invalid);
        }
        if png && decode_png {
            let mut reader = image::ImageReader::with_format(
                std::io::Cursor::new(bytes),
                image::ImageFormat::Png,
            );
            let mut limits = image::Limits::default();
            limits.max_image_width = Some(width);
            limits.max_image_height = Some(height);
            limits.max_alloc = Some(32 * 1024 * 1024);
            reader.limits(limits);
            let decoded = reader.decode().map_err(|_| HostError::Invalid)?;
            if decoded.width() != width || decoded.height() != height {
                return Err(HostError::Invalid);
            }
        }
    }
    Ok(())
}
fn within(rect: CanvasRect, width: u16, height: u16) -> bool {
    rect.width != 0
        && rect.height != 0
        && u32::from(rect.x) + u32::from(rect.width) <= u32::from(width)
        && u32::from(rect.y) + u32::from(rect.height) <= u32::from(height)
}

fn native_color(color: &WireColor) -> Color {
    match *color {
        WireColor::Default => Color::Default,
        WireColor::Indexed(index) => Color::Indexed(index),
        WireColor::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}
fn native_style(cell: &WireCell) -> bmux_tui::style::Style {
    let flags = [
        Modifier::BOLD,
        Modifier::DIM,
        Modifier::ITALIC,
        Modifier::UNDERLINE,
        Modifier::SLOW_BLINK,
        Modifier::REVERSED,
        Modifier::HIDDEN,
        Modifier::CROSSED_OUT,
    ];
    let modifiers = flags
        .iter()
        .enumerate()
        .fold(Modifier::EMPTY, |bits, (index, flag)| {
            if cell.modifiers & (1 << index) != 0 {
                bits.union(*flag)
            } else {
                bits
            }
        });
    bmux_tui::style::Style {
        fg: cell.fg.as_ref().map(native_color),
        bg: cell.bg.as_ref().map(native_color),
        modifiers,
    }
}

impl MirrorFrame {
    /// Decode the legacy product wire projection once into neutral retained primitives.
    /// Generation is supplied by the owning transport; no authority is inferred from
    /// a slide label or installed into the compositor by this adapter.
    ///
    /// # Errors
    /// Returns invalid wire data or resource admission failures unchanged.
    pub fn retained_surface(
        &self,
        generation: u64,
        max_cells: usize,
        max_bytes: usize,
    ) -> Result<bmux_tui::embedded_surface::RetainedSurface, HostError> {
        use bmux_tui::{
            buffer::Buffer,
            embedded_surface::{RetainedSurface, SurfaceError, SurfaceIdentity, SurfaceLimits},
            geometry::{Point, Rect},
            image::{ImageContribution, ImageKey, ImageLifecycle, ImagePlacement},
        };
        use std::sync::Arc;
        // Validate the actual wire topology before Buffer's safe setters normalize it.
        // Native snapshot admission performs bounded PNG decoding exactly once.
        self.validate_wire(max_cells, max_bytes, false)?;
        let mut buffer = Buffer::empty(Rect::new(0, 0, self.width, self.height));
        for (index, cell) in self.cells.iter().enumerate() {
            if cell.continuation {
                continue;
            }
            let x = u16::try_from(index % usize::from(self.width)).map_err(|_| HostError::Limit)?;
            let y = u16::try_from(index / usize::from(self.width)).map_err(|_| HostError::Limit)?;
            buffer.set_cell(Point::new(x, y), &cell.symbol, native_style(cell));
        }
        let contributions = self
            .images
            .iter()
            .map(|image| {
                let payload = match &image.payload {
                    WireImagePayload::Png {
                        bytes,
                        width,
                        height,
                    } => ImagePayload::Png {
                        bytes: bytes.clone(),
                        width: *width,
                        height: *height,
                    },
                    WireImagePayload::Pixels {
                        bytes,
                        width,
                        height,
                        rgba,
                    } => ImagePayload::Pixels {
                        bytes: bytes.clone(),
                        width: *width,
                        height: *height,
                        format: if *rgba {
                            ImagePixelFormat::Rgba8
                        } else {
                            ImagePixelFormat::Rgb8
                        },
                    },
                };
                let destination = image.destination;
                let clip = image.clip;
                ImageContribution::Present(ImagePlacement {
                    key: ImageKey::new(image.key.clone()),
                    payload,
                    destination: Rect::new(
                        destination.x,
                        destination.y,
                        destination.width,
                        destination.height,
                    ),
                    fit: image.fit,
                    allocation: image.allocation,
                    clip: Rect::new(clip.x, clip.y, clip.width, clip.height),
                    lifecycle: ImageLifecycle::Frame,
                })
            })
            .collect::<Vec<_>>();
        let mut images = ImageScene::default();
        images.reconcile(&contributions);
        RetainedSurface::new(
            SurfaceIdentity {
                owner: self.surface_id.clone(),
                generation,
                revision: self.revision,
            },
            Arc::new(buffer),
            Arc::new(images),
            SurfaceLimits {
                max_cells,
                max_bytes,
            },
        )
        .map_err(|error| match error {
            SurfaceError::Invalid => HostError::Invalid,
            SurfaceError::Limit => HostError::Limit,
        })
    }

    /// Paint a validated mirror using BMUX's scoped painting and image scene APIs.
    /// The caller publishes placement/input geometry only after terminal commit.
    pub fn paint(
        &self,
        cx: &mut bmux_tui::paint::PaintCx<'_, '_>,
        max_cells: usize,
        max_bytes: usize,
    ) -> Result<CanvasPlacement, HostError> {
        let area = cx.area();
        self.paint_viewport(cx, area.width, area.height, max_cells, max_bytes)
    }
    pub(crate) fn paint_viewport(
        &self,
        cx: &mut bmux_tui::paint::PaintCx<'_, '_>,
        width: u16,
        height: u16,
        max_cells: usize,
        max_bytes: usize,
    ) -> Result<CanvasPlacement, HostError> {
        let surface = self.retained_surface(0, max_cells, max_bytes)?;
        Ok(surface.paint_viewport(cx, width, height))
    }
}
