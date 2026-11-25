//! A screenshot of the terminal as a PNG, for agents that read images:
//! layout, color and alignment problems are easier to see than to infer
//! from text.

use ab_glyph::{Font as _, FontArc, FontVec, PxScale, ScaleFont as _, point};
use anyhow::{Context as _, Result};
use bmux_terminal_grid::{Color, GridSnapshot, Style};

const FONT_PX: f32 = 18.0;
const PADDING: usize = 8;
const DEFAULT_FG: [u8; 3] = [0xd4, 0xd4, 0xd4];
const DEFAULT_BG: [u8; 3] = [0x1e, 0x1e, 0x1e];

/// The xterm 16-color palette.
const ANSI: [[u8; 3]; 16] = [
    [0x00, 0x00, 0x00],
    [0xcd, 0x31, 0x31],
    [0x0d, 0xbc, 0x79],
    [0xe5, 0xe5, 0x10],
    [0x24, 0x72, 0xc8],
    [0xbc, 0x3f, 0xbc],
    [0x11, 0xa8, 0xcd],
    [0xe5, 0xe5, 0xe5],
    [0x66, 0x66, 0x66],
    [0xf1, 0x4c, 0x4c],
    [0x23, 0xd1, 0x8b],
    [0xf5, 0xf5, 0x43],
    [0x3b, 0x8e, 0xea],
    [0xd6, 0x70, 0xd6],
    [0x29, 0xb8, 0xdb],
    [0xff, 0xff, 0xff],
];

struct Fonts {
    regular: Vec<FontArc>,
    bold: Vec<FontArc>,
}

fn load_fonts() -> Fonts {
    let mut regular = bmux_fonts::load_preset_fonts_for_ab_glyph(bmux_fonts::FontPreset::GhosttyNerd);
    let mut bold = Vec::new();
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    let mut families: Vec<String> =
        bmux_fonts::default_families_for_preset(bmux_fonts::FontPreset::SystemMonospace);
    families.extend(
        [
            "JetBrains Mono",
            "Noto Sans Mono",
            "Fira Code",
            "Cascadia Mono",
            "Consolas",
            "Ubuntu Mono",
        ]
        .map(str::to_owned),
    );
    let load = |family: fontdb::Family<'_>, weight: fontdb::Weight| -> Option<FontArc> {
        let id = db.query(&fontdb::Query {
            families: &[family],
            weight,
            stretch: fontdb::Stretch::Normal,
            style: fontdb::Style::Normal,
        })?;
        let mut font = None;
        db.with_face_data(id, |data, index| {
            font = FontVec::try_from_vec_and_index(data.to_vec(), index)
                .ok()
                .map(FontArc::new);
        });
        font
    };
    for family in families.iter().map(|name| fontdb::Family::Name(name)) {
        if let Some(font) = load(family, fontdb::Weight::NORMAL) {
            regular.push(font);
            if let Some(font) = load(family, fontdb::Weight::BOLD) {
                bold.push(font);
            }
            break;
        }
    }
    if let Some(font) = load(fontdb::Family::Monospace, fontdb::Weight::NORMAL) {
        regular.push(font);
    }
    // Fallbacks for symbols and CJK a monospace font may lack.
    for family in ["Noto Sans Symbols 2", "Noto Sans CJK SC", "Noto Color Emoji", "DejaVu Sans"] {
        if let Some(font) = load(fontdb::Family::Name(family), fontdb::Weight::NORMAL) {
            regular.push(font);
        }
    }
    bold.extend(regular.iter().cloned());
    Fonts { regular, bold }
}

fn resolve(color: Option<Color>, default: [u8; 3]) -> [u8; 3] {
    match color {
        None => default,
        Some(Color::Rgb { r, g, b }) => [r, g, b],
        Some(Color::Indexed(index)) => indexed(index),
    }
}

fn indexed(index: u8) -> [u8; 3] {
    match index {
        0..=15 => ANSI[usize::from(index)],
        16..=231 => {
            let value = index - 16;
            let level = |n: u8| if n == 0 { 0 } else { 55 + n * 40 };
            [level(value / 36), level((value / 6) % 6), level(value % 6)]
        }
        _ => {
            let gray = 8 + (index - 232) * 10;
            [gray, gray, gray]
        }
    }
}

fn cell_colors(style: Style) -> ([u8; 3], [u8; 3]) {
    let mut fg = resolve(style.fg, DEFAULT_FG);
    let mut bg = resolve(style.bg, DEFAULT_BG);
    if let Some(Color::Indexed(index @ 0..=7)) = style.fg
        && style.bold
    {
        fg = ANSI[usize::from(index) + 8];
    }
    if style.inverse {
        std::mem::swap(&mut fg, &mut bg);
    }
    if style.dim {
        fg = fg.map(|channel| channel / 2 + bg[0] / 4);
    }
    if style.hidden {
        fg = bg;
    }
    (fg, bg)
}

struct Canvas {
    width: usize,
    height: usize,
    rgba: Vec<u8>,
}

impl Canvas {
    fn new(width: usize, height: usize) -> Self {
        let mut rgba = vec![0xff; width * height * 4];
        for pixel in rgba.as_chunks_mut::<4>().0 {
            pixel[..3].copy_from_slice(&DEFAULT_BG);
        }
        Self {
            width,
            height,
            rgba,
        }
    }

    fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, color: [u8; 3]) {
        for row in y..(y + h).min(self.height) {
            for col in x..(x + w).min(self.width) {
                let at = (row * self.width + col) * 4;
                self.rgba[at..at + 3].copy_from_slice(&color);
            }
        }
    }

    fn blend(&mut self, x: i64, y: i64, coverage: f32, color: [u8; 3]) {
        let (Ok(x), Ok(y)) = (usize::try_from(x), usize::try_from(y)) else {
            return;
        };
        if x >= self.width || y >= self.height {
            return;
        }
        let at = (y * self.width + x) * 4;
        let alpha = coverage.clamp(0.0, 1.0);
        for (under, over) in self.rgba[at..at + 3].iter_mut().zip(color) {
            let mixed = f32::from(*under) + (f32::from(over) - f32::from(*under)) * alpha;
            // Rounded and clamped to a channel value.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let mixed = mixed.round().clamp(0.0, 255.0) as u8;
            *under = mixed;
        }
    }
}

/// Render the snapshot as PNG bytes.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
// Pixel geometry: small non-negative values converted between f32 and integers.
pub fn render_png(snapshot: &GridSnapshot) -> Result<Vec<u8>> {
    let fonts = load_fonts();
    let scale = PxScale::from(FONT_PX);
    let (cell_w, cell_h, ascent) = fonts.regular.first().map_or((10.0, 20.0, 15.0), |font| {
        let scaled = font.as_scaled(scale);
        let advance = scaled.h_advance(font.glyph_id('M'));
        (advance.ceil(), (scaled.ascent() - scaled.descent() + scaled.line_gap()).ceil(), scaled.ascent())
    });
    let cell_w = cell_w.max(1.0) as usize;
    let cell_h = cell_h.max(1.0) as usize;
    let cols = usize::from(snapshot.width);
    let rows = usize::from(snapshot.height);
    let mut canvas = Canvas::new(cols * cell_w + PADDING * 2, rows * cell_h + PADDING * 2);
    let style_of = |id: u32| -> Style {
        snapshot
            .styles
            .get(usize::try_from(id).unwrap_or(usize::MAX))
            .copied()
            .unwrap_or_default()
    };

    for (row, line) in snapshot.rows.iter().enumerate().take(rows) {
        let top = PADDING + row * cell_h;
        // Backgrounds first, then glyphs, so wide glyphs are not overpainted.
        let mut glyphs = Vec::new();
        for run in &line.runs {
            let style = style_of(run.style.0);
            let (fg, bg) = cell_colors(style);
            let mut col = usize::from(run.start_col);
            for grapheme in bmux_unicode_segmentation::graphemes(&run.text) {
                if col >= cols {
                    break;
                }
                let width = bmux_cell_geometry::grapheme_width(grapheme).max(1);
                canvas.fill(PADDING + col * cell_w, top, cell_w * width, cell_h, bg);
                if let Some(c) = grapheme.chars().next()
                    && !c.is_whitespace()
                {
                    glyphs.push((col, c, fg, style));
                }
                col += width;
            }
        }
        for (col, c, fg, style) in glyphs {
            let candidates = if style.bold { &fonts.bold } else { &fonts.regular };
            let Some(font) = candidates
                .iter()
                .chain(fonts.regular.iter())
                .find(|font| font.glyph_id(c).0 != 0)
            else {
                continue;
            };
            let x = (PADDING + col * cell_w) as f32;
            let glyph = font
                .glyph_id(c)
                .with_scale_and_position(scale, point(x, top as f32 + ascent));
            if let Some(outline) = font.outline_glyph(glyph) {
                let bounds = outline.px_bounds();
                outline.draw(|gx, gy, coverage| {
                    canvas.blend(
                        i64::from(gx) + bounds.min.x as i64,
                        i64::from(gy) + bounds.min.y as i64,
                        coverage,
                        fg,
                    );
                });
            }
            if style.underline {
                canvas.fill(PADDING + col * cell_w, top + cell_h - 2, cell_w, 1, fg);
            }
            if style.strike {
                canvas.fill(PADDING + col * cell_w, top + cell_h / 2, cell_w, 1, fg);
            }
        }
    }

    let cursor = snapshot.cursor;
    if cursor.visible && usize::from(cursor.row) < rows && usize::from(cursor.col) < cols {
        let x = PADDING + usize::from(cursor.col) * cell_w;
        let y = PADDING + usize::from(cursor.row) * cell_h;
        // An outlined block keeps the glyph under the cursor readable.
        let color = [0xff, 0xcc, 0x00];
        canvas.fill(x, y, cell_w, 2, color);
        canvas.fill(x, y + cell_h - 2, cell_w, 2, color);
        canvas.fill(x, y, 2, cell_h, color);
        canvas.fill(x + cell_w - 2, y, 2, cell_h, color);
    }

    bmux_raster::encode_png(
        u32::try_from(canvas.width).context("screenshot too wide")?,
        u32::try_from(canvas.height).context("screenshot too tall")?,
        4,
        &canvas.rgba,
    )
    .map_err(|error| anyhow::anyhow!("encoding the screenshot failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_terminal_grid::{GridLimits, TerminalGridStream};

    #[test]
    fn renders_a_decodable_png_of_the_grid_size() {
        let mut stream = TerminalGridStream::new(20, 4, GridLimits::default()).unwrap();
        stream.process(b"\x1b[41mhi\x1b[0m there");
        let png = render_png(&stream.snapshot(0, 4)).unwrap();
        let raster = bmux_raster::decode(&png, 4096, 4096, 64 * 1024 * 1024).unwrap();
        assert!(raster.width > 20 && raster.height > 4);
    }

    #[test]
    fn palette_covers_the_cube_and_grays() {
        assert_eq!(indexed(16), [0, 0, 0]);
        assert_eq!(indexed(231), [255, 255, 255]);
        assert_eq!(indexed(232), [8, 8, 8]);
        let (fg, bg) = cell_colors(Style {
            inverse: true,
            ..Style::default()
        });
        assert_eq!((fg, bg), (DEFAULT_BG, DEFAULT_FG));
    }
}
