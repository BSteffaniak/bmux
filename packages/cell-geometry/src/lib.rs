//! BMUX terminal cell geometry. Input to `grapheme_width` is one extended
//! grapheme cluster, not a byte, Unicode scalar, or shaped run.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Width of a printable scalar arriving at the VT parser before extension.
/// VT controls are interpreted by the parser, not painted as replacements.
#[must_use]
pub fn scalar_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0).min(2)
}

/// Cells painted for a single grapheme. A standalone zero-width cluster
/// paints nothing; a control cluster in a literal text label paints one
/// replacement. Tabs, VT controls and line breaks are handled by callers
/// before measurement when they have structural meaning.
#[must_use]
pub fn grapheme_width(grapheme: &str) -> usize {
    if grapheme.len() == 1 || grapheme.chars().any(char::is_control) {
        return 1;
    }
    UnicodeWidthStr::width(grapheme).min(2)
}

/// Width of literal, single-line text as painted one grapheme at a time.
/// Whole-string shaping (e.g. Arabic lam-alef) must not collapse two cells.
#[must_use]
pub fn display_width(text: &str) -> usize {
    if text.bytes().all(|byte| matches!(byte, b' '..=b'~')) {
        return text.len();
    }
    text.graphemes(true).map(grapheme_width).sum()
}
