use unicode_segmentation::UnicodeSegmentation;
fn graphemes(s: &str) -> impl DoubleEndedIterator<Item = &str> {
    s.graphemes(true)
}
fn grapheme_indices(s: &str) -> impl DoubleEndedIterator<Item = (usize, &str)> {
    s.grapheme_indices(true)
}
include!("../bench.rs");
