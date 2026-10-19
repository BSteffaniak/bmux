//! Immutable, bounded font faces; third-party font database and scaler types stay private.

use std::sync::Arc;

use swash::{CacheKey, FontRef};

use crate::text::TextStyle;

const MAX_FONTS: usize = 64;
const MAX_FONT_BYTES: usize = 64 * 1024 * 1024;

pub(crate) struct Face {
    data: Arc<[u8]>,
    offset: u32,
    key: CacheKey,
    style: TextStyle,
}

impl Face {
    pub(crate) fn font(&self) -> FontRef<'_> {
        FontRef {
            data: &self.data,
            offset: self.offset,
            key: self.key,
        }
    }
}

pub(crate) struct Fonts {
    faces: Vec<Face>,
    bytes: usize,
}

impl Fonts {
    pub(crate) fn bundled() -> Self {
        let mut fonts = Self {
            faces: Vec::new(),
            bytes: 0,
        };
        for face in bmux_fonts::bundled_fonts_for_preset(bmux_fonts::FontPreset::GhosttyNerd) {
            let style = match face.style {
                bmux_fonts::FontStyle::Regular => TextStyle::default(),
                bmux_fonts::FontStyle::Bold => TextStyle {
                    bold: true,
                    italic: false,
                },
                bmux_fonts::FontStyle::Italic => TextStyle {
                    bold: false,
                    italic: true,
                },
                bmux_fonts::FontStyle::BoldItalic => TextStyle {
                    bold: true,
                    italic: true,
                },
            };
            fonts.add(face.data, 0, style);
        }
        // The existing OFL diagram derivative supplies deterministic monochrome emoji
        // fallback without requiring a host font installation.
        fonts.add(bmux_fonts::DIAGRAM_FONT, 0, TextStyle::default());
        fonts
    }

    fn add(&mut self, data: &[u8], index: u32, style: TextStyle) {
        if self.faces.len() >= MAX_FONTS || data.len() > MAX_FONT_BYTES.saturating_sub(self.bytes) {
            return;
        }
        if let Some(font) = FontRef::from_index(data, index as usize) {
            let offset = font.offset;
            self.faces.push(Face {
                data: Arc::from(data),
                offset,
                key: CacheKey::new(),
                style,
            });
            self.bytes += data.len();
        }
    }

    pub(crate) fn load_system(&mut self) {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        // Prefer color emoji and monospace fallbacks before other installed faces.
        let mut ids: Vec<_> = db
            .faces()
            .map(|f| {
                let emoji = f
                    .families
                    .iter()
                    .any(|(name, _)| name.to_lowercase().contains("emoji"));
                (
                    (!emoji, !f.monospaced),
                    f.id,
                    TextStyle {
                        bold: f.weight.0 >= 600,
                        italic: f.style != fontdb::Style::Normal,
                    },
                )
            })
            .collect();
        ids.sort_by_key(|entry| entry.0);
        for (_, id, style) in ids {
            if self.faces.len() >= MAX_FONTS || self.bytes >= MAX_FONT_BYTES {
                break;
            }
            db.with_face_data(id, |data, index| self.add(data, index, style));
        }
    }

    pub(crate) fn get(&self, index: usize) -> &Face {
        &self.faces[index]
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.faces.is_empty()
    }

    pub(crate) fn select(&self, text: &str, style: TextStyle) -> usize {
        let covers = |face: &&Face| {
            let map = face.font().charmap();
            text.chars()
                .all(|ch| matches!(ch, '\u{200d}' | '\u{fe0e}' | '\u{fe0f}') || map.map(ch) != 0)
        };
        let wants_color = text.contains('\u{fe0f}')
            || (!text.contains('\u{fe0e}')
                && text.chars().any(|ch| {
                    let cp = ch as u32;
                    (0x1f000..=0x1faff).contains(&cp)
                }));
        self.faces
            .iter()
            .enumerate()
            .filter(|(_, face)| covers(face))
            .min_by_key(|(_, face)| {
                let font = face.font();
                let color = font.table(swash::tag_from_bytes(b"COLR")).is_some()
                    || font.table(swash::tag_from_bytes(b"CBDT")).is_some()
                    || font.table(swash::tag_from_bytes(b"sbix")).is_some();
                (
                    wants_color && !color,
                    u8::from(face.style.bold != style.bold)
                        + u8::from(face.style.italic != style.italic),
                )
            })
            .map_or(0, |(index, _)| index)
    }
}
