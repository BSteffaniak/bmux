//! BMUX terminal cell geometry. Input to `grapheme_width` is one extended
//! grapheme cluster, not a byte, Unicode scalar, or shaped run.

use unicode_segmentation::UnicodeSegmentation;
mod width_data;

#[inline]
fn class(ch: char) -> usize {
    let cp = ch as usize;
    let page = usize::from(width_data::PAGE[cp >> 8]);
    usize::from(width_data::CLASS[page][cp & 255])
}

/// Unicode narrow string width before grapheme splitting or cell clipping.
/// Some sequences retroactively narrow the suffix (e.g. Khmer coeng).
/// Accumulate signed adjustments before applying the terminal cell cap.
#[must_use]
pub fn sequence_width(text: &str) -> usize {
    let mut state = 0;
    let mut cells = 0usize;
    for ch in text.chars().rev() {
        let (delta, next) = width_data::STEP[state][class(ch)];
        cells = cells.wrapping_add_signed(isize::from(delta));
        state = usize::from(next);
    }
    cells
}

/// Width of a printable scalar arriving at the VT parser before extension.
/// VT controls are interpreted by the parser, not painted as replacements.
#[must_use]
pub fn scalar_width(ch: char) -> usize {
    unicode_scalar_width(ch).unwrap_or(0).min(2)
}

/// Unicode narrow scalar width before cell clipping; controls have no intrinsic width.
#[must_use]
pub fn unicode_scalar_width(ch: char) -> Option<usize> {
    (!ch.is_control()).then(|| usize::from(width_data::STEP[0][class(ch)].0.unsigned_abs()))
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
    sequence_width(grapheme).min(2)
}

/// Width of literal, single-line text as painted one grapheme at a time.
/// Whole-string shaping (e.g. Arabic lam-alef) must not collapse two cells.
#[must_use]
pub fn display_width(text: &str) -> usize {
    if text.bytes().all(|byte| matches!(byte, b' '..=b'~')) {
        return text.len();
    }
    graphemes(text).map(grapheme_width).sum()
}

/// Extended graphemes in the order in which a terminal row paints them.
pub fn graphemes(text: &str) -> impl DoubleEndedIterator<Item = &str> + Clone {
    text.graphemes(true)
}

#[cfg(test)]
mod compatibility;
