use ab_glyph::{FontArc, FontVec};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontPreset {
    GhosttyNerd,
    SystemMonospace,
}

#[derive(Debug, Clone, Copy)]
pub enum FontStyle {
    Regular,
    Bold,
    Italic,
    BoldItalic,
}

#[derive(Debug, Clone, Copy)]
pub struct EmbeddedFont {
    pub family: &'static str,
    pub style: FontStyle,
    pub data: &'static [u8],
}

pub fn default_families_for_preset(preset: FontPreset) -> Vec<String> {
    match preset {
        FontPreset::GhosttyNerd => vec![
            "JetBrainsMono Nerd Font".to_string(),
            "JetBrains Mono".to_string(),
            "JetBrainsMono NF".to_string(),
            "Symbols Nerd Font Mono".to_string(),
            "SF Mono".to_string(),
            "Menlo".to_string(),
            "Monaco".to_string(),
            "DejaVu Sans Mono".to_string(),
            "Liberation Mono".to_string(),
        ],
        FontPreset::SystemMonospace => vec![
            "SF Mono".to_string(),
            "Menlo".to_string(),
            "Monaco".to_string(),
            "DejaVu Sans Mono".to_string(),
            "Liberation Mono".to_string(),
        ],
    }
}

pub fn register_preset_fonts(db: &mut fontdb::Database, preset: FontPreset) -> usize {
    let mut count = 0usize;
    for font in bundled_fonts_for_preset(preset) {
        db.load_font_data(font.data.to_vec());
        count = count.saturating_add(1);
    }
    count
}

pub fn load_preset_fonts_for_ab_glyph(preset: FontPreset) -> Vec<FontArc> {
    bundled_fonts_for_preset(preset)
        .iter()
        .filter_map(|font| FontVec::try_from_vec(font.data.to_vec()).ok())
        .map(FontArc::new)
        .collect()
}

pub fn bundled_fonts_for_preset(preset: FontPreset) -> &'static [EmbeddedFont] {
    match preset {
        FontPreset::GhosttyNerd => ghostty_nerd_fonts(),
        FontPreset::SystemMonospace => &[],
    }
}

#[cfg(feature = "bundled-ghostty-fonts")]
fn ghostty_nerd_fonts() -> &'static [EmbeddedFont] {
    &[
        EmbeddedFont {
            family: "JetBrainsMono Nerd Font",
            style: FontStyle::Regular,
            data: include_bytes!("../assets/ghostty/JetBrainsMonoNerdFont-Regular.ttf"),
        },
        EmbeddedFont {
            family: "JetBrainsMono Nerd Font",
            style: FontStyle::Bold,
            data: include_bytes!("../assets/ghostty/JetBrainsMonoNerdFont-Bold.ttf"),
        },
        EmbeddedFont {
            family: "JetBrainsMono Nerd Font",
            style: FontStyle::Italic,
            data: include_bytes!("../assets/ghostty/JetBrainsMonoNerdFont-Italic.ttf"),
        },
        EmbeddedFont {
            family: "JetBrainsMono Nerd Font",
            style: FontStyle::BoldItalic,
            data: include_bytes!("../assets/ghostty/JetBrainsMonoNerdFont-BoldItalic.ttf"),
        },
    ]
}

#[cfg(not(feature = "bundled-ghostty-fonts"))]
fn ghostty_nerd_fonts() -> &'static [EmbeddedFont] {
    &[]
}
