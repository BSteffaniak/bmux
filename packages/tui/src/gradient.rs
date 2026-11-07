//! Perceptual color gradients for borders and text.
//!
//! Gradients interpolate in OKLab through [`bmux_color`], so midpoints stay
//! even and vivid. They only replace foreground colors; backgrounds and
//! modifiers of the base style are preserved, and text measurement is
//! unaffected because coloring never changes graphemes.

use bmux_color::{Rgb, mix};
use unicode_segmentation::UnicodeSegmentation;

use crate::style::{Color, Style};
use crate::text::{Line, Span};

/// Largest phase value; a full animation cycle spans `0..PHASE_CYCLE`.
pub const PHASE_CYCLE: u16 = 2000;

/// Two-stop foreground gradient with an animatable phase.
///
/// With phase `0`, positions run linearly from `from` to `to`. A non-zero
/// phase shifts every position along a seamless `from → to → from` cycle of
/// length [`PHASE_CYCLE`], so advancing the phase each tick produces a
/// continuous sweep without a visible seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ColorGradient {
    /// Color at the start position.
    pub from: Rgb,
    /// Color at the end position.
    pub to: Rgb,
    /// Shift along the cycle, in thousandths of the start-to-end distance.
    pub phase: u16,
}

impl ColorGradient {
    /// Create a gradient from `from` to `to` with phase `0`.
    #[must_use]
    pub const fn new(from: Rgb, to: Rgb) -> Self {
        Self { from, to, phase: 0 }
    }

    /// Return this gradient shifted to `phase`, wrapped into `0..PHASE_CYCLE`.
    #[must_use]
    pub const fn with_phase(mut self, phase: u16) -> Self {
        self.phase = phase % PHASE_CYCLE;
        self
    }

    /// Sample at `position` of `span` (`position` clamped to `0..=span`).
    ///
    /// A zero `span` samples the start of the cycle.
    #[must_use]
    pub fn sample(self, position: u64, span: u64) -> Rgb {
        let position = position.min(span);
        // Fixed-point thousandths keep sampling deterministic across platforms.
        let base = position.saturating_mul(1000).checked_div(span).unwrap_or(0);
        let cycle = u64::from(PHASE_CYCLE);
        let offset = (base + u64::from(self.phase)) % cycle;
        let along = if offset <= 1000 {
            offset
        } else {
            cycle - offset
        };
        #[allow(clippy::cast_precision_loss)] // `along` is at most 1000.
        let weight = along as f64 / 1000.0;
        mix(self.to, self.from, weight)
    }

    /// Return `base` with its foreground replaced by the sample at `position`.
    #[must_use]
    pub fn style_at(self, base: Style, position: u64, span: u64) -> Style {
        base.fg(Color::from(self.sample(position, span)))
    }
}

/// Color `text` grapheme by grapheme across `gradient`, over `base`.
///
/// Each grapheme becomes one span, so the line measures exactly like the
/// unstyled text. Empty text yields an empty line.
#[must_use]
pub fn gradient_line(text: &str, gradient: ColorGradient, base: Style) -> Line {
    let graphemes: Vec<&str> = text.graphemes(true).collect();
    let span = u64::try_from(graphemes.len().saturating_sub(1)).unwrap_or(u64::MAX);
    Line::from_spans(
        graphemes
            .into_iter()
            .enumerate()
            .map(|(index, grapheme)| {
                let position = u64::try_from(index).unwrap_or(u64::MAX);
                Span::styled(grapheme, gradient.style_at(base, position, span))
            })
            .collect::<Vec<_>>(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::Modifier;
    use crate::text_width::display_width;

    const FROM: Rgb = Rgb::new(0xff, 0x00, 0xaa);
    const TO: Rgb = Rgb::new(0x00, 0xff, 0xff);

    #[test]
    fn unshifted_gradient_runs_from_start_to_end() {
        let gradient = ColorGradient::new(FROM, TO);
        assert_eq!(gradient.sample(0, 10), FROM);
        assert_eq!(gradient.sample(10, 10), TO);
        assert_eq!(gradient.sample(99, 10), TO);
        assert_eq!(gradient.sample(0, 0), FROM);
    }

    #[test]
    fn phase_cycles_seamlessly() {
        let gradient = ColorGradient::new(FROM, TO);
        assert_eq!(gradient.with_phase(1000).sample(0, 10), TO);
        assert_eq!(gradient.with_phase(PHASE_CYCLE).sample(0, 10), FROM);
        assert_eq!(
            gradient.with_phase(1999).sample(0, 1000),
            gradient.with_phase(1).sample(0, 1000)
        );
        // Shifting by one step equals sampling one step further along.
        assert_eq!(
            gradient.with_phase(100).sample(0, 10),
            gradient.sample(1, 10)
        );
    }

    #[test]
    fn gradient_line_preserves_text_measurement_and_base_attributes() {
        let base = Style::new().bg(Color::Black).add_modifier(Modifier::BOLD);
        for text in ["bcode", "界面 ok", "e\u{301}🙂x", ""] {
            let line = gradient_line(text, ColorGradient::new(FROM, TO), base);
            let joined: String = line
                .spans
                .iter()
                .map(|span| span.content.as_str())
                .collect();
            assert_eq!(joined, text);
            assert_eq!(display_width(&joined), display_width(text));
            for span in &line.spans {
                assert_eq!(span.style.bg, Some(Color::Black));
                assert!(span.style.modifiers.contains(Modifier::BOLD));
            }
        }
        let line = gradient_line("ab", ColorGradient::new(FROM, TO), base);
        assert_eq!(line.spans[0].style.fg, Some(Color::from(FROM)));
        assert_eq!(line.spans[1].style.fg, Some(Color::from(TO)));
        assert_eq!(
            gradient_line("e\u{301}x", ColorGradient::new(FROM, TO), base)
                .spans
                .len(),
            2
        );
    }
}
