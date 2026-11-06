//! Unicode-aware terminal text measurement and wrapping helpers.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Return the display width of a string in terminal cells.
#[must_use]
pub fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Wrap text at word boundaries with a distinct first-line width.
///
/// Words longer than the target width fall back to grapheme wrapping, and
/// continuation rows never begin with wrapped whitespace.
///
/// Use [`wrap_text_with_continuation_character`] when grapheme-boundary wrapping
/// is required, such as for column-significant content.
#[must_use]
pub fn wrap_text_with_continuation(
    text: &str,
    first_width: usize,
    continuation_width: usize,
) -> Vec<String> {
    crate::text::wrap_text(
        text,
        crate::text::TextWrapGeometry::with_continuation(first_width, continuation_width),
        crate::text::TextWrap::Word,
    )
}

/// Wrap text at grapheme boundaries with a distinct first-line width.
///
/// Prefer [`wrap_text_with_continuation`] for prose. This variant exists for
/// column-significant content where word boundaries must be ignored.
#[must_use]
pub fn wrap_text_with_continuation_character(
    text: &str,
    first_width: usize,
    continuation_width: usize,
) -> Vec<String> {
    crate::text::wrap_text(
        text,
        crate::text::TextWrapGeometry::with_continuation(first_width, continuation_width),
        crate::text::TextWrap::Character,
    )
}

/// Truncate text to a terminal display width, appending an ellipsis when clipped.
#[must_use]
pub fn truncate_to_display_width(text: &str, width: usize) -> String {
    // Printable ASCII is one narrow grapheme per byte.
    if text.bytes().all(|byte| matches!(byte, b' '..=b'~')) {
        return match width {
            _ if text.len() <= width => text.to_owned(),
            0 => String::new(),
            1 => "…".to_owned(),
            _ => format!("{}…", &text[..width - 1]),
        };
    }
    // Measure only until the text overflows, so clipping long text costs
    // O(width) rather than O(text).
    let body_width = width.saturating_sub(1);
    let mut used = 0usize;
    let mut body_end = None;
    for (index, grapheme) in text.grapheme_indices(true) {
        let grapheme_width = display_width(grapheme);
        if body_end.is_none() && used.saturating_add(grapheme_width) > body_width {
            body_end = Some(index);
        }
        used = used.saturating_add(grapheme_width);
        if used > width {
            return match (width, body_end) {
                (0, _) => String::new(),
                (1, _) | (_, None) => "…".to_owned(),
                (_, Some(end)) => format!("{}…", &text[..end]),
            };
        }
    }
    text.to_owned()
}

#[cfg(test)]
mod tests {
    use super::{
        display_width, truncate_to_display_width, wrap_text_with_continuation,
        wrap_text_with_continuation_character,
    };

    /// The measure-everything implementation the early-exit one replaced.
    fn reference_truncate(text: &str, width: usize) -> String {
        use unicode_segmentation::UnicodeSegmentation;
        if text.graphemes(true).map(display_width).sum::<usize>() <= width {
            return text.to_owned();
        }
        if width <= 1 {
            return "…".repeat(width);
        }
        let mut output = String::new();
        let mut used = 0;
        for grapheme in text.graphemes(true) {
            let grapheme_width = display_width(grapheme);
            if used + grapheme_width > width - 1 {
                output.push('…');
                return output;
            }
            output.push_str(grapheme);
            used += grapheme_width;
        }
        output
    }

    #[test]
    fn truncation_matches_full_measurement_for_ascii_and_unicode() {
        let texts = [
            "",
            "a",
            "hello world",
            "日本語テキスト",
            "a界b界c",
            "e\u{301}e\u{301}e\u{301}x",
            "👨‍👩‍👧‍👦 family",
            "tab\there",
            "zero\u{200b}width",
        ];
        for text in texts {
            for width in 0..16 {
                assert_eq!(
                    truncate_to_display_width(text, width),
                    reference_truncate(text, width),
                    "text={text:?} width={width}"
                );
            }
        }
    }

    #[test]
    fn truncating_long_text_keeps_only_the_visible_prefix() {
        let long = "x".repeat(100_000);
        assert_eq!(truncate_to_display_width(&long, 4), "xxx…");
        let wide = "界".repeat(100_000);
        assert_eq!(truncate_to_display_width(&wide, 5), "界界…");
    }

    #[test]
    fn wraps_combining_graphemes_without_splitting_marks() {
        let rows = wrap_text_with_continuation_character("e\u{301}e\u{301}e\u{301}", 2, 2);

        assert_eq!(rows, vec!["e\u{301}e\u{301}", "e\u{301}"]);
    }

    #[test]
    fn wraps_prose_at_word_boundaries_by_default() {
        // Wrapping preserves the break whitespace; trimming is a separate,
        // opt-in policy owned by the rendering widget.
        let rows = wrap_text_with_continuation("alpha beta gamma", 11, 11);

        assert_eq!(rows, vec!["alpha beta ", "gamma"]);
    }

    #[test]
    fn wraps_word_longer_than_width_at_graphemes() {
        let rows = wrap_text_with_continuation("supercalifragilistic", 8, 8);

        assert_eq!(rows, vec!["supercal", "ifragili", "stic"]);
    }

    #[test]
    fn honors_distinct_first_row_width() {
        let rows = wrap_text_with_continuation("alpha beta gamma delta", 12, 6);

        assert_eq!(rows, vec!["alpha beta ", "gamma ", "delta"]);
    }

    #[test]
    fn never_starts_a_continuation_row_with_wrapped_whitespace() {
        let rows = wrap_text_with_continuation("alpha      beta", 6, 6);

        assert!(
            rows.iter().skip(1).all(|row| !row.starts_with(' ')),
            "continuation rows must not begin with wrapped whitespace: {rows:?}"
        );
    }

    #[test]
    fn measures_emoji_sequence_as_a_grapheme() {
        assert_eq!(display_width("👨‍👩‍👧‍👦"), 2);
    }

    #[test]
    fn truncates_at_grapheme_boundaries() {
        assert_eq!(truncate_to_display_width("ab👨‍👩‍👧‍👦cd", 5), "ab👨‍👩‍👧‍👦…");
    }
}
