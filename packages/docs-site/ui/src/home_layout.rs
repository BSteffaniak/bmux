//! Styling helpers for the bmux docs home page.

use hyperchad::color::Color;
use hyperchad::template::{Containers, container};

/// Terminal green accent color (#7ee787 — can't use hex literal because `e` parses as exponent).
pub fn green() -> Color {
    Color::from_hex("#7ee787")
}

/// Light text color.
pub fn text_primary() -> Color {
    Color::from_hex("#f0f6fc")
}

/// Muted text color.
pub fn text_secondary() -> Color {
    Color::from_hex("#c9d1d9")
}

/// Muted/dim text color.
pub fn text_muted() -> Color {
    Color::from_hex("#8b949e")
}

/// Dark surface background.
pub fn surface() -> Color {
    Color::from_hex("#161b22")
}

/// Monospace font stack.
pub const MONO_FONT: &str = "'SF Mono', 'Cascadia Code', 'Fira Code', Menlo, Consolas, monospace";

/// Wrap the custom home content in the bmux landing-page shell.
#[must_use]
pub fn page(content: &Containers) -> Containers {
    container! {
        div direction=column min-height="100vh" background=#0d1117 color=#f0f6fc {
            main padding-x=24 padding-y=48 align-items=center {
                div max-width=1100 width=100% direction=column gap=48 {
                    (content)
                }
            }
        }
    }
}
