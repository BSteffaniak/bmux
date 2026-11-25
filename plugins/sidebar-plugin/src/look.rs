//! How the sidebar looks: its colors, taken from the runtime's theme, each
//! workspace's color, and the working spinner.
//!
//! Surfaces (cards, header bands) are blends of the theme's background toward
//! its foreground or accent, so they read the same on light and dark themes.
//! Colors that carry meaning (states, workspaces) are the terminal's own
//! sixteen colors, which the theme already defines. Without a theme the
//! sidebar uses the terminal's own default colors and tints nothing.

use bmux_color::{Rgb, mix};
use bmux_plugin::{RenderColor, RenderNamedColor, RenderStyle};

/// The sidebar's colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// What everything is painted on: the terminal's own background.
    pub base: RenderStyle,
    /// Body text.
    pub text: RenderStyle,
    /// Names: the shown tab's and workspace headers'.
    pub strong: RenderStyle,
    /// Secondary text: directories, counts.
    pub muted: RenderStyle,
    /// Tertiary text: separators, chevrons.
    pub faint: RenderStyle,
    /// The theme's accent (its cursor color).
    pub accent: RenderColor,
    /// The shown tab's card background.
    pub active_card: RenderStyle,
    /// A hovered row's background.
    pub hover_card: RenderStyle,
    /// A workspace header's band.
    pub band: RenderStyle,
    /// A hovered workspace header's band.
    pub band_hover: RenderStyle,
    /// Text on a solid state pill: the theme's background.
    pub on_pill: RenderColor,
}

impl Palette {
    /// The palette of `appearance`. Without a theme (the runtime's unthemed
    /// default appearance) or when its colors do not parse, the terminal's
    /// own colors: the host terminal's foreground and background are not
    /// known, so nothing is blended from a guess at them.
    #[must_use]
    pub fn from_appearance(appearance: &bmux_appearance::RuntimeAppearance) -> Self {
        let unthemed = bmux_appearance::RuntimeAppearance::default();
        if appearance.foreground == unthemed.foreground
            && appearance.background == unthemed.background
        {
            return Self::default();
        }
        let (Ok(fg), Ok(bg)) = (
            appearance.foreground.parse::<Rgb>(),
            appearance.background.parse::<Rgb>(),
        ) else {
            return Self::default();
        };
        let accent = appearance.cursor.parse::<Rgb>().unwrap_or(fg);
        Self::from_colors(fg, bg, accent)
    }

    /// The palette blended from a theme's foreground, background and accent.
    #[must_use]
    pub fn from_colors(fg: Rgb, bg: Rgb, accent: Rgb) -> Self {
        let rgb = |color: Rgb| RenderColor::Rgb {
            r: color.r,
            g: color.g,
            b: color.b,
        };
        let surface = |color: Rgb| RenderStyle::new().background(rgb(color));
        let text = |color: Rgb| RenderStyle::new().foreground(rgb(color));
        Self {
            base: RenderStyle::new(),
            text: text(fg),
            strong: text(fg).bold(),
            muted: text(mix(fg, bg, 0.58)),
            faint: text(mix(fg, bg, 0.36)),
            accent: rgb(accent),
            active_card: surface(mix(accent, bg, 0.16)),
            hover_card: surface(mix(fg, bg, 0.06)),
            band: surface(mix(fg, bg, 0.10)),
            band_hover: surface(mix(fg, bg, 0.15)),
            on_pill: rgb(bg),
        }
    }
}

/// The terminal's own colors, for any background: text in its default
/// foreground and named colors, no tinted surfaces (the shown tab keeps its
/// bar and bold name), and hovered rows and header bands in reverse video.
impl Default for Palette {
    fn default() -> Self {
        Self {
            base: RenderStyle::new(),
            text: RenderStyle::new(),
            strong: RenderStyle::new().bold(),
            muted: RenderStyle::new().named_foreground(RenderNamedColor::BrightBlack),
            faint: RenderStyle::new().named_foreground(RenderNamedColor::BrightBlack),
            accent: RenderColor::Named(RenderNamedColor::Blue),
            active_card: RenderStyle::new(),
            hover_card: RenderStyle::new().reverse(),
            band: RenderStyle::new(),
            band_hover: RenderStyle::new().reverse(),
            on_pill: RenderColor::Named(RenderNamedColor::Black),
        }
    }
}

/// Colors a workspace may be given automatically: the terminal's hues that do
/// not already mean a state (yellow, red and green do).
const WORKSPACE_HUES: [RenderNamedColor; 6] = [
    RenderNamedColor::Blue,
    RenderNamedColor::Magenta,
    RenderNamedColor::Cyan,
    RenderNamedColor::BrightMagenta,
    RenderNamedColor::BrightBlue,
    RenderNamedColor::BrightCyan,
];

/// The color of the workspace named `name`: the one configured for it, or
/// one picked from its name, the same on every machine and every start.
#[must_use]
pub fn workspace_color(
    name: &str,
    configured: &std::collections::BTreeMap<String, RenderColor>,
) -> RenderColor {
    if let Some(color) = configured.get(name) {
        return *color;
    }
    // FNV-1a: stable across builds, unlike the standard hasher.
    let hash = name.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    let index = usize::try_from(hash % WORKSPACE_HUES.len() as u64).unwrap_or(0);
    RenderColor::Named(WORKSPACE_HUES[index])
}

/// Parse a configured color: one of the terminal's sixteen names
/// (`blue`, `bright-magenta`, ...) or `#rrggbb`.
///
/// # Errors
/// Returns a message when `value` is neither.
pub fn parse_color(value: &str) -> Result<RenderColor, String> {
    if value.starts_with('#') {
        return value
            .parse::<Rgb>()
            .map(|rgb| RenderColor::Rgb {
                r: rgb.r,
                g: rgb.g,
                b: rgb.b,
            })
            .map_err(|_| format!("{value:?} is not a #rrggbb color"));
    }
    let normalized = value.to_ascii_lowercase().replace(['_', ' '], "-");
    let (bright, base) = normalized
        .strip_prefix("bright-")
        .map_or((false, normalized.as_str()), |base| (true, base));
    let named = match (bright, base) {
        (false, "black") => RenderNamedColor::Black,
        (false, "red") => RenderNamedColor::Red,
        (false, "green") => RenderNamedColor::Green,
        (false, "yellow") => RenderNamedColor::Yellow,
        (false, "blue") => RenderNamedColor::Blue,
        (false, "magenta") => RenderNamedColor::Magenta,
        (false, "cyan") => RenderNamedColor::Cyan,
        (false, "white") => RenderNamedColor::White,
        (true, "black") => RenderNamedColor::BrightBlack,
        (true, "red") => RenderNamedColor::BrightRed,
        (true, "green") => RenderNamedColor::BrightGreen,
        (true, "yellow") => RenderNamedColor::BrightYellow,
        (true, "blue") => RenderNamedColor::BrightBlue,
        (true, "magenta") => RenderNamedColor::BrightMagenta,
        (true, "cyan") => RenderNamedColor::BrightCyan,
        (true, "white") => RenderNamedColor::BrightWhite,
        _ => {
            return Err(format!(
                "{value:?} is not a terminal color name (like blue or bright-magenta) or #rrggbb"
            ));
        }
    };
    Ok(RenderColor::Named(named))
}

/// The spinner's frames, shown [`SPINNER_FRAME_MS`] apart.
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
/// How long each spinner frame shows.
pub const SPINNER_FRAME_MS: u64 = 100;

/// The working spinner's frame at `now_ms` on this client's clock. Every
/// working tab shows the same frame.
#[must_use]
pub fn spinner_frame(now_ms: u64) -> char {
    let index = usize::try_from((now_ms / SPINNER_FRAME_MS) % SPINNER.len() as u64).unwrap_or(0);
    SPINNER[index]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_colors_are_stable_and_never_a_state_color() {
        let none = std::collections::BTreeMap::new();
        let first = workspace_color("bmux", &none);
        assert_eq!(first, workspace_color("bmux", &none));
        for name in ["a", "bcode", "notes", "日本語", ""] {
            let RenderColor::Named(named) = workspace_color(name, &none) else {
                panic!("named");
            };
            assert!(WORKSPACE_HUES.contains(&named));
        }
        let configured =
            std::collections::BTreeMap::from([("bmux".to_string(), parse_color("red").unwrap())]);
        assert_eq!(
            workspace_color("bmux", &configured),
            RenderColor::Named(RenderNamedColor::Red)
        );
    }

    #[test]
    fn colors_parse_by_name_or_hex() {
        assert_eq!(
            parse_color("Bright_Magenta"),
            Ok(RenderColor::Named(RenderNamedColor::BrightMagenta))
        );
        assert_eq!(
            parse_color("#ff8800"),
            Ok(RenderColor::Rgb {
                r: 0xff,
                g: 0x88,
                b: 0
            })
        );
        assert!(parse_color("teal").is_err());
        assert!(parse_color("#12").is_err());
    }

    /// Surfaces stay close to the background and readable text stays far from
    /// it, on dark and light themes alike.
    #[test]
    fn the_palette_follows_light_and_dark_themes() {
        let luminance = |style: RenderStyle, background: bool| {
            let color = if background { style.bg } else { style.fg };
            let Some(RenderColor::Rgb { r, g, b }) = color else {
                panic!("rgb");
            };
            Rgb::new(r, g, b)
        };
        for (fg, bg) in [("#c0caf5", "#1a1b26"), ("#1f2328", "#ffffff")] {
            let fg = fg.parse::<Rgb>().unwrap();
            let bg = bg.parse::<Rgb>().unwrap();
            let palette = Palette::from_colors(fg, bg, "#7aa2f7".parse().unwrap());
            for surface in [palette.active_card, palette.hover_card, palette.band] {
                let surface = luminance(surface, true);
                assert!(surface.contrast_ratio(bg) < 1.6, "surface near background");
                assert!(fg.contrast_ratio(surface) > 4.5, "text readable on it");
            }
            assert!(luminance(palette.muted, false).contrast_ratio(bg) > 3.0);
        }
    }

    /// Without a theme, nothing assumes a dark terminal: no fixed text color
    /// or tinted surface, so light terminals get no black card.
    #[test]
    fn without_a_theme_the_palette_uses_the_terminals_own_colors() {
        let unthemed = Palette::from_appearance(&bmux_appearance::RuntimeAppearance::default());
        assert_eq!(unthemed, Palette::default());
        for surface in [
            unthemed.active_card,
            unthemed.hover_card,
            unthemed.band,
            unthemed.band_hover,
        ] {
            assert_eq!(surface.bg, None);
        }
        assert_eq!(unthemed.text.fg, None);
        assert_ne!(unthemed.hover_card, unthemed.base, "hover still shows");
        let themed = Palette::from_appearance(&bmux_appearance::RuntimeAppearance {
            foreground: "#1f2328".into(),
            background: "#ffffff".into(),
            ..bmux_appearance::RuntimeAppearance::default()
        });
        assert!(themed.active_card.bg.is_some());
    }
}
