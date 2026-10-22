//! Retained presentation projection into physical quads, not a second layout engine.
use crate::{
    damage::DamageTracker,
    gpu::Gpu,
    images,
    text::{BitmapFormat, TextRasterizer, TextStyle},
};
use bmux_tui::{
    presentation::PreparedPresentation,
    style::{Color, Modifier},
};
use bterm_native_platform::{PhysicalSize, SurfaceTarget};
use std::collections::HashMap;

/// Window-local raster and framebuffer configuration. Sizes are physical except font_size.
#[derive(Clone, Copy, Debug)]
pub struct RendererOptions {
    pub size: PhysicalSize,
    /// Font size in logical pixels.
    pub font_size: f32,
    pub scale_factor: f32,
}
impl Default for RendererOptions {
    fn default() -> Self {
        Self {
            size: PhysicalSize {
                width: 800,
                height: 600,
            },
            font_size: 16.0,
            scale_factor: 1.0,
        }
    }
}
/// Output has been submitted and the acquired surface image presented. Not a scene commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PresentReport {
    pub generation: u64,
}
/// Explicit failures never produce a successful output report.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("native device: {0}")]
    Device(String),
    #[error("native surface: {0}")]
    Surface(String),
    #[error("native image: {0}")]
    Image(String),
    #[error("native capacity: {0}")]
    Capacity(&'static str),
    #[error("native text: {0}")]
    Text(#[from] crate::text::TextError),
    #[error("window is minimized")]
    Suspended,
}
/// Window-owned GPU resources and reconstructible raster caches. Drop releases both.
pub struct NativeRenderer {
    gpu: Gpu,
    text: TextRasterizer,
    options: RendererOptions,
    cell: [f32; 2],
    ascent: f32,
    damage: DamageTracker,
}
impl NativeRenderer {
    pub async fn new(target: SurfaceTarget, options: RendererOptions) -> Result<Self, RenderError> {
        let text = TextRasterizer::with_system_fonts()?;
        let (cell, ascent) = metrics(&text, options)?;
        let gpu = Gpu::new(target, options.size).await?;
        Ok(Self {
            gpu,
            text,
            options,
            cell,
            ascent,
            damage: DamageTracker::default(),
        })
    }
    /// Fixed physical cell metrics; execution dimensions are caller-owned.
    pub const fn cell_size(&self) -> [f32; 2] {
        self.cell
    }
    pub fn resize(&mut self, size: PhysicalSize) {
        self.options.size = size;
        self.gpu.resize(size);
        self.damage.invalidate();
    }
    pub fn set_scale_factor(&mut self, scale: f32) -> Result<(), RenderError> {
        let options = RendererOptions {
            scale_factor: scale,
            ..self.options
        };
        let (cell, ascent) = metrics(&self.text, options)?;
        self.options = options;
        self.cell = cell;
        self.ascent = ascent;
        self.damage.invalidate();
        Ok(())
    }
    /// Render complete retained output. Caller must reject stale provenance before entry
    /// and commit interaction metadata only if this method succeeds.
    pub fn present(
        &mut self,
        prepared: &PreparedPresentation,
    ) -> Result<PresentReport, RenderError> {
        if self.options.size.is_empty() {
            return Err(RenderError::Suspended);
        }
        let area = prepared.buffer().area();
        if prepared.buffer().cells().len() > 1_000_000
            || prepared.image_scene().placements().len() > 4096
        {
            return Err(RenderError::Capacity("presentation item budget exhausted"));
        }
        self.gpu.begin();
        let [cw, ch] = self.cell;
        let screen = [
            0.0,
            0.0,
            self.options.size.width as f32,
            self.options.size.height as f32,
        ];
        // Swapchain images are not persistent. Full redraw is a conservative projection
        // of retained damage, including image removals, cursor motion and exposure.
        for (index, cell) in prepared.buffer().cells().iter().enumerate() {
            let x = f32::from(area.x) + (index % usize::from(area.width)) as f32;
            let y = f32::from(area.y) + (index / usize::from(area.width)) as f32;
            let (_, bg) = colors(cell.style.fg, cell.style.bg, cell.style.modifiers);
            self.gpu.quad([x * cw, y * ch, cw, ch], None, bg, screen)?;
        }
        // Deduplicate repeated cells within the fixed atlas; next frame reclaims it all.
        let mut uploaded = HashMap::new();
        for (index, cell) in prepared.buffer().cells().iter().enumerate() {
            if cell.is_wide_continuation() || cell.style.modifiers.contains(Modifier::HIDDEN) {
                continue;
            }
            let x = (f32::from(area.x) + (index % usize::from(area.width)) as f32) * cw;
            let y = (f32::from(area.y) + (index / usize::from(area.width)) as f32) * ch;
            let clip = [x, y, cw * f32::from(cell.width()), ch];
            let (fg, _) = colors(cell.style.fg, cell.style.bg, cell.style.modifiers);
            let style = TextStyle {
                bold: cell.style.modifiers.contains(Modifier::BOLD),
                italic: cell.style.modifiers.contains(Modifier::ITALIC),
            };
            let raster = self.text.rasterize_cell(
                &cell.symbol,
                u16::from(cell.width()),
                style,
                self.options.font_size * self.options.scale_factor,
            )?;
            for (glyph_index, glyph) in raster.glyphs.iter().enumerate() {
                if glyph.width == 0 || glyph.height == 0 {
                    continue;
                }
                let key = (
                    cell.symbol.as_str(),
                    cell.width(),
                    style.bold,
                    style.italic,
                    glyph_index,
                );
                let uv = if let Some(uv) = uploaded.get(&key) {
                    *uv
                } else {
                    let rgba = match glyph.format {
                        BitmapFormat::Rgba => glyph.pixels.clone(),
                        BitmapFormat::Mask => glyph
                            .pixels
                            .iter()
                            .flat_map(|alpha| [255, 255, 255, *alpha])
                            .collect(),
                    };
                    let uv = self.gpu.upload(glyph.width, glyph.height, &rgba)?;
                    uploaded.insert(key, uv);
                    uv
                };
                let tint = if glyph.format == BitmapFormat::Rgba {
                    [1.0; 4]
                } else {
                    fg
                };
                self.gpu.quad(
                    [
                        x + glyph.x + glyph.left as f32,
                        y + self.ascent + glyph.y - glyph.top as f32,
                        glyph.width as f32,
                        glyph.height as f32,
                    ],
                    Some(uv),
                    tint,
                    clip,
                )?;
            }
            if cell.style.modifiers.contains(Modifier::UNDERLINE) {
                self.gpu
                    .quad([x, y + ch - 2.0, clip[2], 1.0], None, fg, clip)?;
            }
            if cell.style.modifiers.contains(Modifier::CROSSED_OUT) {
                self.gpu
                    .quad([x, y + ch * 0.5, clip[2], 1.0], None, fg, clip)?;
            }
        }
        for placement in prepared.image_scene().placements() {
            let (w, h, rgba) = images::decode(&placement.payload)?;
            let uv = self.gpu.upload(w, h, &rgba)?;
            let rect = placement.pixel_rect(f64::from(cw), f64::from(ch));
            let clip = placement.clip.intersection(placement.destination);
            self.gpu.quad(
                [
                    rect.x as f32,
                    rect.y as f32,
                    rect.width as f32,
                    rect.height as f32,
                ],
                Some(uv),
                [1.0; 4],
                [
                    f32::from(clip.x) * cw,
                    f32::from(clip.y) * ch,
                    f32::from(clip.width) * cw,
                    f32::from(clip.height) * ch,
                ],
            )?;
        }
        if let Some(cursor) = prepared.cursor().filter(|cursor| cursor.visible) {
            self.gpu.quad(
                [
                    f32::from(cursor.position.x) * cw,
                    f32::from(cursor.position.y) * ch + ch - 2.0,
                    cw,
                    2.0,
                ],
                None,
                [1.0; 4],
                screen,
            )?;
        }
        self.gpu.present()?;
        Ok(PresentReport {
            generation: self.damage.committed(),
        })
    }
}
fn metrics(
    text: &TextRasterizer,
    options: RendererOptions,
) -> Result<([f32; 2], f32), RenderError> {
    if !options.scale_factor.is_finite() || options.scale_factor <= 0.0 {
        return Err(RenderError::Text(crate::text::TextError::InvalidPixelSize));
    }
    let metrics = text.metrics(options.font_size * options.scale_factor)?;
    Ok((
        [
            metrics.advance.ceil().max(1.0),
            (metrics.ascent + metrics.descent + metrics.leading)
                .ceil()
                .max(1.0),
        ],
        metrics.ascent,
    ))
}
fn colors(fg: Option<Color>, bg: Option<Color>, modifiers: Modifier) -> ([f32; 4], [f32; 4]) {
    let mut fg = color(fg.unwrap_or_default(), [220, 220, 220]);
    let mut bg = color(bg.unwrap_or_default(), [16, 16, 16]);
    if modifiers.contains(Modifier::REVERSED) {
        std::mem::swap(&mut fg, &mut bg);
    }
    if modifiers.contains(Modifier::DIM) {
        for value in &mut fg[..3] {
            *value *= 0.5;
        }
    }
    (fg, bg)
}
fn color(color: Color, default: [u8; 3]) -> [f32; 4] {
    const ANSI: [[u8; 3]; 16] = [
        [0, 0, 0],
        [205, 49, 49],
        [13, 188, 121],
        [229, 229, 16],
        [36, 114, 200],
        [188, 63, 188],
        [17, 168, 205],
        [229, 229, 229],
        [102, 102, 102],
        [241, 76, 76],
        [35, 209, 139],
        [245, 245, 67],
        [59, 142, 234],
        [214, 112, 214],
        [41, 184, 219],
        [255, 255, 255],
    ];
    let rgb = match color {
        Color::Default => default,
        Color::Rgb(r, g, b) => [r, g, b],
        Color::Indexed(i) => match i {
            0..=15 => ANSI[usize::from(i)],
            16..=231 => {
                let i = i - 16;
                let level = |v: u8| if v == 0 { 0 } else { 55 + 40 * v };
                [level(i / 36), level(i / 6 % 6), level(i % 6)]
            }
            _ => [8 + 10 * (i - 232); 3],
        },
        Color::Black => ANSI[0],
        Color::Red => ANSI[1],
        Color::Green => ANSI[2],
        Color::Yellow => ANSI[3],
        Color::Blue => ANSI[4],
        Color::Magenta => ANSI[5],
        Color::Cyan => ANSI[6],
        Color::White => ANSI[7],
        Color::BrightBlack => ANSI[8],
        Color::BrightRed => ANSI[9],
        Color::BrightGreen => ANSI[10],
        Color::BrightYellow => ANSI[11],
        Color::BrightBlue => ANSI[12],
        Color::BrightMagenta => ANSI[13],
        Color::BrightCyan => ANSI[14],
        Color::BrightWhite => ANSI[15],
    };
    // Vertex colors enter an sRGB framebuffer as linear values.
    let linear = |v: u8| {
        let v = f32::from(v) / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    [linear(rgb[0]), linear(rgb[1]), linear(rgb[2]), 1.0]
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn indexed_endpoints_and_reverse_are_exact() {
        assert_eq!(color(Color::Indexed(16), [1; 3]), [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(color(Color::Indexed(231), [1; 3]), [1.0; 4]);
        let normal = colors(Some(Color::Red), Some(Color::Blue), Modifier::EMPTY);
        let reverse = colors(Some(Color::Red), Some(Color::Blue), Modifier::REVERSED);
        assert_eq!(normal, (reverse.1, reverse.0));
    }
}
