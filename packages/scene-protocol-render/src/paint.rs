//! Helpers for lowering scene-protocol paint commands: style interpolation for gradients, and
//! fixed-width row text. The lowering itself, into the host's render items, lives with the
//! renderer that owns it (the decoration renderer); nothing writes paint commands to a terminal
//! directly.

use bmux_scene_protocol::scene_protocol::Style;

/// Truncate or pad `content` to exactly `width` columns. Used by
/// renderer call sites that paint fixed-width status bars or badges.
#[must_use]
pub fn opaque_row_text(content: &str, width: usize) -> String {
    let mut rendered = content.to_string();
    if rendered.len() > width {
        rendered.truncate(width);
    }
    if rendered.len() < width {
        rendered.push_str(&" ".repeat(width - rendered.len()));
    }
    rendered
}

/// Interpolate between two scene styles. Boolean flags are taken from
/// whichever style is "closer" at `t` (`< 0.5` → `from`, `>= 0.5` → `to`).
/// Colour interpolation works for paired truecolor endpoints; every
/// other pair falls back to the nearest endpoint.
#[must_use]
pub fn interpolate_style(from: &Style, to: &Style, t: f32) -> Style {
    let pick = |a, b| if t < 0.5 { a } else { b };
    Style {
        fg: interpolate_color(from.fg.as_ref(), to.fg.as_ref(), t),
        bg: interpolate_color(from.bg.as_ref(), to.bg.as_ref(), t),
        bold: pick(from.bold, to.bold),
        underline: pick(from.underline, to.underline),
        italic: pick(from.italic, to.italic),
        reverse: pick(from.reverse, to.reverse),
        dim: pick(from.dim, to.dim),
        blink: pick(from.blink, to.blink),
        strikethrough: pick(from.strikethrough, to.strikethrough),
    }
}

fn interpolate_color(
    from: Option<&bmux_scene_protocol::scene_protocol::Color>,
    to: Option<&bmux_scene_protocol::scene_protocol::Color>,
    t: f32,
) -> Option<bmux_scene_protocol::scene_protocol::Color> {
    use bmux_scene_protocol::scene_protocol::Color;
    match (from, to) {
        (
            Some(Color::Rgb {
                r: r0,
                g: g0,
                b: b0,
            }),
            Some(Color::Rgb {
                r: r1,
                g: g1,
                b: b1,
            }),
        ) => Some(Color::Rgb {
            r: lerp_u8(*r0, *r1, t),
            g: lerp_u8(*g0, *g1, t),
            b: lerp_u8(*b0, *b1, t),
        }),
        (a, b) => {
            if t < 0.5 {
                a.cloned()
            } else {
                b.cloned()
            }
        }
    }
}

#[allow(
    clippy::suboptimal_flops,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)] // Colour channel lerp; fma and cast warnings are noise for u8 output.
fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    let a = f32::from(a);
    let b = f32::from(b);
    let v = a + (b - a) * t.clamp(0.0, 1.0);
    v.round().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_scene_protocol::scene_protocol::Color;

    fn default_style() -> Style {
        Style {
            fg: None,
            bg: None,
            bold: false,
            underline: false,
            italic: false,
            reverse: false,
            dim: false,
            blink: false,
            strikethrough: false,
        }
    }

    #[test]
    fn opaque_row_text_truncates_and_pads() {
        assert_eq!(opaque_row_text("hello", 3), "hel");
        assert_eq!(opaque_row_text("hi", 5), "hi   ");
        assert_eq!(opaque_row_text("", 4), "    ");
    }

    #[test]
    fn interpolate_style_blends_truecolor_and_picks_flags_by_side() {
        let from = Style {
            fg: Some(Color::Rgb { r: 0, g: 0, b: 0 }),
            ..default_style()
        };
        let to = Style {
            fg: Some(Color::Rgb {
                r: 200,
                g: 100,
                b: 50,
            }),
            bold: true,
            ..default_style()
        };
        let middle = interpolate_style(&from, &to, 0.5);
        assert_eq!(
            middle.fg,
            Some(Color::Rgb {
                r: 100,
                g: 50,
                b: 25
            })
        );
        assert!(middle.bold);
        assert!(!interpolate_style(&from, &to, 0.25).bold);
    }
}
