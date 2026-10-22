//! Cell-local shaping and rasterization. Terminal columns are input authority, never font advances.
//!
//! One call consumes one retained terminal cell's complete cluster (including combining marks,
//! ZWJ sequences and wide-cell content). The caller supplies its cell origin and clip rectangle.
//! Glyph positions are physical pixels relative to that origin's baseline, y increasing down.
//! Source ranges are UTF-8 byte ranges in the supplied string, not selectable new layout nodes.
//! No cross-cell ligatures, line breaking, or bidi reordering is performed.

use std::{fmt, ops::Range, sync::Arc};

use swash::{
    scale::{Render, ScaleContext, Source, StrikeWith, image::Content},
    shape::ShapeContext,
    text::{Codepoint, Script},
};

use crate::{
    fonts::Fonts,
    text_cache::{Key, TextCache},
};

/// Font face selection; colors remain caller-owned paint properties.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextStyle {
    pub bold: bool,
    pub italic: bool,
}

/// Owned bitmap representation; no rasterizer types escape this module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitmapFormat {
    /// One coverage byte per pixel; tint with the retained cell foreground.
    Mask,
    /// Four RGBA bytes per pixel containing intrinsic color.
    Rgba,
}

/// A positioned glyph. Clip all pixels to the authoritative cell allocation.
#[derive(Debug)]
pub struct GlyphBitmap {
    pub source: Range<usize>,
    pub glyph_id: u16,
    pub missing: bool,
    /// Shaped baseline offset, physical pixels, y down.
    pub x: f32,
    pub y: f32,
    /// Bitmap left bearing and top distance above the glyph baseline.
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    pub format: BitmapFormat,
    pub pixels: Vec<u8>,
}

#[derive(Debug)]
pub struct RasterizedCell {
    /// Unmodified caller-supplied terminal column allocation, including zero-width clusters.
    pub columns: u16,
    pub source_bytes: usize,
    pub glyphs: Vec<GlyphBitmap>,
}

/// Metrics for converting a configured font size to physical cell dimensions.
#[derive(Clone, Copy, Debug)]
pub struct CellMetrics {
    pub advance: f32,
    pub ascent: f32,
    pub descent: f32,
    pub leading: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextError {
    NoFonts,
    InvalidPixelSize,
    ClusterTooLarge,
    RasterizationFailed,
}
impl fmt::Display for TextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "native text: {self:?}")
    }
}
impl std::error::Error for TextError {}

/// Window/renderer-owned, non-global shaping state and derived bitmap cache.
/// Retention defaults to 8 MiB/2048 cells; font data is capped at 64 MiB/64 faces.
/// Returned `Arc`s may outlive eviction; consumers must bound their own in-flight frames.
pub struct TextRasterizer {
    fonts: Fonts,
    shape: ShapeContext,
    scale: ScaleContext,
    cache: TextCache,
}

impl TextRasterizer {
    /// Deterministic bundled fonts only; does not inspect the host font installation.
    pub fn bundled() -> Result<Self, TextError> {
        let fonts = Fonts::bundled();
        if fonts.is_empty() {
            return Err(TextError::NoFonts);
        }
        Ok(Self {
            fonts,
            shape: ShapeContext::with_max_entries(8),
            scale: ScaleContext::with_max_entries(8),
            cache: TextCache::new(8 * 1024 * 1024, 2048),
        })
    }

    /// Bundled primary faces plus bounded installed fallback faces, including color emoji.
    pub fn with_system_fonts() -> Result<Self, TextError> {
        let mut rasterizer = Self::bundled()?;
        rasterizer.fonts.load_system();
        Ok(rasterizer)
    }

    pub fn metrics(&self, pixel_size: f32) -> Result<CellMetrics, TextError> {
        validate_size(pixel_size)?;
        let font = self.fonts.get(0).font();
        let metrics = font.metrics(&[]).scale(pixel_size);
        let advance = font
            .glyph_metrics(&[])
            .scale(pixel_size)
            .advance_width(font.charmap().map('M'));
        Ok(CellMetrics {
            advance,
            ascent: metrics.ascent,
            descent: metrics.descent.abs(),
            leading: metrics.leading,
        })
    }

    /// Cache retention only, excluding caller-held returned bitmaps and immutable font data.
    pub fn cache_usage(&self) -> (usize, usize) {
        self.cache.usage()
    }

    pub fn rasterize_cell(
        &mut self,
        text: &str,
        columns: u16,
        style: TextStyle,
        pixel_size: f32,
    ) -> Result<Arc<RasterizedCell>, TextError> {
        validate_size(pixel_size)?;
        // Bound transient shape/raster work too, not just retained entries.
        if text.len() > 1024 {
            return Err(TextError::ClusterTooLarge);
        }
        let key = Key {
            text: text.to_owned(),
            columns,
            style,
            size: pixel_size.to_bits(),
        };
        if let Some(value) = self.cache.get(&key) {
            return Ok(value);
        }
        let font = self.fonts.get(self.fonts.select(text, style)).font();
        let script = text
            .chars()
            .map(|ch| ch.properties().script())
            .find(|script| !matches!(script, Script::Common | Script::Inherited))
            .unwrap_or(Script::Latin);
        let mut shaper = self
            .shape
            .builder(font)
            .size(pixel_size)
            .script(script)
            .build();
        shaper.add_str(text);
        let mut shaped = Vec::new();
        shaper.shape_with(|cluster| {
            for glyph in cluster.glyphs {
                shaped.push((cluster.source.to_range(), *glyph));
            }
        });
        let mut scaler = self.scale.builder(font).size(pixel_size).hint(true).build();
        let mut glyphs = Vec::new();
        let mut pen = 0.0;
        let mut bytes = 0;
        for (source, glyph) in shaped {
            let image = Render::new(&[
                Source::ColorOutline(0),
                Source::ColorBitmap(StrikeWith::BestFit),
                Source::Outline,
            ])
            .render(&mut scaler, glyph.id);
            if let Some(image) = image {
                bytes += image.data.len();
                if bytes > 4 * 1024 * 1024 {
                    return Err(TextError::ClusterTooLarge);
                }
                let format = match image.content {
                    Content::Mask => BitmapFormat::Mask,
                    Content::Color => BitmapFormat::Rgba,
                    Content::SubpixelMask => return Err(TextError::RasterizationFailed),
                };
                glyphs.push(GlyphBitmap {
                    source,
                    glyph_id: glyph.id,
                    missing: glyph.id == 0,
                    x: pen + glyph.x,
                    y: -glyph.y,
                    left: image.placement.left,
                    top: image.placement.top,
                    width: image.placement.width,
                    height: image.placement.height,
                    format,
                    pixels: image.data,
                });
            } else if !text[source.clone()].chars().all(|ch| {
                ch.is_whitespace()
                    || ch.is_control()
                    || matches!(ch, '\u{200d}' | '\u{fe0e}' | '\u{fe0f}')
            }) {
                // A backend failure must not masquerade as a successfully painted cell.
                return Err(TextError::RasterizationFailed);
            }
            pen += glyph.advance;
        }
        let value = Arc::new(RasterizedCell {
            columns,
            source_bytes: text.len(),
            glyphs,
        });
        self.cache.insert(key, Arc::clone(&value));
        Ok(value)
    }
}

fn validate_size(size: f32) -> Result<(), TextError> {
    if size.is_finite() && (1.0..=256.0).contains(&size) {
        Ok(())
    } else {
        Err(TextError::InvalidPixelSize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_bitmap_and_metrics_are_real() {
        let mut raster = TextRasterizer::bundled().unwrap();
        let metrics = raster.metrics(18.0).unwrap();
        assert!(metrics.advance > 0.0 && metrics.ascent > 0.0);
        let cell = raster
            .rasterize_cell("M", 1, TextStyle::default(), 18.0)
            .unwrap();
        assert_eq!(cell.columns, 1);
        assert!(
            cell.glyphs
                .iter()
                .any(|g| !g.missing && g.pixels.iter().any(|&p| p != 0))
        );
        let again = raster
            .rasterize_cell("M", 1, TextStyle::default(), 18.0)
            .unwrap();
        assert!(Arc::ptr_eq(&cell, &again));
    }

    #[test]
    fn combining_wide_and_emoji_preserve_authoritative_columns_and_utf8() {
        let mut raster = TextRasterizer::bundled().unwrap();
        for (text, columns) in [
            ("a\u{301}", 1),
            ("界", 2),
            ("👩\u{200d}💻", 2),
            ("\u{301}", 0),
            ("אב", 2),
        ] {
            let cell = raster
                .rasterize_cell(text, columns, TextStyle::default(), 18.0)
                .unwrap();
            assert_eq!(cell.columns, columns);
            assert_eq!(cell.source_bytes, text.len());
            for glyph in &cell.glyphs {
                assert!(text.is_char_boundary(glyph.source.start));
                assert!(text.is_char_boundary(glyph.source.end));
                assert!(glyph.source.start <= glyph.source.end && glyph.source.end <= text.len());
                let channels = if glyph.format == BitmapFormat::Mask {
                    1
                } else {
                    4
                };
                assert_eq!(
                    glyph.pixels.len(),
                    glyph.width as usize * glyph.height as usize * channels
                );
            }
        }
    }

    #[test]
    fn bundled_emoji_fallback_is_not_a_missing_glyph() {
        let mut raster = TextRasterizer::bundled().unwrap();
        let cell = raster
            .rasterize_cell("😀", 2, TextStyle::default(), 20.0)
            .unwrap();
        assert!(!cell.glyphs.is_empty());
        assert!(cell.glyphs.iter().all(|glyph| !glyph.missing));
        assert!(
            cell.glyphs
                .iter()
                .any(|glyph| glyph.pixels.iter().any(|&p| p != 0))
        );
    }

    #[test]
    fn rejects_unbounded_work_and_invalid_sizes() {
        let mut raster = TextRasterizer::bundled().unwrap();
        for size in [0.0, -1.0, f32::NAN, f32::INFINITY, 257.0] {
            assert_eq!(
                raster.metrics(size).unwrap_err(),
                TextError::InvalidPixelSize
            );
        }
        assert_eq!(
            raster
                .rasterize_cell(&"x".repeat(1025), 1, TextStyle::default(), 16.0)
                .unwrap_err(),
            TextError::ClusterTooLarge
        );
    }
}
