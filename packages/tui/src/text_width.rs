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
    if display_width(text) <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_owned();
    }

    let mut output = String::new();
    let mut used = 0usize;
    let body_width = width.saturating_sub(1);
    for grapheme in text.graphemes(true) {
        let grapheme_width = display_width(grapheme);
        if used.saturating_add(grapheme_width) > body_width {
            output.push('…');
            return output;
        }
        output.push_str(grapheme);
        used = used.saturating_add(grapheme_width);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{
        display_width, truncate_to_display_width, wrap_text_with_continuation,
        wrap_text_with_continuation_character,
    };

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
