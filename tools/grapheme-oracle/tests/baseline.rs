// Run the identical public-API assertions against the dependency being replaced.
extern crate self as bmux_unicode_segmentation;
use unicode_segmentation::{GraphemeIndices, Graphemes, UnicodeSegmentation};

pub fn graphemes(text: &str) -> Graphemes<'_> {
    text.graphemes(true)
}
pub fn grapheme_indices(text: &str) -> GraphemeIndices<'_> {
    text.grapheme_indices(true)
}

#[path = "../../../packages/unicode-segmentation/tests/conformance.rs"]
mod conformance;
