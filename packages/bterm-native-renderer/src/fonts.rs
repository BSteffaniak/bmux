//! Immutable, bounded font faces; third-party font database and scaler types stay private.

use std::sync::Arc;

use swash::{CacheKey, FontRef};

use crate::text::TextStyle;

const MAX_FONTS: usize = 64;
const MAX_FONT_BYTES: usize = 64 * 1024 * 1024;

/// Validate the tables consumed by the configured Swash shape/raster path before
/// retaining a face. No byte slicing/rebasing: collection table offsets remain
/// relative to the complete file. This is not a replacement for raster errors.
fn supports_renderer(font: FontRef<'_>) -> bool {
    let table = |tag| font.table(swash::tag_from_bytes(tag));
    let glyphs = table(b"maxp")
        .and_then(|bytes| bytes.get(4..6))
        .map(|bytes| u16::from_be_bytes([bytes[0], bytes[1]]));
    let Some(glyphs) = glyphs.filter(|&count| count > 0) else {
        return false;
    };
    let metrics_valid = |header, metrics| {
        let count = table(header)
            .and_then(|bytes| bytes.get(34..36))
            .map(|bytes| u16::from_be_bytes([bytes[0], bytes[1]]));
        count.is_some_and(|count| {
            count > 0
                && count <= glyphs
                && table(metrics).is_some_and(|bytes| {
                    bytes.len() >= usize::from(count) * 4 + usize::from(glyphs - count) * 2
                })
        })
    };
    if !metrics_valid(b"hhea", b"hmtx")
        || (table(b"vhea").is_some() && !metrics_valid(b"vhea", b"vmtx"))
    {
        return false;
    }
    // These are exactly the source families requested by text.rs (no legacy
    // Apple bdat/bloc and no monochrome bitmap source in that render list).
    (table(b"glyf").is_some() && table(b"loca").is_some())
        || table(b"CFF ").is_some()
        || table(b"CFF2").is_some()
        || (table(b"COLR").is_some() && table(b"CPAL").is_some())
        || table(b"sbix").is_some()
        || (table(b"CBDT").is_some() && table(b"CBLC").is_some())
}

pub(crate) struct Face {
    data: Arc<[u8]>,
    offset: u32,
    key: CacheKey,
    style: TextStyle,
    families: Vec<String>,
    priority: usize,
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
            let before = fonts.faces.len();
            fonts.add(face.data, 0, style);
            if fonts.faces.len() > before {
                fonts.faces[before].families = vec![face.family.to_owned()];
            }
        }
        // The existing OFL diagram derivative supplies deterministic monochrome emoji
        // fallback without requiring a host font installation.
        let before = fonts.faces.len();
        fonts.add(bmux_fonts::DIAGRAM_FONT, 0, TextStyle::default());
        if fonts.faces.len() > before {
            fonts.faces[before].families = vec![bmux_fonts::DIAGRAM_FONT_FAMILY.to_owned()];
        }
        fonts
    }

    fn add(&mut self, data: &[u8], index: u32, style: TextStyle) {
        if self.faces.len() >= MAX_FONTS || data.len() > MAX_FONT_BYTES.saturating_sub(self.bytes) {
            return;
        }
        if let Some(font) = FontRef::from_index(data, index as usize) {
            // Character coverage alone does not establish renderer compatibility.
            // For example, macOS NISC18030 has cmap + legacy bdat/bloc/bhed,
            // but no horizontal metrics or a source understood by Swash. Passing
            // it to the shaper underflows Swash's zero long-metric count in debug
            // builds and fails rasterization in release builds. Do not let such a
            // face shadow a usable fallback for the same character.
            if !supports_renderer(font) {
                return;
            }
            let offset = font.offset;
            self.faces.push(Face {
                data: Arc::from(data),
                offset,
                key: CacheKey::new(),
                style,
                families: Vec::new(),
                priority: 0,
            });
            self.bytes += data.len();
        }
    }

    pub(crate) fn load_system(&mut self) {
        self.load_selected_system(None, &[]);
    }

    pub(crate) fn configured(family: Option<&str>, fallback: &[String]) -> Self {
        let mut fonts = Self::bundled();
        let rank = |names: &[String]| family_rank(names, family, fallback, true);
        for face in &mut fonts.faces {
            face.priority = rank(&face.families);
        }
        fonts.load_selected_system(family, fallback);
        fonts.faces.sort_by_key(|face| {
            (
                face.priority,
                u8::from(face.style.bold) + u8::from(face.style.italic),
            )
        });
        fonts
    }

    fn load_selected_system(&mut self, family: Option<&str>, fallback: &[String]) {
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
                    (
                        family_rank(
                            &f.families
                                .iter()
                                .map(|(name, _)| name.clone())
                                .collect::<Vec<_>>(),
                            family,
                            fallback,
                            false,
                        ),
                        !emoji,
                        !f.monospaced,
                    ),
                    f.families
                        .iter()
                        .map(|(name, _)| name.clone())
                        .collect::<Vec<_>>(),
                    f.post_script_name.clone(),
                    match &f.source {
                        fontdb::Source::File(path) | fontdb::Source::SharedFile(path, _) => {
                            path.clone()
                        }
                        fontdb::Source::Binary(_) => std::path::PathBuf::new(),
                    },
                    f.index,
                    f.id,
                    TextStyle {
                        bold: f.weight.0 >= 600,
                        italic: f.style != fontdb::Style::Normal,
                    },
                )
            })
            .collect();
        ids.sort_by(|a, b| (&a.0, &a.1, &a.2, &a.3, a.4).cmp(&(&b.0, &b.1, &b.2, &b.3, b.4)));
        for (priority, families, _, _, _, id, style) in ids {
            if self.faces.len() >= MAX_FONTS || self.bytes >= MAX_FONT_BYTES {
                break;
            }
            let before = self.faces.len();
            db.with_face_data(id, |data, index| self.add(data, index, style));
            if self.faces.len() > before {
                let face = &mut self.faces[before];
                face.families = families;
                face.priority = priority.0;
            }
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
                    face.priority,
                    u8::from(face.style.bold != style.bold)
                        + u8::from(face.style.italic != style.italic),
                )
            })
            .map_or(0, |(index, _)| index)
    }
}

/// Requested families precede the bundled safety net. Unknown names are ignored;
/// installed-font enumeration order never decides equal-priority fallback.
fn family_rank(
    names: &[String],
    family: Option<&str>,
    fallback: &[String],
    bundled: bool,
) -> usize {
    let matches = |requested: &str| {
        names
            .iter()
            .any(|name| name.eq_ignore_ascii_case(requested))
    };
    if family.is_some_and(matches) {
        return 0;
    }
    if family.is_none() && bundled {
        return 0;
    }
    if let Some(index) = fallback.iter().position(|name| matches(name)) {
        return index + 1;
    }
    fallback.len() + if bundled { 1 } else { 2 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_families_have_ordered_case_insensitive_priority() {
        let names = vec!["Chosen Mono".to_owned()];
        let fallback = vec!["Other Mono".to_owned(), "chosen mono".to_owned()];
        assert_eq!(
            family_rank(&names, Some("CHOSEN MONO"), &fallback, false),
            0
        );
        assert_eq!(
            family_rank(&names, Some("Unavailable"), &fallback, false),
            2
        );
        assert_eq!(family_rank(&names, None, &fallback, true), 0);
        assert_eq!(family_rank(&names, Some("Unavailable"), &[], true), 1);
        assert_eq!(family_rank(&names, Some("Unavailable"), &[], false), 2);
    }

    #[test]
    fn selected_bundled_family_controls_metrics_and_style_order() {
        // Exercise selection without depending on installed family names.
        let mut fonts = Fonts::bundled();
        let requested = Some(bmux_fonts::DIAGRAM_FONT_FAMILY);
        for face in &mut fonts.faces {
            face.priority = family_rank(&face.families, requested, &[], true);
        }
        fonts.faces.sort_by_key(|face| {
            (
                face.priority,
                u8::from(face.style.bold) + u8::from(face.style.italic),
            )
        });
        assert!(
            fonts
                .get(0)
                .families
                .iter()
                .any(|name| name == bmux_fonts::DIAGRAM_FONT_FAMILY)
        );
        assert_eq!(fonts.select("ABC", TextStyle::default()), 0);
    }

    fn primary_data() -> Vec<u8> {
        bmux_fonts::bundled_fonts_for_preset(bmux_fonts::FontPreset::GhosttyNerd)[0]
            .data
            .to_vec()
    }

    fn table_record(data: &[u8], tag: &[u8; 4]) -> usize {
        let count = usize::from(u16::from_be_bytes([data[4], data[5]]));
        (0..count)
            .map(|index| 12 + index * 16)
            .find(|&offset| &data[offset..offset + 4] == tag)
            .unwrap()
    }

    #[test]
    fn admission_rejects_missing_and_zero_metrics_before_shaping() {
        let original = primary_data();
        assert!(supports_renderer(
            FontRef::from_index(&original, 0).unwrap()
        ));
        let mut data = original.clone();
        let record = table_record(&data, b"hhea");
        let offset = u32::from_be_bytes(data[record + 8..record + 12].try_into().unwrap()) as usize;
        data[offset + 34..offset + 36].fill(0);
        assert!(!supports_renderer(FontRef::from_index(&data, 0).unwrap()));
        // Removing the complete directory entry preserves the sorted directory,
        // modelling the absent hhea in the legacy Apple bitmap-only face.
        let mut data = original;
        let record = table_record(&data, b"hhea");
        let count = u16::from_be_bytes([data[4], data[5]]);
        data.copy_within(record + 16..12 + usize::from(count) * 16, record);
        data[4..6].copy_from_slice(&(count - 1).to_be_bytes());
        assert!(!supports_renderer(FontRef::from_index(&data, 0).unwrap()));
        let mut fonts = Fonts {
            faces: Vec::new(),
            bytes: 0,
        };
        fonts.add(&data, 0, TextStyle::default());
        assert!(fonts.is_empty());
        assert_eq!(fonts.bytes, 0);
    }

    #[test]
    fn collection_face_keeps_full_file_and_directory_offset() {
        let data = primary_data();
        // A minimal one-face TTC built from the existing real bundled font.
        // All SFNT table offsets must be rebased by the TTC header length.
        const HEADER: u32 = 16;
        let mut collection = b"ttcf\x00\x01\x00\x00\x00\x00\x00\x01\x00\x00\x00\x10".to_vec();
        collection.extend_from_slice(&data);
        let count = usize::from(u16::from_be_bytes([data[4], data[5]]));
        for index in 0..count {
            let offset = HEADER as usize + 12 + index * 16 + 8;
            let value = u32::from_be_bytes(collection[offset..offset + 4].try_into().unwrap());
            collection[offset..offset + 4].copy_from_slice(&(value + HEADER).to_be_bytes());
        }
        let mut fonts = Fonts {
            faces: Vec::new(),
            bytes: 0,
        };
        fonts.add(&collection, 0, TextStyle::default());
        assert_eq!(fonts.faces.len(), 1);
        let font = fonts.get(0).font();
        assert_eq!(font.offset, HEADER);
        assert_eq!(font.data.len(), collection.len());
        assert!(supports_renderer(font));
        assert_ne!(font.charmap().map('M'), 0);
        assert!(
            font.glyph_metrics(&[])
                .advance_width(font.charmap().map('M'))
                > 0.0
        );
    }
}
